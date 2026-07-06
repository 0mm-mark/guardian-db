//! Full-text search: text search configurations and the config-driven
//! functions (`to_tsvector`, `to_tsquery`, `plainto_tsquery`, `ts_rank`,
//! `numnode`, `strip`, and the `@@` operator).
//!
//! Two configurations exist, mirroring PostgreSQL's:
//!   * `simple` — lowercase, split on non-word characters, no stemming, no
//!     stop words;
//!   * `english` — `simple` plus the snowball English stop-word list and the
//!     Porter stemmer (see [`porter_stem`] for the exact algorithm).
//!
//! Any other configuration name is `42704` with PostgreSQL's message shape.
//! The value-level types and raw (`::tsvector` / `::tsquery`) parsers live in
//! [`crate::relational::fts`].

use crate::relational::fts::{self, MAX_POS, TsLexeme, TsQueryNode};
use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};
use crate::sql::exec::Exec;
use crate::sql::ext::{bad_arg, missing_arg};

/// A resolved text search configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsConfig {
    Simple,
    English,
}

/// Resolve a configuration name (case-folded like PostgreSQL's `regconfig`;
/// an optional `pg_catalog.` qualifier is accepted). Unknown names — german,
/// french, ... do not exist in this engine — are `42704`.
pub fn resolve_config(name: &str) -> Result<TsConfig> {
    let lower = name.trim().to_ascii_lowercase();
    let base = lower.strip_prefix("pg_catalog.").unwrap_or(&lower);
    match base {
        "simple" => Ok(TsConfig::Simple),
        "english" => Ok(TsConfig::English),
        _ => Err(SqlError::UndefinedTsConfig(base.to_string())),
    }
}

/// The session's default configuration: `default_text_search_config` when
/// SET, else `pg_catalog.english` (what initdb picks for English locales).
pub fn default_config(exec: &Exec) -> Result<TsConfig> {
    let name = exec
        .vars
        .borrow()
        .get("default_text_search_config")
        .cloned()
        .unwrap_or_else(|| "pg_catalog.english".to_string());
    resolve_config(&name)
}

// ---------------------------------------------------------------------------
// Tokenizer and per-token normalization.
// ---------------------------------------------------------------------------

/// Split into tokens (maximal alphanumeric runs — PostgreSQL's default parser
/// treats every other character as a separator) with 1-based positions.
/// Every token consumes a position, including ones later dropped as stop
/// words, so `'The Fat Rats'` yields fat:2 rat:3 like PostgreSQL.
fn tokenize(text: &str) -> Vec<(&str, u16)> {
    let mut out = Vec::new();
    let mut pos: u32 = 0;
    for tok in text.split(|c: char| !c.is_alphanumeric()) {
        if tok.is_empty() {
            continue;
        }
        pos += 1;
        out.push((tok, pos.min(MAX_POS as u32) as u16));
    }
    out
}

/// Normalize one token under a configuration. `None` = dropped (stop word).
/// Tokens containing digits or non-ASCII letters take the simple-dictionary
/// path even under `english`, like PostgreSQL's `numword`/`word` token types.
fn normalize_token(config: TsConfig, token: &str) -> Option<String> {
    let lower = token.to_lowercase();
    match config {
        TsConfig::Simple => Some(lower),
        TsConfig::English => {
            if !lower.bytes().all(|b| b.is_ascii_lowercase()) {
                return Some(lower);
            }
            if STOP_WORDS.contains(&lower.as_str()) {
                return None;
            }
            Some(porter_stem(&lower))
        }
    }
}

/// `to_tsvector`: tokenize, normalize per config, collect positions.
pub fn to_tsvector(config: TsConfig, text: &str) -> Vec<TsLexeme> {
    let mut raw: Vec<(String, Vec<u16>)> = Vec::new();
    for (token, pos) in tokenize(text) {
        if let Some(word) = normalize_token(config, token) {
            raw.push((word, vec![pos]));
        }
    }
    fts::normalize_lexemes(raw)
}

