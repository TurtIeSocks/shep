# shep-macros

The `DogConfig` derive for [shep](https://github.com/shep-pm/shep), a process
manager written in Rust. A dog (a plugin process shep supervises) publishes a
JSON Schema for its own config, and this derive is how it marks which of those
fields is a credential.

```rust,ignore
use shep_client::dogs::DogConfig;

#[derive(serde::Deserialize, schemars::JsonSchema, DogConfig)]
struct Sink {
    kind: SinkKind,
    #[shep(secret)]
    url: String,
}
```

A field marked `#[shep(secret)]` reaches shep carrying the `x-shep-secret`
schema extension, and shep shows `<set>` in place of the value.

Depend on `shep-client` rather than on this crate: it re-exports the derive
next to the trait the derive implements, so a dog takes one dependency.

## Why a derive and not a documented extension

`schemars` can already say the same thing by hand, with
`#[schemars(extend("x-shep-secret" = true))]`. The reason to ship a derive
anyway is that `x-shep-secret` is a string shep parses and the author types.
Transpose two of its letters and it still compiles, the schema still
validates, the field is simply not marked, and a webhook credential ends up
painted on screen. Nothing fails and nothing warns. It cannot be linted
either, because `schemars` takes a string literal for the extension key, so no
exported constant can go in that position. The derive is what turns that into
a compile error.

## License

MIT OR Apache-2.0, at your option.
