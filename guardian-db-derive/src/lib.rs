use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Attribute, Data, DeriveInput, Expr, Fields, GenericArgument, Lit, LitStr, Meta,
    PathArguments, Type, parse_macro_input, punctuated::Punctuated, token::Comma,
};

/// Derives `guardian_db::odm::Model` and a runtime `ModelSchema`.
///
/// Supported container attributes:
///
/// - `#[model(collection = "employees")]`
/// - `#[model(timestamps)]`
/// - `#[model(timestamps(created_at = "createdAt", updated_at = "updatedAt"))]`
/// - `#[model(flexible)]` (strict schemas are the default)
///
/// Supported field attributes: `#[primary_key]`, `#[unique]`, `#[index]`,
/// `#[created_at]`, and `#[updated_at]`.
#[proc_macro_derive(
    Model,
    attributes(model, primary_key, unique, index, created_at, updated_at)
)]
pub fn derive_model(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_model(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_model(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let rename_all = serde_rename_all(&input.attrs)?;
    let name = input.ident;
    let generics = input.generics;
    let (impl_generics, type_generics, where_clause) = generics.split_for_impl();

    let mut collection = format!("{}s", to_snake_case(&name.to_string()));
    let mut strict = true;
    let mut timestamps = false;
    let mut created_at_name = "created_at".to_string();
    let mut updated_at_name = "updated_at".to_string();
    let mut version = 1_u32;

    for attribute in input.attrs.iter().filter(|attr| attr.path().is_ident("model")) {
        attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("collection") {
                let value: LitStr = meta.value()?.parse()?;
                collection = value.value();
                return Ok(());
            }
            if meta.path.is_ident("strict") {
                strict = true;
                return Ok(());
            }
            if meta.path.is_ident("flexible") {
                strict = false;
                return Ok(());
            }
            if meta.path.is_ident("version") {
                let value: syn::LitInt = meta.value()?.parse()?;
                version = value.base10_parse()?;
                return Ok(());
            }
            if meta.path.is_ident("timestamps") {
                timestamps = true;
                if meta.input.peek(syn::token::Paren) {
                    meta.parse_nested_meta(|nested| {
                        if nested.path.is_ident("created_at") {
                            let value: LitStr = nested.value()?.parse()?;
                            created_at_name = value.value();
                            return Ok(());
                        }
                        if nested.path.is_ident("updated_at") {
                            let value: LitStr = nested.value()?.parse()?;
                            updated_at_name = value.value();
                            return Ok(());
                        }
                        Err(nested.error("unsupported timestamps option"))
                    })?;
                }
                return Ok(());
            }
            Err(meta.error("unsupported #[model(...)] option"))
        })?;
    }

    let fields = match input.data {
        Data::Struct(data) => match data.fields {
            Fields::Named(fields) => fields.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    name,
                    "Model can only be derived for structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                name,
                "Model can only be derived for structs",
            ));
        }
    };

    let mut field_tokens = Vec::new();
    let mut primary_count = 0_usize;
    let mut explicit_primary = false;
    let mut inferred_primary_index = None;

    for (position, field) in fields.into_iter().enumerate() {
        let serde = serde_field_attributes(&field.attrs)?;
        if serde.skip {
            continue;
        }
        if serde.flatten {
            // A flattened field contributes dynamic keys rather than one named
            // field, so the generated schema must permit those keys.
            strict = false;
            continue;
        }
        let rust_name = field.ident.expect("named field").to_string();
        let rust_name = rust_name.strip_prefix("r#").unwrap_or(&rust_name).to_string();
        let serialized_name = serde.rename.unwrap_or_else(|| {
            rename_all
                .as_deref()
                .map(|rule| apply_rename_all(&rust_name, rule))
                .unwrap_or_else(|| rust_name.clone())
        });
        let optional = option_inner(&field.ty).is_some() || serde.skip_serializing_if;
        let effective_type = option_inner(&field.ty).unwrap_or(&field.ty);
        let field_type = field_type_tokens(effective_type);

        let primary = has_attribute(&field.attrs, "primary_key");
        let unique = has_attribute(&field.attrs, "unique");
        let indexed = has_attribute(&field.attrs, "index");
        let is_created_at = has_attribute(&field.attrs, "created_at");
        let is_updated_at = has_attribute(&field.attrs, "updated_at");

        if is_created_at {
            timestamps = true;
            created_at_name = serialized_name.clone();
        }
        if is_updated_at {
            timestamps = true;
            updated_at_name = serialized_name.clone();
        }

        if !primary && (serialized_name == "id" || serialized_name == "_id") {
            inferred_primary_index = Some(position);
        }
        if primary {
            explicit_primary = true;
            primary_count += 1;
        }

        field_tokens.push(FieldExpansion {
            position,
            serialized_name,
            field_type,
            optional,
            primary,
            unique,
            indexed,
        });
    }

    if primary_count > 1 {
        return Err(syn::Error::new_spanned(
            &name,
            "only one #[primary_key] field is supported",
        ));
    }

    if !explicit_primary
        && let Some(position) = inferred_primary_index
        && let Some(field) = field_tokens.iter_mut().find(|field| field.position == position)
    {
        field.primary = true;
    }

    let has_primary = field_tokens.iter().any(|field| field.primary);
    let definitions = field_tokens.iter().map(|field| {
        let field_name = &field.serialized_name;
        let field_type = &field.field_type;
        let required = !field.optional;
        let primary = field.primary;
        let unique = field.unique;
        let indexed = field.indexed;
        quote! {
            {
                let mut field = ::guardian_db::odm::FieldDefinition::new(
                    #field_name,
                    #field_type,
                );
                if #required {
                    field = field.required();
                } else {
                    field = field.nullable();
                }
                if #primary {
                    field = field.primary_key();
                }
                if #unique {
                    field = field.unique();
                }
                if #indexed {
                    field = field.indexed();
                }
                schema.add_field(field);
            }
        }
    });

    let synthetic_primary = if has_primary {
        quote! {}
    } else {
        quote! {
            schema.add_field(
                ::guardian_db::odm::FieldDefinition::new(
                    "_id",
                    ::guardian_db::odm::FieldType::String,
                )
                .primary_key()
                .required(),
            );
        }
    };
    let timestamps_tokens = if timestamps {
        quote! {
            schema.enable_timestamps(#created_at_name, #updated_at_name);
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        impl #impl_generics ::guardian_db::odm::Model for #name #type_generics #where_clause {
            fn schema() -> ::guardian_db::odm::ModelSchema {
                let mut schema = ::guardian_db::odm::ModelSchema::new(
                    stringify!(#name),
                    #collection,
                );
                schema.set_strict(#strict);
                schema.set_version(#version);
                #(#definitions)*
                #synthetic_primary
                #timestamps_tokens
                schema
            }
        }
    })
}

struct FieldExpansion {
    position: usize,
    serialized_name: String,
    field_type: proc_macro2::TokenStream,
    optional: bool,
    primary: bool,
    unique: bool,
    indexed: bool,
}

fn has_attribute(attributes: &[Attribute], name: &str) -> bool {
    attributes.iter().any(|attribute| attribute.path().is_ident(name))
}

fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments.args.iter().find_map(|argument| match argument {
        GenericArgument::Type(inner) => Some(inner),
        _ => None,
    })
}

fn field_type_tokens(ty: &Type) -> proc_macro2::TokenStream {
    match ty {
        Type::Array(_) | Type::Slice(_) => quote!(::guardian_db::odm::FieldType::Array),
        Type::Path(path) => {
            let ident = path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
                .unwrap_or_default();
            match ident.as_str() {
                "String" | "str" | "char" => {
                    quote!(::guardian_db::odm::FieldType::String)
                }
                "bool" => quote!(::guardian_db::odm::FieldType::Boolean),
                "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16"
                | "u32" | "u64" | "u128" | "usize" | "f32" | "f64" => {
                    quote!(::guardian_db::odm::FieldType::Number)
                }
                "Vec" | "VecDeque" | "HashSet" | "BTreeSet" => {
                    quote!(::guardian_db::odm::FieldType::Array)
                }
                "DateTime" | "NaiveDateTime" | "SystemTime" => {
                    quote!(::guardian_db::odm::FieldType::Timestamp)
                }
                "Value" => quote!(::guardian_db::odm::FieldType::Any),
                _ => quote!(::guardian_db::odm::FieldType::Object),
            }
        }
        Type::Reference(reference) => field_type_tokens(&reference.elem),
        _ => quote!(::guardian_db::odm::FieldType::Any),
    }
}

#[derive(Default)]
struct SerdeFieldAttributes {
    rename: Option<String>,
    skip: bool,
    flatten: bool,
    skip_serializing_if: bool,
}

fn serde_field_attributes(attributes: &[Attribute]) -> syn::Result<SerdeFieldAttributes> {
    let mut result = SerdeFieldAttributes::default();
    for attribute in attributes.iter().filter(|attr| attr.path().is_ident("serde")) {
        for meta in parse_meta_list(attribute)? {
            match meta {
                Meta::Path(path)
                    if path.is_ident("skip") || path.is_ident("skip_serializing") =>
                {
                    result.skip = true;
                }
                Meta::Path(path) if path.is_ident("flatten") => {
                    result.flatten = true;
                }
                Meta::NameValue(name_value) if name_value.path.is_ident("rename") => {
                    result.rename = lit_string(&name_value.value);
                }
                Meta::NameValue(name_value)
                    if name_value.path.is_ident("skip_serializing_if") =>
                {
                    result.skip_serializing_if = true;
                }
                Meta::List(list) if list.path.is_ident("rename") => {
                    let nested = list
                        .parse_args_with(Punctuated::<Meta, Comma>::parse_terminated)?;
                    for meta in nested {
                        if let Meta::NameValue(name_value) = meta
                            && name_value.path.is_ident("serialize")
                        {
                            result.rename = lit_string(&name_value.value);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(result)
}

fn serde_rename_all(attributes: &[Attribute]) -> syn::Result<Option<String>> {
    let mut rename_all = None;
    for attribute in attributes.iter().filter(|attr| attr.path().is_ident("serde")) {
        for meta in parse_meta_list(attribute)? {
            if let Meta::NameValue(name_value) = meta
                && name_value.path.is_ident("rename_all")
            {
                rename_all = lit_string(&name_value.value);
            }
        }
    }
    Ok(rename_all)
}

fn parse_meta_list(attribute: &Attribute) -> syn::Result<Punctuated<Meta, Comma>> {
    attribute.parse_args_with(Punctuated::<Meta, Comma>::parse_terminated)
}

fn lit_string(expression: &Expr) -> Option<String> {
    let Expr::Lit(expression) = expression else {
        return None;
    };
    let Lit::Str(value) = &expression.lit else {
        return None;
    };
    Some(value.value())
}

fn apply_rename_all(value: &str, rule: &str) -> String {
    let words = identifier_words(value);
    match rule {
        "lowercase" => words.concat().to_lowercase(),
        "UPPERCASE" => words.concat().to_uppercase(),
        "PascalCase" => words
            .iter()
            .map(|word| capitalize(word))
            .collect::<Vec<_>>()
            .concat(),
        "camelCase" => {
            let mut iter = words.iter();
            let first = iter.next().map(|word| word.to_lowercase()).unwrap_or_default();
            first
                + &iter
                    .map(|word| capitalize(word))
                    .collect::<Vec<_>>()
                    .concat()
        }
        "snake_case" => words.join("_").to_lowercase(),
        "SCREAMING_SNAKE_CASE" => words.join("_").to_uppercase(),
        "kebab-case" => words.join("-").to_lowercase(),
        "SCREAMING-KEBAB-CASE" => words.join("-").to_uppercase(),
        _ => value.to_string(),
    }
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    first.to_uppercase().collect::<String>() + &characters.as_str().to_lowercase()
}

fn identifier_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let characters: Vec<char> = value.chars().collect();

    for (index, character) in characters.iter().copied().enumerate() {
        if character == '_' || character == '-' {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }

        let previous = index.checked_sub(1).and_then(|i| characters.get(i)).copied();
        let next = characters.get(index + 1).copied();
        let starts_word = character.is_uppercase()
            && previous.is_some_and(|previous| {
                previous.is_lowercase()
                    || previous.is_ascii_digit()
                    || (previous.is_uppercase() && next.is_some_and(char::is_lowercase))
            });
        if starts_word && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(character);
    }

    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn to_snake_case(value: &str) -> String {
    identifier_words(value).join("_").to_lowercase()
}