/// `to_tsquery`: full `&`/`|`/`!`/parens syntax; every lexeme operand is
/// normalized per config, and operands that normalize away (stop words)
/// are dropped with the tree rewritten around them, like PostgreSQL.
pub fn to_tsquery(config: TsConfig, input: &str) -> Result<Option<TsQueryNode>> {
    let Some(tree) = fts::parse_tsquery(input)? else {
        // PostgreSQL: to_tsquery('') is a syntax error ('':tsquery is not).
        return Err(SqlError::Syntax(format!(
            "syntax error in tsquery: \"{input}\""
        )));
    };
    map_query(config, &tree)
}

fn map_query(config: TsConfig, node: &TsQueryNode) -> Result<Option<TsQueryNode>> {
    match node {
        TsQueryNode::Lexeme(raw) => {
            let mut words: Vec<String> = Vec::new();
            for (token, _) in tokenize(raw) {
                if let Some(w) = normalize_token(config, token) {
                    words.push(w);
                }
            }
            match words.len() {
                0 => Ok(None),
                1 => Ok(Some(TsQueryNode::Lexeme(words.pop().unwrap()))),
                _ => Err(SqlError::FeatureNotSupported(format!(
                    "to_tsquery operand \"{raw}\" normalizes to multiple lexemes, which \
                     requires the phrase operator <-> (out of the full-text-search subset)"
                ))),
            }
        }
        TsQueryNode::Not(c) => {
            Ok(map_query(config, c)?.map(|inner| TsQueryNode::Not(Box::new(inner))))
        }
        TsQueryNode::And(a, b) | TsQueryNode::Or(a, b) => {
            let is_and = matches!(node, TsQueryNode::And(..));
            let (l, r) = (map_query(config, a)?, map_query(config, b)?);
            Ok(match (l, r) {
                (Some(l), Some(r)) => Some(if is_and {
                    TsQueryNode::And(Box::new(l), Box::new(r))
                } else {
                    TsQueryNode::Or(Box::new(l), Box::new(r))
                }),
                (Some(one), None) | (None, Some(one)) => Some(one),
                (None, None) => None,
            })
        }
    }
}

/// `plainto_tsquery`: no operator syntax — tokenize the whole text like
/// `to_tsvector` and AND the surviving lexemes. Empty in, empty query out.
pub fn plainto_tsquery(config: TsConfig, text: &str) -> Option<TsQueryNode> {
    let mut node: Option<TsQueryNode> = None;
    for (token, _) in tokenize(text) {
        if let Some(word) = normalize_token(config, token) {
            let leaf = TsQueryNode::Lexeme(word);
            node = Some(match node {
                None => leaf,
                Some(acc) => TsQueryNode::And(Box::new(acc), Box::new(leaf)),
            });
        }
    }
    node
}

// ---------------------------------------------------------------------------
// Ranking.
// ---------------------------------------------------------------------------

/// π²/6, PostgreSQL's per-lexeme rank damping constant.
const RANK_DIVISOR: f32 = 1.644_934;

/// `ts_rank` with PostgreSQL's frequency-based formula (`calc_rank_or` in
/// `tsrank.c`): each query lexeme found in the vector contributes
/// `Σ w/(j+1)²` over its positions (damped by π²/6), and the sum divides by
/// the number of query lexeme operands. `weights` is the standard
/// `{D, C, B, A}` array (default `{0.1, 0.2, 0.4, 1.0}`); without
/// `setweight` every lexeme carries weight D, so only `weights[0]` applies.
/// Simplification vs PostgreSQL: queries containing `&` also use this
/// accumulation (PostgreSQL switches to a pairwise position-distance formula
/// for AND/phrase queries); ranks remain monotonic in match count.
pub fn rank(weights: &[f32; 4], tv: &[TsLexeme], query: Option<&TsQueryNode>) -> f32 {
    let Some(root) = query else { return 0.0 };
    if tv.is_empty() {
        return 0.0;
    }
    let mut operands = Vec::new();
    root.lexemes(&mut operands);
    // PostgreSQL's SortAndUniqItems: repeated query terms count once.
    operands.sort();
    operands.dedup();
    if operands.is_empty() {
        return 0.0;
    }
    let w = weights[0];
    let mut res = 0.0f32;
    for lexeme in &operands {
        let Ok(i) = tv.binary_search_by(|l| l.word.as_str().cmp(lexeme)) else {
            continue;
        };
        // A stripped lexeme ranks as one default-weight occurrence.
        let npos = tv[i].positions.len().max(1);
        let mut resj = 0.0f32;
        for j in 0..npos {
            resj += w / (((j + 1) * (j + 1)) as f32);
        }
        res += resj / RANK_DIVISOR;
    }
    res / operands.len() as f32
}

