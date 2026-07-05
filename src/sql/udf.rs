//! User-defined functions (`CREATE FUNCTION` / `DROP FUNCTION`).
//!
//! Two languages are supported, matching what PostgreSQL calls "SQL-language"
//! and "PL/pgSQL" functions:
//!
//! * `LANGUAGE SQL` — the body is one or more `;`-separated plain SQL
//!   statements (`SELECT`/`INSERT`/`UPDATE`/`DELETE`); arguments are bound
//!   both positionally (`$1`, `$2`, ... exactly like a prepared statement)
//!   and by the declared parameter name, matching PostgreSQL (a SQL-language
//!   function may reference its arguments either way). All statements
//!   execute in order; the last statement's result becomes the function's
//!   result (its first row's first column, or `NULL` if it produced no
//!   rows) — this matches PostgreSQL.
//! * `LANGUAGE plpgsql` — a small, explicitly-bounded subset: `DECLARE`d
//!   locals with optional defaults, `:=` assignment, `IF`/`ELSIF`/`ELSE`/
//!   `END IF`, `RETURN [expr]`, `RAISE [NOTICE|WARNING|EXCEPTION] 'msg'[,
//!   args]`, and plain SQL statements. Arguments and locals are referenced by
//!   name (not `$n`). Anything outside this subset (loops, `EXCEPTION`
//!   handlers, cursors, dynamic `EXECUTE`, nested blocks, `OUT`/`INOUT`/
//!   `VARIADIC` parameters, `RETURNS TABLE`/`SETOF`) is rejected with a typed
//!   `0A000` naming the construct — see `docs/postgres-compat.md`.
//!
//! sqlparser has no PL/pgSQL grammar (a function body is opaque `$$...$$`
//! text handed to the language handler, exactly as in real PostgreSQL), so
//! [`parse_plpgsql`] is a small hand-written recursive-descent parser built
//! on top of sqlparser's tokenizer/expression parser (reused for every
//! embedded SQL statement and every expression, so `IF n > 0 THEN` and
//! `x := a + b` parse and evaluate with the exact same grammar as top-level
//! SQL).
//!
//! **Deliberate divergence from PostgreSQL**: real PostgreSQL does not
//! validate a PL/pgSQL body's structure at `CREATE FUNCTION` time (without
//! the separate `plpgsql_check` extension) — a function with a typo or an
//! unsupported construct is only discovered when it is first called. This
//! repo's truthfulness contract requires every construct to either work or
//! fail typed *immediately*, so unsupported constructs and structural
//! problems (e.g. control reaching the end of the function without a
//! `RETURN`) are rejected at `CREATE FUNCTION` (DDL) time instead — a broken
//! function can never silently exist in the catalog.
//!
//! Bodies are stored as raw source text (`prosrc`) and re-parsed on every
//! call, the same pattern row-security policy expressions already follow
//! (see [`crate::sql::rls`]).

