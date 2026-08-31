# shep-cli

This crate was renamed. Install [`shep`](https://crates.io/crates/shep) instead:

```bash
cargo install shep
```

## Why this crate exists

`shep`'s CLI binary was originally packaged under the name `shep-cli`. It was
renamed to `shep` before the first publish (see
[docs/releasing.md](https://github.com/shep-pm/shep/blob/main/docs/releasing.md)
in the main repository) so the install command and the binary name match.

Nothing was ever published under `shep-cli` — no version of it exists on
crates.io, so there is no migration to make and no code here to depend on.
This placeholder is published purely to hold the name: `shep`, `shep-core`,
`shep-daemon` and `shep-client` are visible under one `shep-*` naming
convention, which makes `shep-cli` a predictable name for someone else to
register, intentionally or not, adjacent to a project they have nothing to
do with. This crate exists so that does not happen.

It ships no library and no binary. There is nothing to build and nothing to
run.

## License

MIT OR Apache-2.0, at your option.