// ---------------------------------------------------------------------------
// Scalar-function entry points (called from `funcs::call_scalar`).
// ---------------------------------------------------------------------------

/// Shared `([config,] text)` argument handling for the three constructors.
/// `Ok(None)` = a NULL argument (all these functions are strict).
fn config_and_text(
    exec: &Exec,
    func: &str,
    args: &[SqlValue],
) -> Result<Option<(TsConfig, String)>> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(None);
    }
    let text_arg = |idx: usize| -> Result<String> {
        match args.get(idx) {
            Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => Ok(s.clone()),
            Some(SqlValue::Json(_)) => Err(SqlError::FeatureNotSupported(format!(
                "{func}(json) is not supported (out of the full-text-search subset)"
            ))),
            Some(other) => Err(bad_arg(func, idx, "text", other)),
            None => Err(missing_arg(func, idx)),
        }
    };
    match args.len() {
        1 => Ok(Some((default_config(exec)?, text_arg(0)?))),
        2 => Ok(Some((resolve_config(&text_arg(0)?)?, text_arg(1)?))),
        n => Err(SqlError::UndefinedFunction(format!(
            "{func} with {n} arguments"
        ))),
    }
}

pub fn fn_to_tsvector(exec: &Exec, args: &[SqlValue]) -> Result<SqlValue> {
    Ok(match config_and_text(exec, "to_tsvector", args)? {
        None => SqlValue::Null,
        Some((config, text)) => SqlValue::TsVector(to_tsvector(config, &text)),
    })
}

pub fn fn_to_tsquery(exec: &Exec, args: &[SqlValue]) -> Result<SqlValue> {
    Ok(match config_and_text(exec, "to_tsquery", args)? {
        None => SqlValue::Null,
        Some((config, text)) => SqlValue::TsQuery(to_tsquery(config, &text)?),
    })
}

pub fn fn_plainto_tsquery(exec: &Exec, args: &[SqlValue]) -> Result<SqlValue> {
    Ok(match config_and_text(exec, "plainto_tsquery", args)? {
        None => SqlValue::Null,
        Some((config, text)) => SqlValue::TsQuery(plainto_tsquery(config, &text)),
    })
}

/// A tsvector argument; a text value takes the raw-parse path (how
/// PostgreSQL coerces an unknown literal via `tsvector_in`).
fn arg_tsvector(args: &[SqlValue], idx: usize, func: &str) -> Result<Vec<TsLexeme>> {
    match args.get(idx) {
        Some(SqlValue::TsVector(v)) => Ok(v.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::TsVector)? {
                SqlValue::TsVector(v) => Ok(v),
                _ => unreachable!("from_text(tsvector) yields TsVector"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "tsvector", other)),
        None => Err(missing_arg(func, idx)),
    }
}

/// A tsquery argument; text raw-parses like an unknown literal.
fn arg_tsquery(args: &[SqlValue], idx: usize, func: &str) -> Result<Option<TsQueryNode>> {
    match args.get(idx) {
        Some(SqlValue::TsQuery(q)) => Ok(q.clone()),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            match SqlValue::from_text(s, &SqlType::TsQuery)? {
                SqlValue::TsQuery(q) => Ok(q),
                _ => unreachable!("from_text(tsquery) yields TsQuery"),
            }
        }
        Some(other) => Err(bad_arg(func, idx, "tsquery", other)),
        None => Err(missing_arg(func, idx)),
    }
}

