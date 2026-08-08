//! Every rendered payload type in the binary, and the [`Render`] impl that
//! makes each one's table and JSON renderings the same source of truth.
//!
//! Payload types live here, not under `commands/`, and that is load-bearing
//! rather than tidy: this module is pure tier and its own tests (below) name
//! every one of these types directly. A payload type defined under
//! `commands/` (`#[cfg(unix)]`) could not be named by a test running on the
//! Windows leg at all, and `commands/query.rs` (a later task) does not exist
//! yet for a test here to depend on regardless. Every type below is built
//! entirely from `ProcessInfo` / `u32`, and shep-core carries no `cfg` of any
//! kind, so this really is pure tier.

use serde::Serialize;
use shep_core::protocol::ProcessInfo;

use super::Render;

/// `Vec<ProcessInfo>` for `flock`, `describe`, `fold`, `start`, `stop`,
/// `restart`. A newtype because `ProcessInfo` is shep-core's and the orphan
/// rule forbids implementing our `Render` on it directly. `transparent` so
/// the JSON is a plain array of `ProcessInfo`, not a wrapper object.
///
/// Constructed by `commands/query.rs`'s `flock`/`describe_selector` and
/// `commands/lifecycle.rs`'s `start`/`stop`/`restart`, each from a real
/// `Response`.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct FlockRows(pub Vec<ProcessInfo>);

