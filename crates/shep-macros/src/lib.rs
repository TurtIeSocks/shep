//! The `DogConfig` derive, and nothing else.
//!
//! A dog (a plugin process the shepherd supervises) publishes a JSON Schema
//! for its own config so shep can render a settings pane for it. One kind of
//! field needs marking: a credential, such as a webhook URL, which a pane
//! shows as `<set>` rather than as its value.
//!
//! `schemars` can express that by hand, with
//! `#[schemars(extend("x-shep-secret" = true))]`. This crate exists because
//! `x-shep-secret` is then a string the author types: transpose two of its
//! letters and it compiles, the schema validates, the field is not marked,
//! and the credential is painted on screen. Nothing fails and nothing warns.
//! It cannot be linted either, because `schemars` takes a string literal for
//! the extension key, so no exported constant can go in that position. The
//! derive is what turns that into a compile error.
//!
//! Depend on `shep-client`, not on this crate directly. It re-exports the
//! derive next to the `DogConfig` trait the derive implements, so a dog takes
//! one dependency.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Attribute, Data, DeriveInput, Field, Fields, Meta, Token, parse_macro_input};

/// The attribute this derive claims. Only `#[shep(secret)]` is spelled with
/// it today.
const ATTR: &str = "shep";

/// The one option `#[shep(...)]` accepts.
const SECRET: &str = "secret";

/// Marks which of a dog's config fields are credentials, so shep can redact
/// them without the dog author ever typing the extension key that says so.
///
/// Derive it on the struct a dog deserializes its config into, alongside
/// `schemars::JsonSchema`, and mark each credential field with
/// `#[shep(secret)]`:
///
/// ```rust,ignore
/// use shep_client::dogs::DogConfig;
///
/// #[derive(serde::Deserialize, schemars::JsonSchema, DogConfig)]
/// struct Sink {
///     kind: String,
///     #[shep(secret)]
///     url: String,
/// }
/// ```
///
/// The example is `ignore`d rather than run because `shep-client` depends on
/// this crate, so this crate cannot depend back on it to compile a doctest.
/// The derive's behaviour is tested in `shep-client`, where it is used.
///
/// # What it expands to
///
/// An `impl shep_client::dogs::DogConfig`, carrying the names of the marked
/// fields and the extension key that marks them. The key comes from
/// `shep_core::dogs::SECRET_KEY` by way of `shep_client`'s re-export, so it is
/// never spelled out here or in the dog:
///
/// ```rust,ignore
/// impl ::shep_client::dogs::DogConfig for Sink {
///     const SECRET_KEY: &'static str = ::shep_client::dogs::SECRET_KEY;
///     const SECRET_FIELDS: &'static [&'static str] = &["url"];
/// }
/// ```
///
/// # Limitations
///
/// A field is named by its Rust identifier. A `#[serde(rename)]` or
/// `#[serde(rename_all)]` that changes what the field is called in the schema
/// is not followed, and the marker would then miss it silently, which is the
/// failure this derive exists to remove. Do not rename a field marked
/// `#[shep(secret)]`.
///
/// # Compile errors
///
/// Deliberate refusals, each with its own message:
///
/// - an enum, a union, or a tuple struct, none of which has a named field for
///   the schema to carry a property for;
/// - `#[shep(...)]` on the struct itself, which marks nothing;
/// - any option other than `secret`, which is the misspelling this crate is
///   here to catch.
///
/// A struct with no fields, or with no field marked, is accepted: a dog whose
/// config holds no credential still wants the impl.
#[proc_macro_derive(DogConfig, attributes(shep))]
pub fn derive_dog_config(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// The derive's whole body, in a form that can return an error instead of
/// a token stream.
fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    if let Some(attr) = find_shep_attribute(&input.attrs) {
        return Err(syn::Error::new_spanned(
            attr,
            "`#[shep(secret)]` marks a field, not the type: move it onto the \
             field that holds the credential",
        ));
    }

    let mut secrets = Vec::new();
    for field in fields_of(input)? {
        if is_secret(field)?
            && let Some(ident) = &field.ident
        {
            secrets.push(ident.to_string());
        }
    }

    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics ::shep_client::dogs::DogConfig for #name #ty_generics #where_clause {
            const SECRET_KEY: &'static str = ::shep_client::dogs::SECRET_KEY;
            const SECRET_FIELDS: &'static [&'static str] = &[#(#secrets),*];
        }
    })
}

/// The named fields of a struct, or the refusal that shape earns.
///
/// A unit struct has no fields and gets an empty list rather than an error: a
/// dog with nothing to configure is a real dog. A tuple struct is refused
/// instead of being treated the same way, because it does have fields and an
/// author marking one would reasonably expect the mark to land.
fn fields_of(input: &DeriveInput) -> syn::Result<Vec<&Field>> {
    let refusal = |what: &str| {
        Err(syn::Error::new_spanned(
            &input.ident,
            format!(
                "`DogConfig` cannot be derived for {what}: a dog's config is a \
                 struct with named fields, since `#[shep(secret)]` marks one of \
                 them by the name it carries in the schema",
            ),
        ))
    };

    match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => Ok(named.named.iter().collect()),
            Fields::Unit => Ok(Vec::new()),
            Fields::Unnamed(_) => refusal("a tuple struct"),
        },
        Data::Enum(_) => refusal("an enum"),
        Data::Union(_) => refusal("a union"),
    }
}

/// Whether a field carries `#[shep(secret)]`.
///
/// Repeating the attribute on one field is accepted and marks it once. It is
/// redundant rather than wrong, and a second error message for it would be
/// noise next to the one that matters, which is a misspelled option.
fn is_secret(field: &Field) -> syn::Result<bool> {
    let mut secret = false;
    for attr in &field.attrs {
        if !attr.path().is_ident(ATTR) {
            continue;
        }
        if !matches!(attr.meta, Meta::List(_)) {
            return Err(syn::Error::new_spanned(
                attr,
                "`#[shep]` needs an option in parentheses: the only one is \
                 `secret`, as in `#[shep(secret)]`",
            ));
        }
        attr.parse_nested_meta(|meta| {
            if !meta.path.is_ident(SECRET) {
                return Err(meta.error(
                    "unknown `shep` option: the only one is `secret`, as in \
                     `#[shep(secret)]`",
                ));
            }
            if meta.input.peek(Token![=]) {
                return Err(meta.error(
                    "`secret` is a flag and takes no value: write \
                     `#[shep(secret)]`",
                ));
            }
            secret = true;
            Ok(())
        })?;
    }
    Ok(secret)
}

/// The first `#[shep(...)]` in a list, for the places one does not belong.
fn find_shep_attribute(attrs: &[Attribute]) -> Option<&Attribute> {
    attrs.iter().find(|attr| attr.path().is_ident(ATTR))
}
