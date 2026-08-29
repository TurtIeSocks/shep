# `shep dogs --available` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `shep dogs --available` lists the dogs an operator could adopt, read from the community index the docs site publishes, without a shepherd running and without downloading anything.

**Architecture:** Three layers, each testable alone. A new `fetch` module takes the connect-and-TLS setup out of bark's webhook client and adds a bounded GET. An `index` module parses, validates and **sanitises** the fetched JSON, treating every string in it as hostile input. The verb renders through the existing `Render` trait, so the table, `--format json` and the style dial all work unchanged.

**Tech Stack:** Rust 2024, MSRV 1.88. `tokio-rustls` and `webpki-roots`, both already workspace dependencies. No new crates.

**Spec:** [docs/brainstorming/specs/2026-08-21-dog-index-cli-design.md](../../brainstorming/specs/2026-08-21-dog-index-cli-design.md). Read it before Task 1; it carries reasoning this plan does not repeat.

## Global Constraints

- **`docs/idiomatic-rust.md`'s rules (IR-1..IR-45).** `core::error::Error`, never `std::error::Error`. `# Errors` sections on fallible public functions. `# Panics` with `#[track_caller]`. Invoke the `shep-idiomatic-rust` skill before writing any Rust here.
- **No new dependencies.** Everything needed is already in the workspace.
- **No em dashes or en dashes** in anything a person reads, including `///` comments clap renders into `--help`. A test pins the top-level help.
- **Clean-room rule, non-negotiable:** never open, read or reference `~/GitHub/pm2`.
- **`crates/shep-cli/src/cli.rs` carries the maintainer's own uncommitted work** near `DevArgs` (`cli.rs:1097`). Task 3 must edit that file. Stage by name, never `git add -A`, and **never run `git checkout` on it** - that would destroy work of hers that is not in any commit.
- **One cargo shape per task.** The workspace shares one target-dir lock. Gates each as their own command with `$?` read directly, never through a pipe: in zsh a pipeline's `$?` is the last command's.
- **Do not change bark's behaviour.** `crates/shep-cli/src/dog/bark/sinks.rs` is working, tested code on the path that pages people. The extraction takes TLS setup, URL parsing and connect. It leaves `build_request`, `write_and_read`, `read_response` and `parse_status_code` where they are.

## Verified facts, measured rather than assumed

Established 2026-08-21 by running them against the real host and reading the real code.

- **GitHub Pages sends `Content-Length` and never chunks.** Checked on a 404 (9,379 bytes) and a 40,856-byte page. No `Content-Encoding` unless the client asks.
- **A file path does not redirect.** `/docs/dogs` 301s to `/docs/dogs/`, but `/dogs.json` returns 200 directly.
- **`/dogs.json` is currently 404 on the live site**, because the index merge has not been deployed. Tests must not depend on the live URL.
- **`Commands::Dogs` is a unit variant** (`crates/shep-cli/src/lib.rs:1125`), dispatched as `Commands::Dogs => match connect_client(...)`. A wiring test at `lib.rs:1698` asserts `matches!(..., Commands::Dogs)` and **will fail to compile** once the variant takes a field. That is expected; update it.
- **`query::dogs`** (`crates/shep-cli/src/commands/query.rs:300`) renders via `request_and_render` with `DogRows`.
- **The rendering trait is `Render`** (`crates/shep-cli/src/output/mod.rs:144`): `fn headers() -> &'static [&'static str]`, `fn rows(&self) -> Vec<Vec<String>>`, and an optional `fn rows_for(&self, Presentation, bool)`. `DogRows` (`output/rows.rs:278`) is the precedent to follow.
- **`crates/shep-cli/src/http.rs` already exists**, 539 lines, and is an HTTP SERVER used by `dog::metrics`, `serve::worker` and both bark modules' tests. It already exports `HttpError`. The client goes in a NEW `fetch.rs` with a `FetchError`. I got this wrong in the first draft of this plan by checking for network dependencies and for bark's client without ever listing the source directory.
- **bark separates transport from policy**, and this plan copies that: `sinks.rs` speaks both http and https so its tests can bind an ephemeral plain-HTTP port, while `require_secure_scheme` enforces https at config time. Do the same. The `get` function supports both schemes; the index fetch applies the https policy on top.

---

### Task 1: A bounded GET

