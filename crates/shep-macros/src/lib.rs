//! The `DogConfig` derive, and nothing else.
//!
//! A dog (a plugin process the shepherd supervises) publishes a JSON Schema
//! for its own config so shep can render a settings pane for it. One kind of
//! field needs marking: a credential, such as the webhook URL in a bark sink,
//! which a pane shows as `<set>` rather than as its value.
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
use syn::{Attribute, Data, DeriveInput, Field, Meta, Token, parse_macro_input};

/// The attribute this derive claims. Only `#[shep(secret)]` is spelled with
/// it today.
const ATTR: &str = "shep";

/// The one option `#[shep(...)]` accepts.
const SECRET: &str = "secret";

/// Marks which of a dog's config fields are credentials, so shep can redact
/// them without the dog author ever typing the extension key that says so.
///
/// Derive it on the type a dog deserializes its config into, alongside
/// `schemars::JsonSchema`, and mark each credential field with
/// `#[shep(secret)]`. A struct and an enum both work, and the enum matters:
/// a bark sink is one, tagged by kind, with a webhook URL in every variant.
///
/// ```rust,ignore
/// use shep_client::dogs::DogConfig;
///
/// #[derive(serde::Deserialize, schemars::JsonSchema, DogConfig)]
/// #[serde(tag = "kind", rename_all = "snake_case")]
/// enum Sink {
///     Discord {
///         #[shep(secret)]
///         url: String,
///     },
///     Slack {
///         #[shep(secret)]
///         url: String,
///     },
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
/// One `"url"`, not two, for the two variants above: the list is names, and a
/// name repeated across variants is one name. The schema `schemars` builds
/// for a tagged enum is a `oneOf` of one object per variant, each with its own
/// `properties`, so whatever marks a field has to reach every occurrence of
/// the name rather than a single top-level property.
///
/// # Renames
///
/// A field is named here by its Rust identifier. A `#[serde(rename)]` on a
/// marked field changes what the schema calls it, and the marker would then
/// have no property to land on. That is caught where the marking happens, in
/// `shep_client`, which refuses a name it cannot find rather than passing an
/// unmarked credential on.
///
/// A `#[serde(rename_all)]` on a tagged enum renames the VARIANTS, not their
/// fields, so a `url` inside one stays `url`. Measured against `schemars`
/// 1.2.2 rather than assumed.
///
/// # Compile errors
///
/// Deliberate refusals, each with its own message:
///
/// - `#[shep(secret)]` on a field with no name, in a tuple struct or a tuple
///   variant, where a schema has no named property for it to mark;
/// - `#[shep(...)]` on the type or on a variant, neither of which is a field;
/// - a union, which has no serde representation to build a schema from;
/// - any option other than `secret`, which is the misspelling this crate is
///   here to catch.
///
/// Everything else is accepted and simply carries no marks: a struct or a
/// variant with no fields, an unmarked tuple, an enum of plain unit variants.
/// A dog whose config holds no credential still wants the impl.
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

    let mut secrets: Vec<String> = Vec::new();
    for field in fields_of(input)? {
        if !is_secret(field)? {
            continue;
        }
        let Some(ident) = &field.ident else {
            return Err(syn::Error::new_spanned(
                field,
                "`#[shep(secret)]` cannot mark an unnamed field: a JSON Schema \
                 property has a name and this field has none, so the mark would \
                 have nothing to land on. Name the field.",
            ));
        };
        let name = ident.to_string();
        // Deduped rather than pushed blind: an internally tagged enum repeats
        // a field name across its variants, the way a bark sink repeats
        // `url`, and the list is names to look for rather than places to look.
        if !secrets.contains(&name) {
            secrets.push(name);
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

/// Every field the type has, a struct's directly and an enum's gathered from
/// its variants.
///
/// Shapes that cannot hold a named field are not refused here, only left
/// empty. A unit variant has nothing to mark, and an unmarked tuple is a
/// config type someone wrote for other reasons. What is refused is a mark
/// that cannot land, and [`expand`] does that once it knows which fields
/// carry one, so the rule stays the same wherever the field came from.
fn fields_of(input: &DeriveInput) -> syn::Result<Vec<&Field>> {
    match &input.data {
        Data::Struct(data) => Ok(data.fields.iter().collect()),
        Data::Enum(data) => {
            let mut fields = Vec::new();
            for variant in &data.variants {
                if let Some(attr) = find_shep_attribute(&variant.attrs) {
                    return Err(syn::Error::new_spanned(
                        attr,
                        "`#[shep(secret)]` marks a field, not a variant: move \
                         it onto the field inside that holds the credential",
                    ));
                }
                fields.extend(variant.fields.iter());
            }
            Ok(fields)
        }
        Data::Union(_) => Err(syn::Error::new_spanned(
            &input.ident,
            "`DogConfig` cannot be derived for a union: a union has no serde \
             representation, so there is no schema for a mark to go into",
        )),
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