/// `ts_rank([weights,] tsvector, tsquery [, normalization])`.
pub fn fn_ts_rank(args: &[SqlValue]) -> Result<SqlValue> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let (weights, base) = match args.first() {
        Some(SqlValue::Array(items)) => {
            if items.len() < 4 {
                return Err(SqlError::InvalidParameter(
                    "array of weight is too short".into(),
                ));
            }
            let mut w = [0.0f32; 4];
            for (i, item) in items.iter().take(4).enumerate() {
                let f = item
                    .as_f64()
                    .ok_or_else(|| bad_arg("ts_rank", 0, "real[]", item))?
                    as f32;
                if f < 0.0 {
                    return Err(SqlError::InvalidParameter(
                        "array of weight must not contain negative values".into(),
                    ));
                }
                w[i] = f;
            }
            (w, 1)
        }
        _ => ([0.1, 0.2, 0.4, 1.0], 0),
    };
    let tv = arg_tsvector(args, base, "ts_rank")?;
    let query = arg_tsquery(args, base + 1, "ts_rank")?;
    match args.len() - base {
        2 => {}
        3 => {
            // The normalization bitmask: only 0 (no normalization, the
            // default) is in subset.
            let norm = args[base + 2].as_i64().unwrap_or(-1);
            if norm != 0 {
                return Err(SqlError::FeatureNotSupported(format!(
                    "ts_rank normalization option {norm} is not supported \
                     (only 0, the default, is in the full-text-search subset)"
                )));
            }
        }
        _ => {
            return Err(SqlError::UndefinedFunction(format!(
                "ts_rank with {} arguments",
                args.len()
            )));
        }
    }
    Ok(SqlValue::Float4(rank(&weights, &tv, query.as_ref())))
}

/// `numnode(tsquery)`: lexeme + operator count.
pub fn fn_numnode(args: &[SqlValue]) -> Result<SqlValue> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let q = arg_tsquery(args, 0, "numnode")?;
    Ok(SqlValue::Int4(q.map(|n| n.count_nodes()).unwrap_or(0)))
}

/// `strip(tsvector)`: drop all position information.
pub fn fn_strip(args: &[SqlValue]) -> Result<SqlValue> {
    if args.iter().any(SqlValue::is_null) {
        return Ok(SqlValue::Null);
    }
    let mut v = arg_tsvector(args, 0, "strip")?;
    for lex in &mut v {
        lex.positions.clear();
    }
    Ok(SqlValue::TsVector(v))
}

/// The `@@` match operator, in every argument order PostgreSQL defines:
/// `tsvector @@ tsquery`, `tsquery @@ tsvector`, `text @@ tsquery`
/// (`to_tsvector(x) @@ q` under the default config) and `text @@ text`
/// (`to_tsvector(x) @@ plainto_tsquery(y)`). A text operand opposite a
/// tsvector/tsquery raw-parses, the way PostgreSQL coerces unknown literals.
pub fn at_at(exec: &Exec, a: &SqlValue, b: &SqlValue) -> Result<SqlValue> {
    if a.is_null() || b.is_null() {
        return Ok(SqlValue::Null);
    }
    let text_of = |v: &SqlValue| -> Option<String> {
        match v {
            SqlValue::Text(s) | SqlValue::Citext(s) => Some(s.clone()),
            _ => None,
        }
    };
    let matched = match (a, b) {
        (SqlValue::TsVector(v), SqlValue::TsQuery(q))
        | (SqlValue::TsQuery(q), SqlValue::TsVector(v)) => matches_opt(v, q.as_ref()),
        (SqlValue::TsVector(v), other) | (other, SqlValue::TsVector(v)) => {
            let s = text_of(other).ok_or_else(|| at_at_mismatch(a, b))?;
            let q = fts::parse_tsquery(&s)?;
            matches_opt(v, q.as_ref())
        }
        (SqlValue::TsQuery(q), other) => {
            // PG resolves text @@ tsquery via to_tsvector, but an unknown
            // literal against a tsquery on the *left* coerces via tsvector_in.
            let s = text_of(other).ok_or_else(|| at_at_mismatch(a, b))?;
            let v = fts::parse_tsvector(&s)?;
            matches_opt(&v, q.as_ref())
        }
        (other, SqlValue::TsQuery(q)) => {
            let s = text_of(other).ok_or_else(|| at_at_mismatch(a, b))?;
            let v = to_tsvector(default_config(exec)?, &s);
            matches_opt(&v, q.as_ref())
        }
        _ => {
            let (Some(doc), Some(query)) = (text_of(a), text_of(b)) else {
                return Err(at_at_mismatch(a, b));
            };
            let config = default_config(exec)?;
            let v = to_tsvector(config, &doc);
            matches_opt(&v, plainto_tsquery(config, &query).as_ref())
        }
    };
    Ok(SqlValue::Bool(matched))
}