**Files:**
- Create: `crates/shep-cli/src/fetch.rs`
- Modify: `crates/shep-cli/src/dog/bark/sinks.rs`, `crates/shep-cli/src/lib.rs` (or `main.rs`, wherever modules are declared)

**`crates/shep-cli/src/http.rs` ALREADY EXISTS and is not yours.** It is 539 lines of HTTP *server* -- `read_request`, `write_response`, `write_head` -- with four callers: `dog::metrics`, `serve::worker`, and tests in both bark modules. **Do not add the client to it.** Its own module doc gives the reason out loud: it is deliberately TLS-free, "with no TLS to get wrong because the metrics endpoint is loopback by default". A TLS client contradicts that in the file that states it, and would push a 539-line module past 800.

It also **already exports `HttpError`**, imported by `metrics/mod.rs`, `serve/worker.rs` and both bark modules. The new error type is therefore called `FetchError`, and reusing or extending `HttpError` is a defect, not a shortcut.

The new module's doc comment must point at `http.rs` and say why they are separate: one serves a request, one fetches a document, and only the second needs TLS.

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct Target { pub https: bool, pub host: String, pub port: u16, pub path: String }`
  - `pub fn parse_url(url: &str) -> Result<Target, HttpError>`
  - `pub fn tls_connector() -> &'static tokio_rustls::TlsConnector`
  - `pub async fn get(target: &Target, limit: usize, timeout: Duration) -> Result<Vec<u8>, HttpError>`
  - `pub enum FetchError { Url(String), Transport(std::io::Error), Status(u16), Redirect { location: String }, Chunked, TooLarge { limit: usize }, Timeout, Truncated { expected: usize, got: usize } }`

- [ ] **Step 1: Move the shared pieces, changing no behaviour**

Move `tls_connector`, `SinkTarget` (renamed `Target`, fields made `pub`) and `parse_sink_url` (renamed `parse_url`) from `sinks.rs` into `fetch.rs`. `sinks.rs` imports them. Its own error type keeps mapping through its existing variants, so bark's messages do not change.

- [ ] **Step 2: Prove bark is untouched**

```bash
cargo test -p shep --lib --all-features -- bark
```
Expected: PASS, with the same count as before the move. **Record that count before you start**, so "the same" is a measurement rather than an impression.

- [ ] **Step 3: Write the failing tests for `get`**

