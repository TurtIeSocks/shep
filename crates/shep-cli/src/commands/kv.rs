//! `shep set` / `shep get` / `shep unset`: the CLI half of
//! [`shep_core::kv`].
//!
//! No [`Client`](shep_client::Client) anywhere in this module — matching
//! `commands::dogs::barks`, the precedent for a verb that reads/writes a
//! file under `$SHEP_HOME` and never touches the socket. The store has to
//! work with no shepherd running, exactly as `shep enable` does (Task 12's
//! own module doc), so `main` dispatches straight off the resolved
//! [`ShepPaths`] rather than through `connect_client`.

use shep_core::kv::{self, KvError};
use shep_core::paths::ShepPaths;

use crate::cli::{Format, KvGetArgs, KvSetArgs, KvUnsetArgs};
use crate::exit::ExitCode;
use crate::output::{KvEntry, KvRows, KvUnsetRow, Streams, emit, emit_error, write_outcome};

/// The exit code Task 13's own decision table maps each [`KvError`] to.
///
/// `InvalidKey`/`ValueTooLong` are `Usage`: the operator typed it.
/// `FutureVersion`/`Decode` are `InvalidConfig`: the file on disk is the
/// problem, not the command line. `Io`, and any variant this binary
/// predates, has no more specific code than `Failure`.
///
/// `KvError` is `#[non_exhaustive]` (shep-core, IR-20), so the fallback arm
/// is load-bearing, not decoration — matching
/// `From<&shep_client::ConnectError> for ExitCode`'s own precedent
/// (`exit.rs`): a future variant falls to [`ExitCode::Failure`] rather than
/// being guessed at.
fn exit_code_for(err: &KvError) -> ExitCode {
    match err {
        KvError::InvalidKey(_) | KvError::ValueTooLong { .. } => ExitCode::Usage,
        KvError::FutureVersion(_) | KvError::Decode(_) => ExitCode::InvalidConfig,
        // `KvError::Io` and any future variant both land here.
        _ => ExitCode::Failure,
    }
}

/// Renders `err` to `streams.err` and returns the code [`exit_code_for`]
/// maps it to — the one place any of the three verbs below turns a
/// `KvError` into a report.
fn fail(streams: &mut Streams<'_>, fmt: Format, err: &KvError) -> ExitCode {
    let code = exit_code_for(err);
    let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
    code
}

/// `shep set <key> <value>`.
pub fn set(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &KvSetArgs,
) -> ExitCode {
    match kv::set(&paths.kv, &args.key, &args.value) {
        Ok(()) => {
            let row = KvRows(vec![KvEntry {
                key: args.key.clone(),
                value: args.value.clone(),
            }]);
            write_outcome(emit(&mut *streams.out, fmt, "set", row, streams.style))
        }
        Err(err) => fail(streams, fmt, &err),
    }
}

/// `shep get [key]`: one value, or the whole store — newest key order,
/// matching [`kv::all`]'s own `BTreeMap` iteration — with no key given.
///
/// Exits [`ExitCode::NotFound`] for a key the store does not have, writing
/// nothing to `streams.out`: this is what makes `shep get k || echo
/// default` work in a script, the shape this store exists to serve
/// (Task 13's own spec).
pub fn get(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &KvGetArgs,
) -> ExitCode {
    let Some(key) = &args.key else {
        return match kv::all(&paths.kv) {
            Ok(entries) => {
                let rows = KvRows(
                    entries
                        .into_iter()
                        .map(|(key, value)| KvEntry { key, value })
                        .collect(),
                );
                write_outcome(emit(&mut *streams.out, fmt, "get", rows, streams.style))
            }
            Err(err) => fail(streams, fmt, &err),
        };
    };

    match kv::get(&paths.kv, key) {
        Ok(Some(value)) => {
            let row = KvRows(vec![KvEntry {
                key: key.clone(),
                value,
            }]);
            write_outcome(emit(&mut *streams.out, fmt, "get", row, streams.style))
        }
        Ok(None) => {
            let message = format!("`{key}` is not set");
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::NotFound.code_str(),
                &message,
            );
            ExitCode::NotFound
        }
        Err(err) => fail(streams, fmt, &err),
    }
}