fn matches_opt(v: &[TsLexeme], q: Option<&TsQueryNode>) -> bool {
    q.is_some_and(|node| fts::eval_match(v, node))
}

fn at_at_mismatch(a: &SqlValue, b: &SqlValue) -> SqlError {
    SqlError::FeatureNotSupported(format!(
        "@@ is not supported between {} and {} (supported: tsvector @@ tsquery, \
         tsquery @@ tsvector, text @@ tsquery, text @@ text)",
        a.type_of().name(),
        b.type_of().name()
    ))
}

// ---------------------------------------------------------------------------
// English stop words and the Porter stemmer.
// ---------------------------------------------------------------------------

/// The snowball English stop-word list (PostgreSQL's `english.stop`).
static STOP_WORDS: [&str; 127] = [
    "i",
    "me",
    "my",
    "myself",
    "we",
    "our",
    "ours",
    "ourselves",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
    "he",
    "him",
    "his",
    "himself",
    "she",
    "her",
    "hers",
    "herself",
    "it",
    "its",
    "itself",
    "they",
    "them",
    "their",
    "theirs",
    "themselves",
    "what",
    "which",
    "who",
    "whom",
    "this",
    "that",
    "these",
    "those",
    "am",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "having",
    "do",
    "does",
    "did",
    "doing",
    "a",
    "an",
    "the",
    "and",
    "but",
    "if",
    "or",
    "because",
    "as",
    "until",
    "while",
    "of",
    "at",
    "by",
    "for",
    "with",
    "about",
    "against",
    "between",
    "into",
    "through",
    "during",
    "before",
    "after",
    "above",
    "below",
    "to",
    "from",
    "up",
    "down",
    "in",
    "out",
    "on",
    "off",
    "over",
    "under",
    "again",
    "further",
    "then",
    "once",
    "here",
    "there",
    "when",
    "where",
    "why",
    "how",
    "all",
    "any",
    "both",
    "each",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "no",
    "nor",
    "not",
    "only",
    "own",
    "same",
    "so",
    "than",
    "too",
    "very",
    "s",
    "t",
    "can",
    "will",
    "just",
    "don",
    "should",
    "now",
];

/// Is `w[i]` a consonant under Porter's definition (`y` is a consonant at the
/// start of the word or after a vowel)?
fn is_cons(w: &[u8], i: usize) -> bool {
    match w[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => i == 0 || !is_cons(w, i - 1),
        _ => true,
    }
}

/// Porter's measure *m* of the stem `w[..j]`: the number of VC sequences in
/// `[C](VC)^m[V]`.
fn measure(w: &[u8], j: usize) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < j && is_cons(w, i) {
        i += 1;
    }
    loop {
        while i < j && !is_cons(w, i) {
            i += 1;
        }
        if i >= j {
            return n;
        }
        while i < j && is_cons(w, i) {
            i += 1;
        }
        n += 1;
        if i >= j {
            return n;
        }
    }
}

fn has_vowel(w: &[u8], j: usize) -> bool {
    (0..j).any(|i| !is_cons(w, i))
}

fn ends_double_cons(w: &[u8]) -> bool {
    let j = w.len();
    j >= 2 && w[j - 1] == w[j - 2] && is_cons(w, j - 1)
}