impl Render for FlockRows {
    fn headers() -> &'static [&'static str] {
        &["ID", "NAME", "STATUS", "PID", "RESTARTS", "UPTIME", "FOLD"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0
            .iter()
            .map(|p| {
                vec![
                    p.id.to_string(),
                    p.name.clone(),
                    p.status.to_string(),
                    // `-` rather than an empty cell: an empty cell in a
                    // padded table is indistinguishable from a rendering bug.
                    p.pid.map_or_else(|| "-".to_string(), |pid| pid.to_string()),
                    p.restarts.to_string(),
                    super::human_duration(p.uptime_ms),
                    p.fold.clone().unwrap_or_else(|| "-".to_string()),
                ]
            })
            .collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            "NAME" => "name",
            "STATUS" => "status",
            "PID" => "pid",
            "RESTARTS" => "restarts",
            "UPTIME" => "uptime_ms",
            "FOLD" => "fold",
            other => panic!("FlockRows::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[
        // Absolute log paths, often longer than every other column put
        // together — a column here would wreck the table `flock` exists to
        // print. They ride the JSON so a programmatic consumer can find a
        // sheep's logs without re-deriving paths the daemon alone resolves.
        "out_file", "err_file",
    ];
}

/// `Response::Deleted(Vec<u32>)` — the ids that were removed.
///
/// Not constructed outside this module's own tests yet: `commands/
/// lifecycle.rs`'s `delete`, which builds one from a real `Response`, is
/// Task 8. `#[allow(dead_code)]` says so explicitly rather than inventing a
/// call site nothing needs yet.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct DeletedIds(pub Vec<u32>);

impl Render for DeletedIds {
    fn headers() -> &'static [&'static str] {
        &["ID"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.0.iter().map(|id| vec![id.to_string()]).collect()
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "ID" => "id",
            other => panic!("DeletedIds::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `ping`: the daemon identity the handshake already told us.
///
/// Constructed by `commands/query.rs`'s `ping`, from the real `HelloAck`
/// `Client::daemon` holds.
#[derive(Debug, Serialize)]
pub struct PingRow {
    /// Daemon crate version, read off the handshake `HelloAck`.
    pub daemon_version: String,
    /// Daemon pid, from the same handshake.
    pub pid: u32,
}

impl Render for PingRow {
    fn headers() -> &'static [&'static str] {
        &["DAEMON_VERSION", "PID"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.daemon_version.clone(), self.pid.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "DAEMON_VERSION" => "daemon_version",
            "PID" => "pid",
            other => panic!("PingRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

/// `kill`: what teardown actually achieved.
///
/// Not constructed outside this module's own tests yet: `commands/admin.rs`'s
/// `kill`, which builds one after tearing the daemon down, is Task 11.
/// `#[allow(dead_code)]` says so explicitly rather than inventing a call
/// site nothing needs yet.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct KillRow {
    /// Daemon pid at the moment of kill, read before the connection dropped.
    pub pid: u32,
    /// Whether the daemon removed its own socket file before exiting.
    pub socket_removed: bool,
}

impl Render for KillRow {
    fn headers() -> &'static [&'static str] {
        &["PID", "SOCKET_REMOVED"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![self.pid.to_string(), self.socket_removed.to_string()]]
    }

    /// # Panics
    /// If `header` is not one of `Self::headers()`'s own values.
    #[track_caller]
    fn json_key_for(header: &str) -> &'static str {
        match header {
            "PID" => "pid",
            "SOCKET_REMOVED" => "socket_removed",
            other => panic!("KillRow::headers() does not include {other:?}"),
        }
    }

    const JSON_ONLY: &'static [&'static str] = &[];
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeSet;

    use shep_core::status::ProcStatus;

    use super::*;

    fn sample_info(id: u32, name: &str, uptime_ms: u64) -> ProcessInfo {
        ProcessInfo {
            id,
            name: name.to_string(),
            status: ProcStatus::Online,
            // Every `Option` field `Some`: `flock_rows_do_not_drift` below
            // serializes this value and diffs its keys against `headers()`,
            // and a `None` field vanishes from the JSON entirely (no key at
            // all) rather than merely rendering empty — the drift test would
            // not see it either way.
            pid: Some(1000 + id),
            restarts: id,
            uptime_ms,
            fold: Some("backend".to_string()),
            out_file: Some(format!("/logs/{name}-0-out.log")),
            err_file: Some(format!("/logs/{name}-0-err.log")),
        }
    }

    /// Three fully-populated sheep, shared by every test in this module and
    /// by `output`'s own envelope/emit tests.
    pub(crate) fn sample_flock() -> FlockRows {
        FlockRows(vec![
            sample_info(1, "web", 60_000),
            sample_info(2, "worker", 120_000),
            sample_info(3, "cron", 30_000),
        ])
    }

    pub(crate) fn info_with_uptime_ms(uptime_ms: u64) -> ProcessInfo {
        sample_info(1, "web", uptime_ms)
    }

    /// The anti-drift gate, written once and instantiated three times — once
    /// per payload type with JSON object keys (`DeletedIds` has none — see
    /// its own test below), per this task's own rule.
    ///
    /// Three checks, each catching a mutation the other two miss:
    /// 1. Serializes a fully-populated value, collects its JSON object keys,
    ///    and asserts they match `headers()` after `json_key_for`, so a
    ///    field added to `Serialize` and forgotten in `rows()` fails here
    ///    rather than silently vanishing from the table.
    /// 2. Every row's cell count must equal `headers().len()` — a dropped or
    ///    added cell shifts every later column without changing the row
    ///    *count*, which `table_and_json_report_the_same_record_count`
    ///    checks but this doesn't.
    /// 3. The first row's cell for each non-`formatted` header is pinned
    ///    against that same field's own JSON value — a cell-count check
    ///    alone cannot see two same-arity cells swapped (e.g. NAME and
    ///    STATUS trading places).
    ///
    /// `formatted` lists headers whose table cell is a human-only rendering
    /// of the field rather than the field's raw value (`FlockRows`'s
    /// `UPTIME`, formatted by `human_duration` — see `table.rs`'s own tests
    /// for that formatting's coverage instead). Comparing those cells
    /// against the raw JSON value would either duplicate that formatting
    /// here or spuriously fail; every other header IS compared cell-for-cell,
    /// which is what actually catches a swap.
    fn assert_no_drift<T: Render>(
        value: &T,
        first_record: fn(&serde_json::Value) -> &serde_json::Value,
        formatted: &[&str],
    ) {
        let json = serde_json::to_value(value).unwrap();
        let record = first_record(&json);
        let keys: BTreeSet<&str> = record
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();

        let covered: BTreeSet<&str> = T::headers()
            .iter()
            .map(|h| T::json_key_for(h))
            .chain(T::JSON_ONLY.iter().copied())
            .collect();

        assert_eq!(
            keys, covered,
            "a serialized field is a column, or it is in JSON_ONLY with a reason — never neither"
        );

        let rows = value.rows();
        for row in &rows {
            assert_eq!(
                row.len(),
                T::headers().len(),
                "a row has {} cells but headers() has {} — a dropped or added cell changes no \
                 row *count*, so table_and_json_report_the_same_record_count would miss it",
                row.len(),
                T::headers().len(),
            );
        }

        let Some(row) = rows.first() else {
            return;
        };
        for (i, header) in T::headers().iter().enumerate() {
            if formatted.contains(header) {
                continue;
            }
            let key = T::json_key_for(header);
            let expected = match &record[key] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                // Not exercised by today's fully-populated fixtures (see
                // `sample_info`'s own comment on why every `Option` is
                // `Some`); skipped rather than panicking so a future
                // `None`-carrying fixture doesn't fail here for an unrelated
                // reason.
                serde_json::Value::Null => continue,
                other => panic!(
                    "{header} ({key}) serialized to {other:?}; teach this match how to \
                     stringify it, or add {header} to `formatted`"
                ),
            };
            assert_eq!(
                row[i], expected,
                "{header} cell does not match its own JSON field {key:?} — swapped or \
                 substituted with a neighbouring column?"
            );
        }
    }

    #[test]
    fn flock_rows_do_not_drift() {
        // UPTIME is formatted (`human_duration`), not a raw echo of
        // `uptime_ms` — see the doc comment on `assert_no_drift` above.
        assert_no_drift(&sample_flock(), |j| &j[0], &["UPTIME"]);
    }

    #[test]
    fn ping_row_does_not_drift() {
        assert_no_drift(
            &PingRow {
                daemon_version: "9.9.9".into(),
                pid: 4242,
            },
            |j| j,
            &[],
        );
    }

    #[test]
    fn kill_row_does_not_drift() {
        assert_no_drift(
            &KillRow {
                pid: 4242,
                socket_removed: true,
            },
            |j| j,
            &[],
        );
    }

    /// `DeletedIds` is `#[serde(transparent)]` over `Vec<u32>`, so it
    /// serializes as a bare JSON array of numbers with no object keys —
    /// `assert_no_drift`'s key-set comparison has nothing to compare
    /// against, and `json_key_for("ID") -> "id"` names a key that never
    /// exists in this type's JSON at all. This test is `DeletedIds`'s drift
    /// coverage instead: it pins each row's one cell against the array
    /// element at the same position, so a `rows()` that dropped, reordered,
    /// or mis-rendered an id still fails.
    #[test]
    fn deleted_ids_rows_match_their_own_json_values() {
        let ids = DeletedIds(vec![10, 20, 30]);
        let json = serde_json::to_value(&ids).unwrap();
        let array = json.as_array().unwrap();
        let rows = ids.rows();

        assert_eq!(rows.len(), array.len());
        for (row, value) in rows.iter().zip(array) {
            assert_eq!(row.len(), 1, "DeletedIds::headers() has exactly one column");
            assert_eq!(row[0], value.to_string());
        }
    }

    #[test]
    fn table_and_json_report_the_same_record_count() {
        let rows = sample_flock(); // three sheep
        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 3);
        assert_eq!(
            rows.rows().len(),
            3,
            "the two renderings must never disagree on how many records exist"
        );

        let ids = DeletedIds(vec![1, 2, 3, 4]);
        assert_eq!(
            serde_json::to_value(&ids)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(ids.rows().len(), 4);
    }
}