/// `shep unset <key>` / `shep unset --all`.
///
/// Exits [`ExitCode::NotFound`] for a key the store does not have — the
/// same "did this actually do anything" honesty [`get`] gives a script,
/// rather than exiting 0 on a no-op an operator would read as success
/// (`shep_core::kv::unset`'s own doc makes the same point about its `bool`).
pub fn unset(
    streams: &mut Streams<'_>,
    fmt: Format,
    paths: &ShepPaths,
    args: &KvUnsetArgs,
) -> ExitCode {
    if args.all {
        return match kv::clear(&paths.kv) {
            Ok(removed) => write_outcome(emit(
                &mut *streams.out,
                fmt,
                "unset",
                KvUnsetRow { removed },
                streams.style,
            )),
            Err(err) => fail(streams, fmt, &err),
        };
    }

    // clap's `required_unless_present = "all"` (cli.rs's `KvUnsetArgs`)
    // guarantees `key.is_some()` on every path that reaches here: the
    // `args.all` arm above already returned for the other one.
    let key = args
        .key
        .as_deref()
        .expect("clap requires a key when --all is not set");
    match kv::unset(&paths.kv, key) {
        Ok(true) => write_outcome(emit(
            &mut *streams.out,
            fmt,
            "unset",
            KvUnsetRow { removed: 1 },
            streams.style,
        )),
        Ok(false) => {
            let message = format!("`{key}` is not set");
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::NotFound.code_str(),
                &message,
            );
            ExitCode::NotFound
        }
        Err(err) => fail(streams, fmt, &err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streams<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Streams<'a> {
        Streams {
            out,
            err,
            style: crate::style::StyleLevel::Bare,
        }
    }

    /// `$SHEP_HOME` pinned to `dir` itself (not a nested `.shep`), so
    /// `paths.kv`'s parent directory already exists — `kv::set`'s staging
    /// file is created via `tempfile_in(parent)`, which does not create a
    /// missing parent.
    fn kv_path(dir: &tempfile::TempDir) -> ShepPaths {
        let home = dir.path().display().to_string();
        shep_core::paths::ShepPaths::resolve(
            &move |key| (key == "SHEP_HOME").then(|| home.clone()),
            dir.path(),
        )
    }

    /// fails if a value set by `set` cannot be read back by `get` — the one
    /// case that says the CLI round-trips through `shep_core::kv` at all.
    #[test]
    fn set_then_get_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = kv_path(&dir);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = set(
            &mut streams(&mut out, &mut err),
            Format::Json,
            &paths,
            &KvSetArgs {
                key: "bark.cooldown".to_string(),
                value: "30s".to_string(),
            },
        );
        assert_eq!(code, ExitCode::Success);

        out.clear();
        let code = get(
            &mut streams(&mut out, &mut err),
            Format::Json,
            &paths,
            &KvGetArgs {
                key: Some("bark.cooldown".to_string()),
            },
        );
        assert_eq!(code, ExitCode::Success);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("bark.cooldown"), "{text}");
        assert!(text.contains("30s"), "{text}");
    }

    /// fails if `get` on a key that was never set exits anything but
    /// `NotFound`, or writes anything to stdout — the script-friendly
    /// contract this store exists to serve (`shep get k || echo default`).
    #[test]
    fn get_on_an_absent_key_exits_not_found_and_writes_nothing_to_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = kv_path(&dir);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = get(
            &mut streams(&mut out, &mut err),
            Format::Json,
            &paths,
            &KvGetArgs {
                key: Some("ghost".to_string()),
            },
        );
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty(), "{out:?}");
    }

    /// fails if `unset --all` leaves anything behind, or fails to report
    /// how many keys it took.
    #[test]
    fn unset_all_empties_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let paths = kv_path(&dir);
        let mut out = Vec::new();
        let mut err = Vec::new();
        set(
            &mut streams(&mut out, &mut err),
            Format::Json,
            &paths,
            &KvSetArgs {
                key: "a".to_string(),
                value: "1".to_string(),
            },
        );
        set(
            &mut streams(&mut out, &mut err),
            Format::Json,
            &paths,
            &KvSetArgs {
                key: "b".to_string(),
                value: "2".to_string(),
            },
        );

        out.clear();
        let code = unset(
            &mut streams(&mut out, &mut err),
            Format::Json,
            &paths,
            &KvUnsetArgs {
                key: None,
                all: true,
            },
        );
        assert_eq!(code, ExitCode::Success);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"removed\":2"), "{text}");
        assert!(kv::all(&paths.kv).unwrap().is_empty());
    }

    /// fails if a key outside the grammar is accepted, or if the store file
    /// is created for a `set` that was refused — `shep_core::kv::check_key`
    /// runs before the lock is ever taken, and this pins that the CLI does
    /// not somehow reach the file first.
    #[test]
    fn a_bad_key_exits_usage_without_creating_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = kv_path(&dir);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = set(
            &mut streams(&mut out, &mut err),
            Format::Json,
            &paths,
            &KvSetArgs {
                key: "not valid".to_string(),
                value: "1".to_string(),
            },
        );
        assert_eq!(code, ExitCode::Usage);
        assert!(
            !paths.kv.exists(),
            "the store must not be created on a refused key"
        );
    }
}