use crate::relational::catalog::{
    DropFunctionByName, FunctionArgDef, FunctionDef, FunctionLanguage, FunctionVolatility,
};
use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError, unsupported};
use crate::sql::exec::Exec;
use crate::sql::names::{ident_name, split_schema_table};
use crate::sql::result::ExecResult;
use sqlparser::ast::{
    ArgMode, CreateFunction, CreateFunctionBody, DataType, DropFunction, Expr, Function,
    FunctionArg, FunctionArgExpr, FunctionArguments, FunctionBehavior, FunctionCalledOnNull,
    FunctionReturnType, Ident, OnConflictAction, OnInsert, OperateFunctionArg, Query, Select,
    SelectItem, SetExpr, Statement, TableWithJoins, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

/// The bounded recursion budget for user-defined-function calls, shared
/// across an entire statement's call chain (see `Exec::udf_depth`) so
/// self-recursion is bounded regardless of Rust call-stack depth.
/// PostgreSQL's analogous guard is a stack-depth check reported as SQLSTATE
/// 54001 (`statement_too_complex`); reused here for the same reason (no hang,
/// no unbounded native stack growth).
///
/// Each nested call re-parses the callee's body and recurses through the
/// expression evaluator/statement executor on the real Rust call stack (see
/// [`call_function_inner`]), so — unlike the `WITH RECURSIVE` iteration cap,
/// which bounds a `loop`, not stack depth — this constant is sized to stay
/// well under a worker thread's stack (e.g. Tokio's 2 MiB default) rather
/// than to match PostgreSQL's own default `max_stack_depth`.
const MAX_CALL_DEPTH: u32 = 25;

// ---------------------------------------------------------------------------
// CREATE / DROP FUNCTION
// ---------------------------------------------------------------------------

impl Exec {
    pub fn exec_create_function(&mut self, cf: &CreateFunction) -> Result<ExecResult> {
        if cf.temporary {
            return Err(unsupported("CREATE TEMPORARY FUNCTION"));
        }
        let (schema_opt, name) = split_schema_table(&cf.name);
        let schema = self.catalog.creation_schema(schema_opt.as_deref())?;
        let args = parse_function_args(cf.args.as_deref().unwrap_or(&[]))?;
        let return_type = parse_return_type(&cf.return_type, &name)?;
        let language = parse_language(cf.language.as_ref())?;
        let volatility = match cf.behavior {
            Some(FunctionBehavior::Immutable) => FunctionVolatility::Immutable,
            Some(FunctionBehavior::Stable) => FunctionVolatility::Stable,
            Some(FunctionBehavior::Volatile) | None => FunctionVolatility::Volatile,
        };
        let strict = matches!(
            cf.called_on_null,
            Some(FunctionCalledOnNull::Strict) | Some(FunctionCalledOnNull::ReturnsNullOnNullInput)
        );
        let body = extract_body_text(cf.function_body.as_ref(), &name)?;

        // Reject at DDL time (see module docs): syntax plus the fixed
        // unsupported-construct list. Table/column existence is still
        // deferred to call time, matching PostgreSQL (a function may
        // legitimately reference a table created later).
        match language {
            FunctionLanguage::Sql => {
                parse_body_statements(&body)?;
            }
            FunctionLanguage::PlPgSql => {
                let prog = parse_plpgsql(&body)?;
                if !always_returns(&prog.body) {
                    return Err(SqlError::InvalidFunctionDefinition(format!(
                        "function \"{name}\" control reached end of function without RETURN"
                    )));
                }
            }
        }

        let oid = self.catalog.allocate_oid();
        let def = FunctionDef {
            oid,
            schema,
            name: name.clone(),
            args,
            return_type,
            language,
            volatility,
            strict,
            body,
        };
        if cf.or_replace {
            self.catalog.replace_function(def);
        } else {
            self.catalog.insert_function(def)?;
        }
        self.catalog_dirty = true;
        Ok(ExecResult::empty_command("CREATE FUNCTION"))
    }

    pub fn exec_drop_function(&mut self, df: &DropFunction) -> Result<ExecResult> {
        for desc in &df.func_desc {
            let (schema_opt, name) = split_schema_table(&desc.name);
            match &desc.args {
                Some(arg_list) => {
                    let removed =
                        self.catalog
                            .drop_function(schema_opt.as_deref(), &name, arg_list.len());
                    if !removed && !df.if_exists {
                        let types = arg_list
                            .iter()
                            .map(|a| a.data_type.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(SqlError::UndefinedFunction(format!("{name}({types})")));
                    }
                }
                None => match self
                    .catalog
                    .drop_function_by_name(schema_opt.as_deref(), &name)
                {
                    DropFunctionByName::Removed => {}
                    DropFunctionByName::NotFound => {
                        if !df.if_exists {
                            return Err(SqlError::UndefinedFunction(name.clone()));
                        }
                    }
                    DropFunctionByName::Ambiguous => {
                        return Err(SqlError::AmbiguousFunction(format!(
                            "function name \"{name}\" is not unique; specify the argument \
                             list to select the function unambiguously"
                        )));
                    }
                },
            }
        }
        self.catalog_dirty = true;
        Ok(ExecResult::empty_command("DROP FUNCTION"))
    }
}

fn parse_function_args(args: &[OperateFunctionArg]) -> Result<Vec<FunctionArgDef>> {
    args.iter()
        .enumerate()
        .map(|(i, a)| {
            match a.mode {
                Some(ArgMode::Out) => return Err(unsupported("OUT parameters")),
                Some(ArgMode::InOut) => return Err(unsupported("INOUT parameters")),
                Some(ArgMode::Variadic) => return Err(unsupported("VARIADIC parameters")),
                Some(ArgMode::In) | None => {}
            }
            let name = a
                .name
                .as_ref()
                .map(ident_name)
                .unwrap_or_else(|| format!("${}", i + 1));
            let ty = crate::sql::eval::parse_data_type(&a.data_type)?;
            Ok(FunctionArgDef { name, ty })
        })
        .collect()
}

fn parse_return_type(rt: &Option<FunctionReturnType>, fname: &str) -> Result<SqlType> {
    match rt {
        None => Err(SqlError::InvalidFunctionDefinition(format!(
            "function \"{fname}\" has no RETURNS clause"
        ))),
        Some(FunctionReturnType::SetOf(_)) => Err(unsupported("RETURNS SETOF")),
        Some(FunctionReturnType::DataType(DataType::Table(_))) => Err(unsupported("RETURNS TABLE")),
        Some(FunctionReturnType::DataType(dt)) => {
            if dt.to_string().eq_ignore_ascii_case("trigger") {
                return Err(unsupported("trigger functions (RETURNS trigger)"));
            }
            crate::sql::eval::parse_data_type(dt)
        }
    }
}

fn parse_language(lang: Option<&Ident>) -> Result<FunctionLanguage> {
    let Some(lang) = lang else {
        return Err(SqlError::InvalidFunctionDefinition(
            "LANGUAGE clause is required".into(),
        ));
    };
    let name = ident_name(lang);
    match name.as_str() {
        "sql" => Ok(FunctionLanguage::Sql),
        "plpgsql" => Ok(FunctionLanguage::PlPgSql),
        "c" | "internal" | "plpython3u" | "plperl" | "plperlu" | "pltcl" => {
            Err(unsupported(format!("LANGUAGE {name}")))
        }
        other => Err(SqlError::UndefinedObject(format!(
            "language \"{other}\" does not exist"
        ))),
    }
}

fn extract_body_text(body: Option<&CreateFunctionBody>, fname: &str) -> Result<String> {
    let (expr, has_link_symbol) = match body {
        None => {
            return Err(SqlError::InvalidFunctionDefinition(format!(
                "function \"{fname}\" has no function body (AS clause)"
            )));
        }
        Some(CreateFunctionBody::AsBeforeOptions { body, link_symbol }) => {
            (body, link_symbol.is_some())
        }
        Some(CreateFunctionBody::AsAfterOptions(body)) => (body, false),
        Some(other) => {
            return Err(unsupported(format!("function body form: {other:?}")));
        }
    };
    if has_link_symbol {
        return Err(unsupported("LANGUAGE C functions"));
    }
    match expr {
        Expr::Value(vws) => match &vws.value {
            Value::DollarQuotedString(d) => Ok(d.value.clone()),
            Value::SingleQuotedString(s) => Ok(s.clone()),
            other => Err(SqlError::Syntax(format!(
                "unsupported function body literal: {other}"
            ))),
        },
        other => Err(SqlError::Syntax(format!(
            "unsupported function body expression: {other}"
        ))),
    }
}

/// Parse a `LANGUAGE SQL` body into its statement list, rejecting anything
/// but plain `SELECT`/`INSERT`/`UPDATE`/`DELETE`.
fn parse_body_statements(body: &str) -> Result<Vec<Statement>> {
    let stmts = crate::sql::parser::parse_sql(body)?;
    if stmts.is_empty() {
        return Err(SqlError::InvalidFunctionDefinition(
            "function body must contain at least one statement".into(),
        ));
    }
    for s in &stmts {
        ensure_supported_body_stmt(s)?;
    }
    Ok(stmts)
}

fn ensure_supported_body_stmt(stmt: &Statement) -> Result<()> {
    match stmt {
        Statement::Query(_)
        | Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_) => Ok(()),
        other => Err(SqlError::FeatureNotSupported(format!(
            "{} is not supported inside a function body",
            stmt_kind_name(other)
        ))),
    }
}

/// A short, human-readable name for a statement kind (`"CREATE TABLE"`,
/// `"ALTER TABLE"`, ...), used only for error messages.
fn stmt_kind_name(stmt: &Statement) -> String {
    stmt.to_string()
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// PL/pgSQL: AST + recursive-descent parser
// ---------------------------------------------------------------------------

struct PlpgsqlProgram {
    decls: Vec<PlpgsqlDecl>,
    body: Vec<PlpgsqlStmt>,
}

struct PlpgsqlDecl {
    name: String,
    default: Option<Expr>,
}

enum PlpgsqlStmt {
    Assign {
        name: String,
        value: Expr,
    },
    If {
        branches: Vec<(Expr, Vec<PlpgsqlStmt>)>,
        else_branch: Option<Vec<PlpgsqlStmt>>,
    },
    Return(Option<Expr>),
    Raise {
        level: RaiseLevel,
        message: String,
        args: Vec<Expr>,
    },
    Sql(Box<Statement>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RaiseLevel {
    Notice,
    Warning,
    Exception,
}

/// Statement-leading keywords that put the body outside the supported
/// PL/pgSQL subset, each with the name used in the resulting `0A000` message.
const UNSUPPORTED_STMT_KEYWORDS: &[(&str, &str)] = &[
    ("FOR", "FOR loop"),
    ("WHILE", "WHILE loop"),
    ("LOOP", "LOOP"),
    ("EXCEPTION", "EXCEPTION handler"),
    ("EXECUTE", "dynamic SQL (EXECUTE)"),
    ("DECLARE", "nested block"),
    ("BEGIN", "nested block"),
    ("PERFORM", "PERFORM"),
    ("OPEN", "cursors"),
    ("FETCH", "cursors"),
    ("CLOSE", "cursors"),
    ("GET", "GET DIAGNOSTICS"),
];

fn parse_plpgsql(body: &str) -> Result<PlpgsqlProgram> {
    let mut p = Parser::new(&PostgreSqlDialect {})
        .try_with_sql(body)
        .map_err(crate::sql::error::parse_error)?;
    let decls = if peek_word(&p).as_deref() == Some("DECLARE") {
        p.next_token();
        parse_decls(&mut p)?
    } else {
        Vec::new()
    };
    expect_word(&mut p, "BEGIN")?;
    let body_stmts = parse_stmt_list(&mut p, &["END"])?;
    expect_word(&mut p, "END")?;
    // Optional trailing block label (`END foo;`); best-effort skip of a
    // single identifier before the terminating `;`.
    if matches!(p.peek_token_ref().token, Token::Word(_)) {
        p.next_token();
    }
    expect_semi(&mut p)?;
    if p.peek_token_ref().token != Token::EOF {
        return Err(unsupported("content after the function body's closing END"));
    }
    Ok(PlpgsqlProgram {
        decls,
        body: body_stmts,
    })
}

fn peek_word(p: &Parser) -> Option<String> {
    match &p.peek_token_ref().token {
        Token::Word(w) => Some(w.value.to_ascii_uppercase()),
        _ => None,
    }
}

fn eat_word(p: &mut Parser, word: &str) -> bool {
    if peek_word(p).as_deref() == Some(word) {
        p.next_token();
        true
    } else {
        false
    }
}

fn expect_word(p: &mut Parser, word: &str) -> Result<()> {
    if eat_word(p, word) {
        Ok(())
    } else {
        Err(SqlError::Syntax(format!(
            "expected {word} in function body, found \"{}\"",
            p.peek_token_ref().token
        )))
    }
}

fn expect_semi(p: &mut Parser) -> Result<()> {
    p.expect_token(&Token::SemiColon)
        .map(|_| ())
        .map_err(crate::sql::error::parse_error)
}

fn parse_decls(p: &mut Parser) -> Result<Vec<PlpgsqlDecl>> {
    let mut out = Vec::new();
    loop {
        if peek_word(p).as_deref() == Some("BEGIN") {
            break;
        }
        if p.peek_token_ref().token == Token::EOF {
            return Err(SqlError::Syntax(
                "unexpected end of input in DECLARE section".into(),
            ));
        }
        let ident = p
            .parse_identifier()
            .map_err(crate::sql::error::parse_error)?;
        let name = ident_name(&ident);
        if peek_word(p).as_deref() == Some("CONSTANT") {
            p.next_token();
        }
        if peek_word(p).as_deref() == Some("CURSOR") {
            return Err(unsupported("cursors"));
        }
        let dt = p
            .parse_data_type()
            .map_err(crate::sql::error::parse_error)?;
        // Type-check the declared type; the value itself is not retained
        // (PL/pgSQL variables are dynamically typed in this simplified
        // interpreter — assignments are not checked against it).
        crate::sql::eval::parse_data_type(&dt)?;
        if peek_word(p).as_deref() == Some("NOT") {
            p.next_token();
            expect_word(p, "NULL")?;
        }
        let default = if p.peek_token_ref().token == Token::Assignment {
            p.next_token();
            Some(p.parse_expr().map_err(crate::sql::error::parse_error)?)
        } else if eat_word(p, "DEFAULT") {
            Some(p.parse_expr().map_err(crate::sql::error::parse_error)?)
        } else {
            None
        };
        expect_semi(p)?;
        out.push(PlpgsqlDecl { name, default });
    }
    Ok(out)
}

fn parse_stmt_list(p: &mut Parser, terminators: &[&str]) -> Result<Vec<PlpgsqlStmt>> {
    let mut out = Vec::new();
    loop {
        if let Some(w) = peek_word(p)
            && terminators.contains(&w.as_str())
        {
            break;
        }
        if p.peek_token_ref().token == Token::EOF {
            return Err(SqlError::Syntax("unexpected end of function body".into()));
        }
        out.push(parse_stmt(p)?);
    }
    Ok(out)
}

fn parse_stmt(p: &mut Parser) -> Result<PlpgsqlStmt> {
    if let Some(w) = peek_word(p) {
        match w.as_str() {
            "RETURN" => return parse_return(p),
            "RAISE" => return parse_raise(p),
            "IF" => return parse_if(p),
            "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" => return parse_sql_stmt(p),
            _ => {
                if let Some((_, label)) = UNSUPPORTED_STMT_KEYWORDS.iter().find(|(k, _)| *k == w) {
                    return Err(unsupported(*label));
                }
            }
        }
    }
    parse_assign(p)
}

fn parse_return(p: &mut Parser) -> Result<PlpgsqlStmt> {
    p.next_token();
    let value = if p.peek_token_ref().token == Token::SemiColon {
        None
    } else {
        Some(p.parse_expr().map_err(crate::sql::error::parse_error)?)
    };
    expect_semi(p)?;
    Ok(PlpgsqlStmt::Return(value))
}

fn parse_sql_stmt(p: &mut Parser) -> Result<PlpgsqlStmt> {
    let stmt = p
        .parse_statement()
        .map_err(crate::sql::error::parse_error)?;
    expect_semi(p)?;
    ensure_supported_body_stmt(&stmt)?;
    Ok(PlpgsqlStmt::Sql(Box::new(stmt)))
}

fn parse_assign(p: &mut Parser) -> Result<PlpgsqlStmt> {
    let found = p.peek_token_ref().token.clone();
    let ident = p
        .parse_identifier()
        .map_err(|_| SqlError::Syntax(format!("unexpected token in function body: \"{found}\"")))?;
    let name = ident_name(&ident);
    if p.peek_token_ref().token != Token::Assignment {
        return Err(SqlError::Syntax(format!(
            "expected \":=\" after \"{name}\" in function body"
        )));
    }
    p.next_token();
    let value = p.parse_expr().map_err(crate::sql::error::parse_error)?;
    expect_semi(p)?;
    Ok(PlpgsqlStmt::Assign { name, value })
}

fn parse_if(p: &mut Parser) -> Result<PlpgsqlStmt> {
    p.next_token(); // IF
    let cond = p.parse_expr().map_err(crate::sql::error::parse_error)?;
    expect_word(p, "THEN")?;
    let then_body = parse_stmt_list(p, &["ELSIF", "ELSE", "END"])?;
    let mut branches = vec![(cond, then_body)];
    while peek_word(p).as_deref() == Some("ELSIF") {
        p.next_token();
        let c = p.parse_expr().map_err(crate::sql::error::parse_error)?;
        expect_word(p, "THEN")?;
        let b = parse_stmt_list(p, &["ELSIF", "ELSE", "END"])?;
        branches.push((c, b));
    }
    let else_branch = if peek_word(p).as_deref() == Some("ELSE") {
        p.next_token();
        Some(parse_stmt_list(p, &["END"])?)
    } else {
        None
    };
    expect_word(p, "END")?;
    expect_word(p, "IF")?;
    expect_semi(p)?;
    Ok(PlpgsqlStmt::If {
        branches,
        else_branch,
    })
}

fn parse_raise(p: &mut Parser) -> Result<PlpgsqlStmt> {
    p.next_token(); // RAISE
    let level = match peek_word(p).as_deref() {
        Some("NOTICE") => {
            p.next_token();
            RaiseLevel::Notice
        }
        Some("WARNING") => {
            p.next_token();
            RaiseLevel::Warning
        }
        Some("EXCEPTION") => {
            p.next_token();
            RaiseLevel::Exception
        }
        Some("DEBUG") | Some("LOG") | Some("INFO") => {
            p.next_token();
            RaiseLevel::Notice
        }
        _ => RaiseLevel::Exception, // PostgreSQL's default when omitted.
    };
    if p.peek_token_ref().token == Token::SemiColon {
        p.next_token();
        return Ok(PlpgsqlStmt::Raise {
            level,
            message: String::new(),
            args: Vec::new(),
        });
    }
    let message = p
        .parse_literal_string()
        .map_err(crate::sql::error::parse_error)?;
    let mut args = Vec::new();
    while p.consume_token(&Token::Comma) {
        args.push(p.parse_expr().map_err(crate::sql::error::parse_error)?);
    }
    if peek_word(p).as_deref() == Some("USING") {
        return Err(unsupported("RAISE ... USING"));
    }
    expect_semi(p)?;
    Ok(PlpgsqlStmt::Raise {
        level,
        message,
        args,
    })
}

/// Whether `stmts`, executed straight through, is guaranteed to hit a
/// `RETURN` (or a `RAISE EXCEPTION`, which aborts) before falling off the
/// end — a sound and complete check for this subset, since the only control
/// constructs are straight-line statements and `IF`/`ELSIF`/`ELSE` (no loops
/// or exception handlers to reintroduce fallthrough).
fn always_returns(stmts: &[PlpgsqlStmt]) -> bool {
    match stmts.last() {
        Some(PlpgsqlStmt::Return(_)) => true,
        Some(PlpgsqlStmt::Raise {
            level: RaiseLevel::Exception,
            ..
        }) => true,
        Some(PlpgsqlStmt::If {
            branches,
            else_branch,
        }) => {
            else_branch.as_ref().is_some_and(|e| always_returns(e))
                && branches.iter().all(|(_, b)| always_returns(b))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Preload support: table references a function body might touch.
// ---------------------------------------------------------------------------

/// Every plain-SQL statement embedded in `def`'s body (recursing into
/// PL/pgSQL's `IF`/`ELSIF`/`ELSE` branches), for the engine's statement
/// preload pass — a called function's `INSERT`/`UPDATE`/`DELETE` can touch
/// tables the calling statement's own text never mentions, and table
/// loading is synchronous and preload-driven (see `crate::sql::exec`), so
/// those tables must be folded into the preload set before execution
/// starts. Returns an empty list on a parse failure: bodies are already
/// validated at `CREATE FUNCTION` time, so this only defends against a
/// hand-edited/corrupted catalog document, not user error.
pub fn body_statements(def: &FunctionDef) -> Vec<Statement> {
    match def.language {
        FunctionLanguage::Sql => parse_body_statements(&def.body).unwrap_or_default(),
        FunctionLanguage::PlPgSql => parse_plpgsql(&def.body)
            .map(|p| collect_plpgsql_sql_stmts(&p.body))
            .unwrap_or_default(),
    }
}

fn collect_plpgsql_sql_stmts(stmts: &[PlpgsqlStmt]) -> Vec<Statement> {
    let mut out = Vec::new();
    for s in stmts {
        match s {
            PlpgsqlStmt::Sql(stmt) => out.push((**stmt).clone()),
            PlpgsqlStmt::If {
                branches,
                else_branch,
            } => {
                for (_, b) in branches {
                    out.extend(collect_plpgsql_sql_stmts(b));
                }
                if let Some(b) = else_branch {
                    out.extend(collect_plpgsql_sql_stmts(b));
                }
            }
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Calling a user-defined function.
// ---------------------------------------------------------------------------

/// Call `def` with already-evaluated `args`, from `exec` (the caller's
/// execution context — the top-level statement's, or a parent UDF call's).
/// Builds an owned sub-[`Exec`] seeded from `exec`'s current catalog/tables
/// (so the callee sees committed-so-far state, including any writes made
/// earlier in the same outer statement or by an enclosing recursive call),
/// executes the body, and folds any mutations the body made back into the
/// shared, `Rc`-backed mutation list `exec` itself will flush on commit.
pub fn call_function(exec: &Exec, def: &FunctionDef, args: Vec<SqlValue>) -> Result<SqlValue> {
    if args.len() != def.arity() {
        return Err(SqlError::UndefinedFunction(format!(
            "{}({} args)",
            def.name,
            args.len()
        )));
    }
    if def.strict && args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let depth = exec.udf_depth.fetch_add(1, Ordering::SeqCst) + 1;
    if depth > MAX_CALL_DEPTH {
        exec.udf_depth.fetch_sub(1, Ordering::SeqCst);
        return Err(SqlError::StatementTooComplex(format!(
            "function call depth limit exceeded (max {MAX_CALL_DEPTH}) while calling \"{}\"",
            def.name
        )));
    }
    let result = call_function_inner(exec, def, args);
    exec.udf_depth.fetch_sub(1, Ordering::SeqCst);
    result
}

fn call_function_inner(exec: &Exec, def: &FunctionDef, args: Vec<SqlValue>) -> Result<SqlValue> {
    match def.language {
        FunctionLanguage::Sql => {
            // PostgreSQL SQL-language bodies may reference declared
            // parameters either positionally (`$1`) or by name; both are
            // supported here — `$n` via `Exec::param` (the same mechanism a
            // prepared statement's placeholders use), names via the same
            // substitution PL/pgSQL bodies use.
            let vars = bind_named_args(&def.args, args.clone());
            let mut sub = new_sub_exec(exec, args);
            let stmts = parse_body_statements(&def.body)?;
            run_sql_body(&mut sub, &stmts, &vars)
        }
        FunctionLanguage::PlPgSql => {
            let mut sub = new_sub_exec(exec, Vec::new());
            let prog = parse_plpgsql(&def.body)?;
            let mut vars = bind_named_args(&def.args, args);
            for decl in &prog.decls {
                let value = match &decl.default {
                    Some(expr) => eval_with_vars(&sub, expr, &vars)?,
                    None => SqlValue::Null,
                };
                vars.insert(decl.name.clone(), value);
            }
            match run_plpgsql_body(&mut sub, &prog.body, &mut vars)? {
                Flow::Return(v) => Ok(v),
                Flow::Fallthrough => Err(SqlError::Internal(
                    "PL/pgSQL function fell through without RETURN (should have been \
                     rejected at CREATE FUNCTION time)"
                        .into(),
                )),
            }
        }
    }
}

/// A fresh, owned execution context for one UDF invocation: a snapshot of
/// the caller's catalog/tables (so the body reads consistent state) plus
/// shared handles for anything that must be visible to the caller
/// afterwards (mutations, the recursion-depth counter).
fn new_sub_exec(exec: &Exec, params: Vec<SqlValue>) -> Exec {
    let mut sub = Exec::new(
        exec.catalog.clone(),
        exec.tables.clone(),
        params,
        exec.now,
        exec.database.clone(),
        exec.username.clone(),
        exec.locks.clone(),
        exec.session_id,
    );
    sub.vars = RefCell::new(exec.vars.borrow().clone());
    sub.mutations = exec.mutations.clone();
    sub.udf_depth = exec.udf_depth.clone();
    sub
}

/// Bind declared parameter names to call-site argument values, in order.
fn bind_named_args(arg_defs: &[FunctionArgDef], args: Vec<SqlValue>) -> HashMap<String, SqlValue> {
    arg_defs.iter().map(|a| a.name.clone()).zip(args).collect()
}

fn run_sql_body(
    sub: &mut Exec,
    stmts: &[Statement],
    vars: &HashMap<String, SqlValue>,
) -> Result<SqlValue> {
    let mut last = SqlValue::Null;
    for stmt in stmts {
        let substituted = substitute_stmt(stmt, vars);
        last = exec_body_statement(sub, &substituted)?;
    }
    Ok(last)
}

/// Run one embedded plain-SQL statement and reduce its result to the
/// function-body scalar convention: the first row's first column (`NULL`
/// if it produced no rows), or `NULL` for a statement with no result rows
/// at all (e.g. an `INSERT` without `RETURNING`).
fn exec_body_statement(sub: &mut Exec, stmt: &Statement) -> Result<SqlValue> {
    sub.init_rls(stmt)?;
    match stmt {
        Statement::Query(q) => {
            if let Some(with) = &q.with {
                sub.materialize_with(with)?;
            }
            let rs = sub.exec_select_query(q, &[])?;
            Ok(scalar_of_rows(&rs.rows))
        }
        Statement::Insert(insert) => Ok(scalar_of_result(&sub.exec_insert(insert)?)),
        Statement::Update(update) => Ok(scalar_of_result(&sub.exec_update(update)?)),
        Statement::Delete(delete) => Ok(scalar_of_result(&sub.exec_delete(delete)?)),
        other => Err(SqlError::FeatureNotSupported(format!(
            "{} is not supported inside a function body",
            stmt_kind_name(other)
        ))),
    }
}

fn scalar_of_rows(rows: &[Vec<SqlValue>]) -> SqlValue {
    rows.first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or(SqlValue::Null)
}

fn scalar_of_result(result: &ExecResult) -> SqlValue {
    match result {
        ExecResult::Rows { rows, .. } => scalar_of_rows(rows),
        ExecResult::Command { .. } => SqlValue::Null,
    }
}

// ---------------------------------------------------------------------------
// PL/pgSQL interpreter.
// ---------------------------------------------------------------------------

/// Control-flow outcome of running a PL/pgSQL statement list.
enum Flow {
    Fallthrough,
    Return(SqlValue),
}

fn run_plpgsql_body(
    sub: &mut Exec,
    stmts: &[PlpgsqlStmt],
    vars: &mut HashMap<String, SqlValue>,
) -> Result<Flow> {
    for stmt in stmts {
        match run_plpgsql_stmt(sub, stmt, vars)? {
            Flow::Fallthrough => continue,
            ret @ Flow::Return(_) => return Ok(ret),
        }
    }
    Ok(Flow::Fallthrough)
}

fn run_plpgsql_stmt(
    sub: &mut Exec,
    stmt: &PlpgsqlStmt,
    vars: &mut HashMap<String, SqlValue>,
) -> Result<Flow> {
    match stmt {
        PlpgsqlStmt::Assign { name, value } => {
            let v = eval_with_vars(sub, value, vars)?;
            vars.insert(name.clone(), v);
            Ok(Flow::Fallthrough)
        }
        PlpgsqlStmt::Return(expr) => {
            let v = match expr {
                Some(e) => eval_with_vars(sub, e, vars)?,
                None => SqlValue::Null,
            };
            Ok(Flow::Return(v))
        }
        PlpgsqlStmt::Raise {
            level,
            message,
            args,
        } => {
            let text = render_raise_message(sub, message, args, vars)?;
            match level {
                // NOTICE/WARNING are accepted and are no-ops: GuardianDB has
                // no client notice channel to surface them on (see
                // `docs/postgres-compat.md`).
                RaiseLevel::Notice | RaiseLevel::Warning => Ok(Flow::Fallthrough),
                RaiseLevel::Exception => Err(SqlError::RaisedException(text)),
            }
        }
        PlpgsqlStmt::If {
            branches,
            else_branch,
        } => {
            for (cond, body) in branches {
                let c = eval_with_vars(sub, cond, vars)?;
                if c.truthy() == Some(true) {
                    return run_plpgsql_body(sub, body, vars);
                }
            }
            match else_branch {
                Some(body) => run_plpgsql_body(sub, body, vars),
                None => Ok(Flow::Fallthrough),
            }
        }
        PlpgsqlStmt::Sql(stmt) => {
            let substituted = substitute_stmt(stmt, vars);
            exec_body_statement(sub, &substituted)?;
            Ok(Flow::Fallthrough)
        }
    }
}

fn eval_with_vars(sub: &Exec, expr: &Expr, vars: &HashMap<String, SqlValue>) -> Result<SqlValue> {
    let substituted = substitute_expr(expr, vars);
    sub.eval(&substituted, &[])
}

/// Render a `RAISE` message, substituting `%` placeholders with `args`'
/// evaluated text values in order (`%%` is a literal `%`). Extra `%`s or
/// extra args are left as-is rather than erroring — a best-effort
/// simplification of PostgreSQL's stricter arity check.
fn render_raise_message(
    sub: &Exec,
    message: &str,
    args: &[Expr],
    vars: &HashMap<String, SqlValue>,
) -> Result<String> {
    if args.is_empty() {
        return Ok(message.to_string());
    }
    let mut out = String::with_capacity(message.len());
    let mut arg_iter = args.iter();
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            out.push('%');
            continue;
        }
        match arg_iter.next() {
            Some(expr) => {
                let v = eval_with_vars(sub, expr, vars)?;
                out.push_str(&v.to_text().unwrap_or_default());
            }
            None => out.push('%'),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// PL/pgSQL variable substitution.
//
// PL/pgSQL variables are referenced by bare name directly in expressions;
// GuardianDB's expression evaluator only resolves identifiers against table
// columns (see `crate::sql::eval`). Rather than teaching it about a second,
// PL/pgSQL-only namespace, a declared variable's *current value* is
// substituted as a literal directly into the expression/statement AST
// before it reaches the normal evaluator/executor — so `x := a + 1` and
// `UPDATE t SET v = v + amount WHERE id = target_id` run through the exact
// same code as any other expression or statement.
//
// This always prefers the variable over a same-named column when a body
// statement touches a real table — PostgreSQL's own default
// (`plpgsql.variable_conflict`) is stricter (`error` on ambiguity); this is
// a deliberate simplification, documented in `docs/postgres-compat.md`.
// ---------------------------------------------------------------------------

/// Encode `v` as a literal expression by round-tripping through its text
/// representation and an explicit cast to its own type name — reuses the
/// existing text parser/cast machinery instead of hand-building AST nodes
/// for every `SqlValue` variant.
fn value_to_expr(v: &SqlValue) -> Expr {
    if v.is_null() {
        return Expr::Value(Value::Null.into());
    }
    let text = v.to_text().unwrap_or_default().replace('\'', "''");
    let type_name = v.type_of().name();
    let sql = format!("CAST('{text}' AS {type_name})");
    crate::sql::parser::parse_expr(&sql).unwrap_or(Expr::Value(Value::Null.into()))
}

fn substitute_expr(expr: &Expr, vars: &HashMap<String, SqlValue>) -> Expr {
    if let Expr::Identifier(ident) = expr {
        let name = ident_name(ident);
        if let Some(v) = vars.get(&name) {
            return value_to_expr(v);
        }
        return expr.clone();
    }
    let mut out = expr.clone();
    match &mut out {
        Expr::BinaryOp { left, right, .. } => {
            **left = substitute_expr(left, vars);
            **right = substitute_expr(right, vars);
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner) => {
            **inner = substitute_expr(inner, vars);
        }
        Expr::Cast { expr: inner, .. } => {
            **inner = substitute_expr(inner, vars);
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            ..
        } => {
            **inner = substitute_expr(inner, vars);
            **low = substitute_expr(low, vars);
            **high = substitute_expr(high, vars);
        }
        Expr::InList {
            expr: inner, list, ..
        } => {
            **inner = substitute_expr(inner, vars);
            for item in list.iter_mut() {
                *item = substitute_expr(item, vars);
            }
        }
        Expr::Like {
            expr: inner,
            pattern,
            ..
        }
        | Expr::ILike {
            expr: inner,
            pattern,
            ..
        } => {
            **inner = substitute_expr(inner, vars);
            **pattern = substitute_expr(pattern, vars);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                **o = substitute_expr(o, vars);
            }
            for w in conditions.iter_mut() {
                w.condition = substitute_expr(&w.condition, vars);
                w.result = substitute_expr(&w.result, vars);
            }
            if let Some(e) = else_result {
                **e = substitute_expr(e, vars);
            }
        }
        Expr::Function(func) => substitute_function(func, vars),
        Expr::InSubquery { expr: inner, .. } => {
            **inner = substitute_expr(inner, vars);
        }
        _ => {}
    }
    out
}

fn substitute_function(func: &mut Function, vars: &HashMap<String, SqlValue>) {
    if let FunctionArguments::List(list) = &mut func.args {
        for arg in list.args.iter_mut() {
            let e = match arg {
                FunctionArg::Named { arg, .. }
                | FunctionArg::ExprNamed { arg, .. }
                | FunctionArg::Unnamed(arg) => arg,
            };
            if let FunctionArgExpr::Expr(e) = e {
                *e = substitute_expr(e, vars);
            }
        }
    }
}

fn substitute_stmt(stmt: &Statement, vars: &HashMap<String, SqlValue>) -> Statement {
    let mut out = stmt.clone();
    match &mut out {
        Statement::Query(q) => substitute_query(q, vars),
        Statement::Insert(insert) => {
            if let Some(src) = &mut insert.source {
                substitute_query(src, vars);
            }
            if let Some(OnInsert::OnConflict(oc)) = &mut insert.on
                && let OnConflictAction::DoUpdate(du) = &mut oc.action
            {
                for a in du.assignments.iter_mut() {
                    a.value = substitute_expr(&a.value, vars);
                }
                if let Some(sel) = &mut du.selection {
                    *sel = substitute_expr(sel, vars);
                }
            }
        }
        Statement::Update(update) => {
            for a in update.assignments.iter_mut() {
                a.value = substitute_expr(&a.value, vars);
            }
            if let Some(sel) = &mut update.selection {
                *sel = substitute_expr(sel, vars);
            }
        }
        Statement::Delete(delete) => {
            if let Some(sel) = &mut delete.selection {
                *sel = substitute_expr(sel, vars);
            }
        }
        _ => {}
    }
    out
}

fn substitute_query(q: &mut Query, vars: &HashMap<String, SqlValue>) {
    if let Some(with) = &mut q.with {
        for cte in with.cte_tables.iter_mut() {
            substitute_query(&mut cte.query, vars);
        }
    }
    substitute_setexpr(&mut q.body, vars);
}

fn substitute_setexpr(s: &mut SetExpr, vars: &HashMap<String, SqlValue>) {
    match s {
        SetExpr::Select(sel) => substitute_select(sel, vars),
        SetExpr::Query(q) => substitute_query(q, vars),
        SetExpr::SetOperation { left, right, .. } => {
            substitute_setexpr(left, vars);
            substitute_setexpr(right, vars);
        }
        SetExpr::Values(v) => {
            for row in v.rows.iter_mut() {
                for e in row.content.iter_mut() {
                    *e = substitute_expr(e, vars);
                }
            }
        }
        _ => {}
    }
}

fn substitute_select(sel: &mut Select, vars: &HashMap<String, SqlValue>) {
    if let Some(w) = &mut sel.selection {
        *w = substitute_expr(w, vars);
    }
    if let Some(h) = &mut sel.having {
        *h = substitute_expr(h, vars);
    }
    for item in sel.projection.iter_mut() {
        match item {
            SelectItem::UnnamedExpr(e) => *e = substitute_expr(e, vars),
            SelectItem::ExprWithAlias { expr, .. } => *expr = substitute_expr(expr, vars),
            _ => {}
        }
    }
    for twj in sel.from.iter_mut() {
        substitute_twj(twj, vars);
    }
}

fn substitute_twj(twj: &mut TableWithJoins, vars: &HashMap<String, SqlValue>) {
    use sqlparser::ast::{JoinConstraint, JoinOperator};
    for j in twj.joins.iter_mut() {
        if let JoinOperator::Inner(JoinConstraint::On(e))
        | JoinOperator::Left(JoinConstraint::On(e))
        | JoinOperator::Right(JoinConstraint::On(e))
        | JoinOperator::FullOuter(JoinConstraint::On(e)) = &mut j.join_operator
        {
            *e = substitute_expr(e, vars);
        }
    }
}
