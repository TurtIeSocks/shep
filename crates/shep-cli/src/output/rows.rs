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
/// Not constructed outside this module's own tests yet: `commands/query.rs`
/// and `commands/lifecycle.rs`, which build one from a real `Response`, are
/// Tasks 8-9. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
#[allow(dead_code)]
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

    const JSON_ONLY: &'static [&'static str] = &[];
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
/// Not constructed outside this module's own tests yet: `commands/query.rs`'s
/// `ping`, which builds one from a real `HelloAck`, is Task 9.
/// `#[allow(dead_code)]` says so explicitly rather than inventing a call
/// site nothing needs yet.
#[allow(dead_code)]
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
            // Both `Option` fields `Some`: `flock_rows_do_not_drift` below
            // serializes this value and diffs its keys against `headers()`,
            // and a `None` field vanishes from the JSON entirely (no key at
            // all) rather than merely rendering empty — the drift test would
            // not see it either way.
            pid: Some(1000 + id),
            restarts: id,
            uptime_ms,
            fold: Some("backend".to_string()),
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

    /// The anti-drift gate, written once and instantiated four times — once
    /// per payload type, per this task's own rule. Serializes a
    /// fully-populated value, collects its JSON object keys, and asserts
    /// they match `headers()` after `json_key_for`, so a field added to
    /// `Serialize` and forgotten in `rows()` fails here rather than silently
    /// vanishing from the table.
    fn assert_no_drift<T: Render>(
        value: &T,
        first_record: fn(&serde_json::Value) -> &serde_json::Value,
    ) {
        let json = serde_json::to_value(value).unwrap();
        let keys: BTreeSet<&str> = first_record(&json)
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
    }

    #[test]
    fn flock_rows_do_not_drift() {
        assert_no_drift(&sample_flock(), |j| &j[0]);
    }

    #[test]
    fn ping_row_does_not_drift() {
        assert_no_drift(
            &PingRow {
                daemon_version: "9.9.9".into(),
                pid: 4242,
            },
            |j| j,
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
        );
    }

    // `DeletedIds` serializes as an array of bare numbers, so it has no
    // object keys to drift; its test is the record-count one below.

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