/// The *o condition on the stem `w[..j]`: ends consonant-vowel-consonant with
/// the final consonant not w/x/y — plus snowball's short-syllable amendment
/// accepting a word-initial vowel+consonant, so `ate`/`use` keep their `e`
/// (classic Porter would strip it; PostgreSQL's english stemmer does not).
fn ends_cvc(w: &[u8], j: usize) -> bool {
    if j == 2 {
        return !is_cons(w, 0) && is_cons(w, 1);
    }
    j >= 3
        && is_cons(w, j - 3)
        && !is_cons(w, j - 2)
        && is_cons(w, j - 1)
        && !matches!(w[j - 1], b'w' | b'x' | b'y')
}

fn ends(w: &[u8], suffix: &str) -> bool {
    w.len() > suffix.len() && w.ends_with(suffix.as_bytes())
}

/// Apply the first (longest-first) matching suffix rule whose stem satisfies
/// `m > min_m`. A matching suffix whose condition fails still ends the step,
/// per Porter's "longest matching suffix decides" semantics.
fn apply_rules(w: &mut Vec<u8>, rules: &[(&str, &str)], min_m: usize) {
    for (suffix, replacement) in rules {
        if ends(w, suffix) {
            let j = w.len() - suffix.len();
            if measure(w, j) > min_m {
                w.truncate(j);
                w.extend_from_slice(replacement.as_bytes());
            }
            return;
        }
    }
}