In `fetch.rs`. Each binds an ephemeral plain-HTTP listener and serves a canned response, the way `sinks.rs`'s own tests already do; read those first for the harness shape.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Serves one canned response on an ephemeral port, then stops.
    async fn serve(response: &'static [u8]) -> Target { /* bind, spawn, return Target */ }

    #[tokio::test]
    async fn a_content_length_body_is_read_exactly() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").await;
        let body = get(&target, 1 << 20, Duration::from_secs(5)).await.expect("read");
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn a_chunked_response_is_refused_rather_than_misparsed() {
        let target = serve(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n").await;
        assert!(matches!(get(&target, 1 << 20, Duration::from_secs(5)).await, Err(FetchError::Chunked)));
    }

    #[tokio::test]
    async fn a_redirect_is_refused_and_names_where_it_pointed() {
        let target = serve(b"HTTP/1.1 301 Moved\r\nLocation: https://elsewhere/\r\nContent-Length: 0\r\n\r\n").await;
        let err = get(&target, 1 << 20, Duration::from_secs(5)).await.expect_err("refused");
        let FetchError::Redirect { location } = err else { panic!("wrong variant: {err:?}") };
        assert_eq!(location, "https://elsewhere/");
    }

    #[tokio::test]
    async fn a_body_over_the_limit_is_refused_before_it_is_read() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n").await;
        assert!(matches!(get(&target, 10, Duration::from_secs(5)).await, Err(FetchError::TooLarge { limit: 10 })));
    }

    #[tokio::test]
    async fn a_non_2xx_carries_its_status() {
        let target = serve(b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n").await;
        assert!(matches!(get(&target, 1 << 20, Duration::from_secs(5)).await, Err(FetchError::Status(500))));
    }

    #[tokio::test]
    async fn a_peer_that_closes_mid_body_is_an_error_not_a_short_read() {
        let target = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").await;
        let err = get(&target, 1 << 20, Duration::from_secs(5)).await.expect_err("refused");
        assert!(matches!(err, FetchError::Truncated { expected: 10, got: 5 }), "{err:?}");
    }

    #[tokio::test]
    async fn a_missing_content_length_is_refused() {
        let target = serve(b"HTTP/1.1 200 OK\r\n\r\nbody").await;
        assert!(get(&target, 1 << 20, Duration::from_secs(5)).await.is_err());
    }

    #[test]
    fn a_url_that_is_not_http_or_https_is_refused() {
        assert!(matches!(parse_url("file:///etc/passwd"), Err(FetchError::Url(_))));
        assert!(matches!(parse_url("not a url"), Err(FetchError::Url(_))));
    }
}
```

The size cap is checked **against the declared `Content-Length` before reading**, and again while reading, so neither a lying header nor an honest one can make shep read forever.

- [ ] **Step 4: Run them to watch them fail**

```bash
cargo test -p shep --lib --all-features -- fetch::
```
Expected: FAIL, `get` not defined.

- [ ] **Step 5: Implement `get`**

`GET {path} HTTP/1.1`, `Host`, `Connection: close`, and **no `Accept-Encoding`**, so no gzip can arrive. Read the status line, then headers to the blank line, then exactly `Content-Length` bytes. `tokio::time::timeout` wraps the whole exchange.

Refuse, in this order: a 3xx (naming `Location`), a non-2xx (carrying the code), `Transfer-Encoding` present, `Content-Length` absent or unparseable, `Content-Length` above `limit`.

- [ ] **Step 6: Run the tests, then the whole crate**

```bash
cargo test -p shep --lib --all-features -- fetch::
```
Expected: PASS, 8 tests.

```bash
cargo test -p shep --lib --bins --all-features
```
Expected: PASS, and bark's count matches Step 2's.

- [ ] **Step 7: Commit**

```bash
git add crates/shep-cli/src/fetch.rs crates/shep-cli/src/dog/bark/sinks.rs crates/shep-cli/src/lib.rs
git commit -m "feat(cli): a bounded GET, over bark's connect-and-TLS path"
```

---

### Task 2: Parsing the index, treating it as hostile

**Files:**
- Create: `crates/shep-cli/src/dog_index.rs`
- Modify: wherever modules are declared

**Interfaces:**
- Consumes: `fetch::{get, parse_url, FetchError}` (Task 1).
- Produces:
  - `pub struct AvailableDog { pub name: String, pub package: String, pub adopt_as: String, pub description: String, pub repo: String, pub license: String, pub category: String, pub source: DogSourceKind }`
  - `pub enum DogSourceKind { CargoGit { url: String }, GoInstall { module: String }, Manual { instructions: String } }`
  - `pub struct Index { pub dogs: Vec<AvailableDog>, pub skipped: usize, pub sanitised: usize }`
  - `pub fn parse_index(bytes: &[u8]) -> Result<Index, IndexError>`
  - `pub async fn fetch_index(url: &str) -> Result<Index, IndexError>`
  - `pub const DEFAULT_INDEX_URL: &str = "https://shep.turtlesocks.dev/dogs.json";`
  - `fn sanitise(field: &str) -> (String, bool)`, returning the cleaned string and whether anything was removed

**This task is the security boundary of the whole feature.** Everything it returns is printed to a terminal.

- [ ] **Step 1: Write the failing tests, sanitiser first**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_escape_class_is_stripped() {
        // Each of these reaches a terminal if it survives. shep emits colour
        // itself, so a reader cannot tell an entry's bytes from shep's own.
        let hostile = [
            ("\u{1b}[2J", "clears the screen"),
            ("\u{1b}]0;pwned\u{7}", "rewrites the window title"),
            ("\u{1b}[31mred", "forges shep's own colour"),
            ("before\rafter", "overwrites the line with a bare CR"),
            ("a\u{0}b", "a nul byte"),
            ("tab\there", "a raw tab"),
            ("line\nbreak", "escapes the row"),
        ];
        for (input, why) in hostile {
            let (clean, changed) = sanitise(input);
            assert!(changed, "{why}: should have been reported as sanitised");
            assert!(!clean.contains('\u{1b}'), "{why}: escape survived in {clean:?}");
            for ch in clean.chars() {
                assert!(!ch.is_control(), "{why}: control char survived in {clean:?}");
            }
        }
    }

    #[test]
    fn ordinary_text_is_left_exactly_alone() {
        let (clean, changed) = sanitise("Rotates grown log files. MIT OR Apache-2.0.");
        assert_eq!(clean, "Rotates grown log files. MIT OR Apache-2.0.");
        assert!(!changed);
    }

    #[test]
    fn non_ascii_prose_survives_because_it_is_not_the_threat() {
        let (clean, changed) = sanitise("rotiert Protokolldateien");
        assert_eq!(clean, "rotiert Protokolldateien");
        assert!(!changed);
    }

    #[test]
    fn a_sanitised_entry_still_lists_and_is_counted() {
        let index = parse_index(one_entry_with_description("clean\u{1b}[2Jhere").as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 1, "a hostile description does not remove the dog");
        assert_eq!(index.sanitised, 1);
        assert!(!index.dogs[0].description.contains('\u{1b}'));
    }

    #[test]
    fn a_malformed_entry_is_skipped_and_counted_while_its_neighbours_list() {
        // A missing `adopt_as`, beside two good entries.
        let index = parse_index(THREE_ENTRIES_MIDDLE_BROKEN).expect("parses");
        assert_eq!(index.dogs.len(), 2);
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn an_unknown_category_is_skipped_rather_than_shown() {
        let index = parse_index(one_entry_with_category("logz").as_bytes()).expect("parses");
        assert_eq!(index.dogs.len(), 0);
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn a_non_https_repo_is_skipped() {
        let index = parse_index(one_entry_with_repo("http://example.com/x").as_bytes()).expect("parses");
        assert_eq!(index.skipped, 1);
    }

    #[test]
    fn a_document_that_is_not_an_array_is_an_error_not_an_empty_list() {
        assert!(parse_index(b"{}").is_err());
    }

    #[test]
    fn an_empty_array_is_a_valid_empty_index() {
        let index = parse_index(b"[]").expect("parses");
        assert!(index.dogs.is_empty());
        assert_eq!(index.skipped, 0);
    }
}
```

- [ ] **Step 2: Run to watch them fail**

```bash
cargo test -p shep --lib --all-features -- dog_index::
```
Expected: FAIL.

- [ ] **Step 3: Implement**

`sanitise` keeps a character when `!ch.is_control()` and the char is not part of an escape sequence; the simplest correct rule is to drop every `char::is_control()` and every `\u{1b}`, then collapse the runs of whitespace that removal can leave. **Non-ASCII text is not the threat and must survive** - a German or Japanese description is ordinary.

`parse_index` deserialises to a permissive raw shape, then validates entry by entry. A failing entry increments `skipped` and is dropped. Sanitising every string field of a surviving entry increments `sanitised` once for the entry, not once per field.

`fetch_index` applies the https policy (refusing a non-https URL, the way `require_secure_scheme` does for a sink), calls `fetch::get` with a **1 MiB** limit and a **10 second** timeout, then `parse_index`.

- [ ] **Step 4: Run them**

```bash
cargo test -p shep --lib --all-features -- dog_index::
```
Expected: PASS, 9 tests.

- [ ] **Step 5: Mutation-verify the sanitiser, because it is the guard**

Make `sanitise` return its input unchanged, run only `dog_index::`, confirm `every_escape_class_is_stripped` and `a_sanitised_entry_still_lists_and_is_counted` both fail, then restore. **Report which tests caught it.** Three tests written earlier in this project passed while checking nothing, and every one was caught by breaking the thing rather than reading it.

- [ ] **Step 6: Commit**

```bash
git add crates/shep-cli/src/dog_index.rs crates/shep-cli/src/lib.rs
git commit -m "feat(cli): parse the dog index, treating every string in it as hostile"
```

---

### Task 3: The verb

**Files:**
- Modify: `crates/shep-cli/src/cli.rs`, `crates/shep-cli/src/lib.rs`, `crates/shep-cli/src/output/rows.rs`, `crates/shep-cli/src/commands/query.rs`
- Test: `crates/shep-cli/tests/cli_e2e.rs`

**Interfaces:**
- Consumes: everything from Tasks 1 and 2.
- Produces: `pub struct DogsArgs { pub available: bool, pub filter: Option<String> }`, `Commands::Dogs(DogsArgs)`, and `AvailableDogRows` implementing `Render`.

**`cli.rs` carries the maintainer's uncommitted work near `DevArgs` (`cli.rs:1097`).** Your change goes in the `Commands` enum, a different region, so there is no conflict. Stage by name. **Never `git checkout` that file.**

- [ ] **Step 1: Add the args and update the wiring test**

```rust
/// `shep dogs`, and the index of dogs you could adopt.
#[derive(Debug, clap::Args)]
pub struct DogsArgs {
    /// List the dogs published in the community index instead of the ones
    /// this shepherd is running. Needs no shepherd.
    #[arg(long)]
    pub available: bool,
    /// Narrow the listing to entries whose name, package or description
    /// contains this text, case-insensitively.
    #[arg(value_name = "FILTER")]
    pub filter: Option<String>,
}
```

`Commands::Dogs` becomes `Commands::Dogs(DogsArgs)`. The test at `lib.rs:1698` stops compiling; change its pattern to `Commands::Dogs(_)` and **add a second assertion** that `["shep", "dogs", "--available", "spot"]` parses with `available: true` and `filter: Some("spot")`, so the flag is pinned and not merely present.

- [ ] **Step 2: Dispatch without a shepherd**

```rust
Commands::Dogs(args) if args.available => {
    query::available_dogs(&mut streams, fmt, &args).await
}
Commands::Dogs(args) => match connect_client(&mut streams, fmt, &paths).await {
    Ok(client) => query::dogs(&client, &mut streams, fmt, &args).await,
    Err(code) => code,
},
```

The guard arm is what makes `--available` work with no daemon, which is the property the flag promises and the one an e2e test asserts.

- [ ] **Step 3: Render**

`AvailableDogRows(pub Vec<AvailableDog>)` implementing `Render`, headers `["NAME", "PACKAGE", "CATEGORY", "DESCRIPTION"]`. Follow `DogRows` at `output/rows.rs:278` for the house pattern.

The detail view, when the filter matches exactly one entry, prints the block the spec shows, with the install line from `source` and **the adopt line built from `adopt_as`, never from `name` or `package`.** Getting that wrong ships a copy-pasteable command that silently discards an operator's whole config section, which is the failure the whole feature exists to prevent.

Footer notes, when non-zero: `2 entries skipped`, `1 entry contained control characters`. A filter matching nothing prints `no dog matches "wombat"` and **exits 0** - that is an answer, not a failure.

- [ ] **Step 4: Write the e2e tests**

In `crates/shep-cli/tests/cli_e2e.rs`, serving a canned index from an ephemeral local port with `SHEP_DOG_INDEX` pointed at it.

- The table lists a known index.
- The detail view prints both commands, and the adopt line contains `adopt_as`, not `name`.
- **An entry whose description carries `\u{1b}[2J`, asserting the escape never appears in stdout.** This is the test the feature exists to pass. Assert on the raw bytes.
- A filter matching nothing exits 0 and says so.
- **`--available` succeeds with no shepherd running**, in a `$SHEP_HOME` where no daemon was ever started.
- A server returning 500, and one that closes mid-body, each producing a clear error naming the URL rather than a panic or a hang.

- [ ] **Step 5: Run the gate**

```bash
cargo test -p shep --lib --bins --all-features
```
```bash
cargo test -p shep --test cli_e2e --all-features
```
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
```bash
cargo fmt --all --check
```

- [ ] **Step 6: Regenerate the docs, because the CLI surface changed**

This is the project's docs hard trigger: a new flag is something an operator types.

```bash
cargo build --release
```
```bash
./web/scripts/generate-cli-reference.sh
```
```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

`git diff` on the generated reference is the check. Then read `web/src/pages/docs/dogs.astro` and `community-dogs.astro`: both now have a CLI route to the same data and should say so.

- [ ] **Step 7: Commit**

```bash
git add -- crates/shep-cli/src/cli.rs crates/shep-cli/src/lib.rs crates/shep-cli/src/output/rows.rs crates/shep-cli/src/commands/query.rs crates/shep-cli/tests/cli_e2e.rs web/
git commit -m "feat(cli): shep dogs --available lists the community index"
```

Stage by name. `crates/shep-cli/src/commands/init.rs` is the maintainer's and must not appear in this commit.

---

## Final verification

```bash
cargo fmt --all --check
```
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
```bash
cargo test --workspace --all-features
```
```bash
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```
```bash
cd web && npx astro build
```
```bash
cd web && npx astro check
```

Each as its own command with `$?` read directly, never through a pipe.

**Confirm before calling it done:** `git status --porcelain` still shows `crates/shep-cli/src/commands/init.rs` as modified and uncommitted. That is the maintainer's work. If it has vanished, something ran `git checkout` on it and it is not recoverable from any commit.