/// The classic Porter (1980) stemming algorithm, with small snowball-english
/// alignments so common words stem the way PostgreSQL's `english_stem`
/// dictionary does (each marked inline):
///   1. step 1a's `-ies`/`-s` refinements (`ties` → `tie`, `gas` → `gas`);
///   2. step 1c replaces `y` only after a non-initial consonant
///      (`play` stays `play`, `cry` → `cri`);
///   3. the *o test also accepts a word-initial vowel+consonant
///      (`ate` keeps its `e`);
///   4. Porter2's short exception list (`dying` → `die`, `news` → `news`).
///
/// Where classic Porter and snowball still diverge, this is classic Porter.
/// Input must be lowercase ASCII letters.
pub fn porter_stem(word: &str) -> String {
    match word {
        "skis" => return "ski".into(),
        "skies" => return "sky".into(),
        "dying" => return "die".into(),
        "lying" => return "lie".into(),
        "tying" => return "tie".into(),
        "idly" => return "idl".into(),
        "gently" => return "gentl".into(),
        "ugly" => return "ugli".into(),
        "early" => return "earli".into(),
        "only" => return "onli".into(),
        "singly" => return "singl".into(),
        "sky" | "news" | "howe" | "atlas" | "cosmos" | "bias" | "andes" => return word.into(),
        _ => {}
    }
    let mut w = word.as_bytes().to_vec();
    if w.len() <= 2 {
        return word.into();
    }

    // Step 1a: plurals.
    if ends(&w, "sses") {
        w.truncate(w.len() - 2);
    } else if ends(&w, "ies") || ends(&w, "ied") {
        // Snowball treats -ied exactly like -ies (died → die, carried → carri).
        let stem = w.len() - 3;
        w.truncate(stem);
        w.push(b'i');
        // Snowball: after a single letter the suffix is -ie (ties → tie).
        if stem <= 1 {
            w.push(b'e');
        }
    } else if ends(&w, "ss") {
        // Unchanged.
    } else if w.last() == Some(&b's') {
        // Snowball: delete only when a vowel precedes the penultimate
        // letter (gaps → gap, but gas and this stay).
        if (0..w.len().saturating_sub(2)).any(|i| !is_cons(&w, i)) {
            w.truncate(w.len() - 1);
        }
    }

    // Step 1b: -eed / -ed / -ing.
    if ends(&w, "eed") {
        if measure(&w, w.len() - 3) > 0 {
            w.truncate(w.len() - 1);
        }
    } else {
        let removed = if ends(&w, "ed") && has_vowel(&w, w.len() - 2) {
            w.truncate(w.len() - 2);
            true
        } else if ends(&w, "ing") && has_vowel(&w, w.len() - 3) {
            w.truncate(w.len() - 3);
            true
        } else {
            false
        };
        if removed {
            if ends(&w, "at") || ends(&w, "bl") || ends(&w, "iz") {
                w.push(b'e');
            } else if ends_double_cons(&w) && !matches!(w[w.len() - 1], b'l' | b's' | b'z') {
                w.truncate(w.len() - 1);
            } else if measure(&w, w.len()) == 1 && ends_cvc(&w, w.len()) {
                w.push(b'e');
            }
        }
    }

    // Step 1c: y → i, snowball's condition (after a non-initial consonant).
    if w.last() == Some(&b'y') {
        let j = w.len() - 1;
        if j >= 2 && is_cons(&w, j - 1) {
            w[j] = b'i';
        }
    }

    // Step 2 (m > 0), longest suffix first.
    apply_rules(
        &mut w,
        &[
            ("ational", "ate"),
            ("ization", "ize"),
            ("iveness", "ive"),
            ("fulness", "ful"),
            ("ousness", "ous"),
            ("biliti", "ble"),
            ("tional", "tion"),
            ("ation", "ate"),
            ("alism", "al"),
            ("aliti", "al"),
            ("iviti", "ive"),
            ("entli", "ent"),
            ("ousli", "ous"),
            ("enci", "ence"),
            ("anci", "ance"),
            ("izer", "ize"),
            ("abli", "able"),
            ("alli", "al"),
            ("ator", "ate"),
            ("logi", "log"),
            ("eli", "e"),
            ("bli", "ble"),
        ],
        0,
    );

    // Step 3 (m > 0).
    apply_rules(
        &mut w,
        &[
            ("icate", "ic"),
            ("ative", ""),
            ("alize", "al"),
            ("iciti", "ic"),
            ("ical", "ic"),
            ("ness", ""),
            ("ful", ""),
        ],
        0,
    );

    // Step 4 (m > 1): bare suffix removal; -ion only after s/t.
    for suffix in [
        "ement", "ance", "ence", "able", "ible", "ment", "ant", "ent", "ion", "ism", "ate", "iti",
        "ous", "ive", "ize", "al", "er", "ic", "ou",
    ] {
        if ends(&w, suffix) {
            let j = w.len() - suffix.len();
            let ion_ok = suffix != "ion" || (j > 0 && matches!(w[j - 1], b's' | b't'));
            if measure(&w, j) > 1 && ion_ok {
                w.truncate(j);
            }
            break;
        }
    }

    // Step 5a: final -e.
    if w.last() == Some(&b'e') {
        let j = w.len() - 1;
        let m = measure(&w, j);
        if m > 1 || (m == 1 && !ends_cvc(&w, j)) {
            w.truncate(j);
        }
    }
    // Step 5b: -ll → -l when m > 1.
    if measure(&w, w.len()) > 1 && ends_double_cons(&w) && w[w.len() - 1] == b'l' {
        w.truncate(w.len() - 1);
    }

    String::from_utf8(w).expect("porter stemmer operates on ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porter_stems_pg_documented_examples() {
        // PostgreSQL docs: to_tsvector('english', 'The Fat Rats') = 'fat':2 'rat':3
        // and ts_lexize('english_stem', 'stars') = {star}.
        for (word, stem) in [
            ("rats", "rat"),
            ("stars", "star"),
            ("jumping", "jump"),
            ("jumps", "jump"),
            ("jumped", "jump"),
            ("ate", "ate"), // snowball keeps the e (classic Porter: "at")
            ("cats", "cat"),
            ("sat", "sat"),
            ("mats", "mat"),
        ] {
            assert_eq!(porter_stem(word), stem, "{word}");
        }
    }

    #[test]
    fn porter_stems_suffix_classes() {
        for (word, stem) in [
            // -ation / -ization class.
            ("relational", "relat"),
            ("operation", "oper"),
            ("organization", "organ"),
            ("generalizations", "gener"),
            ("conditional", "condit"),
            ("rational", "ration"),
            // -fulness / -ousness class.
            ("hopefulness", "hope"),
            ("callousness", "callous"),
            // classic vocabulary checks.
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "tie"),
            ("connection", "connect"),
            ("happiness", "happi"),
            ("controlling", "control"),
            ("agreed", "agre"),
            ("plastered", "plaster"),
            ("motoring", "motor"),
            ("hoping", "hope"),
            ("sky", "sky"),
            ("news", "news"),
            ("play", "play"),
            ("cry", "cri"),
        ] {
            assert_eq!(porter_stem(word), stem, "{word}");
        }
    }

    #[test]
    fn simple_config_lowercases_only() {
        let v = to_tsvector(TsConfig::Simple, "The Fat Rats");
        assert_eq!(fts::format_tsvector(&v), "'fat':2 'rats':3 'the':1");
    }

    #[test]
    fn english_config_stems_and_drops_stop_words() {
        // PG: SELECT to_tsvector('english', 'The Fat Rats') => 'fat':2 'rat':3
        let v = to_tsvector(TsConfig::English, "The Fat Rats");
        assert_eq!(fts::format_tsvector(&v), "'fat':2 'rat':3");
        // PG docs: 'a fat cat sat on a mat - it ate a fat rats'
        //          => 'ate':9 'cat':3 'fat':2,11 'mat':7 'rat':12 'sat':4
        let v = to_tsvector(
            TsConfig::English,
            "a fat cat sat on a mat - it ate a fat rats",
        );
        assert_eq!(
            fts::format_tsvector(&v),
            "'ate':9 'cat':3 'fat':2,11 'mat':7 'rat':12 'sat':4"
        );
    }

    #[test]
    fn to_tsquery_normalizes_and_drops_stop_words() {
        // PG: to_tsquery('english', 'The & Fat & Rats') => 'fat' & 'rat'
        let q = to_tsquery(TsConfig::English, "The & Fat & Rats").unwrap();
        assert_eq!(fts::format_tsquery(q.as_ref()), "'fat' & 'rat'");
        // A negated stop word drops with its operator.
        let q = to_tsquery(TsConfig::English, "fat & !the").unwrap();
        assert_eq!(fts::format_tsquery(q.as_ref()), "'fat'");
        // All-stop-word queries collapse to the empty query.
        let q = to_tsquery(TsConfig::English, "the & a").unwrap();
        assert_eq!(q, None);
    }

    #[test]
    fn plainto_ands_lexemes() {
        // PG: plainto_tsquery('english', 'The Fat & Rats:C') => 'fat' & 'rat' & 'c'
        let q = plainto_tsquery(TsConfig::English, "The Fat Rats");
        assert_eq!(fts::format_tsquery(q.as_ref()), "'fat' & 'rat'");
        assert_eq!(plainto_tsquery(TsConfig::English, "the a"), None);
    }

    #[test]
    fn rank_matches_pg_reference_values() {
        let w = [0.1f32, 0.2, 0.4, 1.0];
        // PG: ts_rank(to_tsvector('cat'), to_tsquery('cat')) = 0.06079271
        let tv = to_tsvector(TsConfig::English, "cat");
        let q = to_tsquery(TsConfig::English, "cat").unwrap();
        let r = rank(&w, &tv, q.as_ref());
        assert!((r - 0.06079271).abs() < 1e-6, "{r}");
        // PG: ts_rank(to_tsvector('cat cat'), to_tsquery('cat')) = 0.075990885
        let tv = to_tsvector(TsConfig::English, "cat cat");
        let r = rank(&w, &tv, q.as_ref());
        assert!((r - 0.07599089).abs() < 1e-6, "{r}");
        // More matched query terms rank higher.
        let tv = to_tsvector(TsConfig::English, "fat cat");
        let both = to_tsquery(TsConfig::English, "fat | cat").unwrap();
        let one = to_tsquery(TsConfig::English, "fat | dog").unwrap();
        assert!(rank(&w, &tv, both.as_ref()) > rank(&w, &tv, one.as_ref()));
    }

    #[test]
    fn unknown_config_is_undefined_object() {
        let err = resolve_config("german").unwrap_err();
        assert_eq!(err.sqlstate(), "42704");
        assert_eq!(
            err.to_string(),
            "text search configuration \"german\" does not exist"
        );
        assert!(resolve_config("pg_catalog.simple").is_ok());
        assert!(resolve_config("English").is_ok());
    }
}
