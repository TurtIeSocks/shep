//! Renders a [`super::Reading`] as Prometheus text exposition.
//!
//! [`render`] is what `dog::metrics::handle_connection` calls to answer
//! `/metrics`.

use core::fmt::{self, Write as _};

use shep_core::protocol::DogSource;
use shep_core::status::ProcStatus;

use super::Reading;

/// Every [`ProcStatus`] value, in the order `shep_sheep_status` renders
/// them, so a scrape's series order is stable across calls.
const ALL_STATUSES: [ProcStatus; 6] = [
    ProcStatus::Starting,
    ProcStatus::Online,
    ProcStatus::Stopping,
    ProcStatus::Stopped,
    ProcStatus::Errored,
    ProcStatus::WaitingRestart,
];

/// One `# HELP`/`# TYPE` block and the series beneath it.
struct MetricGroup {
    name: &'static str,
    help: &'static str,
    kind: &'static str,
    series: Vec<String>,
}

impl MetricGroup {
    fn new(name: &'static str, help: &'static str, kind: &'static str) -> Self {
        Self {
            name,
            help,
            kind,
            series: Vec::new(),
        }
    }

    /// Appends one series line. `label_str` comes from [`labels`], braces
    /// included, or empty for a label-less metric.
    fn push(&mut self, label_str: &str, value: impl fmt::Display) {
        let name = self.name;
        let _ = writeln!(self.series_line(), "{name}{label_str} {value}");
    }

    fn series_line(&mut self) -> &mut String {
        self.series.push(String::new());
        self.series.last_mut().expect("just pushed")
    }

    /// Renders this group's `# HELP`/`# TYPE` pair and series, or nothing
    /// at all when it has no series.
    fn render_into(&self, out: &mut String) {
        if self.series.is_empty() {
            return;
        }
        let _ = writeln!(out, "# HELP {} {}", self.name, self.help);
        let _ = writeln!(out, "# TYPE {} {}", self.name, self.kind);
        for line in &self.series {
            out.push_str(line);
        }
    }
}

/// Escapes a label value per the Prometheus text exposition format:
/// backslash, double quote and newline are the only three that need it.
/// One pass over the characters, since sequential replaces would
/// double-escape the backslashes an earlier one introduced.
fn escape_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str(r"\\"),
            '"' => escaped.push_str(r#"\""#),
            '\n' => escaped.push_str(r"\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Formats a label list as `{k1="v1",k2="v2"}`, or an empty string for no
/// labels, so [`MetricGroup::push`] formats both cases the same way.
fn labels(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(out, "{key}=\"{}\"", escape_label_value(value));
    }
    out.push('}');
    out
}

/// `DogSource`'s label value. `DogSource` is `#[non_exhaustive]`, so a kind
/// this client predates renders `unknown` rather than failing to build.
fn dog_source_label(source: &DogSource) -> &'static str {
    match source {
        DogSource::BuiltIn => "built-in",
        DogSource::Adopted { .. } => "adopted",
        _ => "unknown",
    }
}

/// Renders `reading` as Prometheus text exposition, format version 0.0.4.
///
/// One `# HELP`/`# TYPE` pair per metric name, every series of a name
/// grouped beneath it, and a trailing newline.
///
/// Label values are escaped (`\\`, `"`, `\n`). A sheep's name is
/// operator-supplied and reaches this function verbatim, so an unescaped
/// quote in one name would corrupt that series' line.
#[must_use]
pub fn render(reading: &Reading) -> String {
    let mut cpu = MetricGroup::new(
        "shep_sheep_cpu_percent",
        "Tree CPU as a percentage of one core, over the last sampling window.",
        "gauge",
    );
    let mut memory = MetricGroup::new(
        "shep_sheep_memory_bytes",
        "Tree resident set size in bytes.",
        "gauge",
    );
    let mut restarts = MetricGroup::new(
        "shep_sheep_restart_total",
        "Restart count since registration.",
        "counter",
    );
    let mut uptime = MetricGroup::new(
        "shep_sheep_uptime_seconds",
        "Seconds since this sheep's last successful start.",
        "gauge",
    );
    let mut status = MetricGroup::new(
        "shep_sheep_status",
        "1 for the status this sheep is currently in, 0 for every other status.",
        "gauge",
    );
    let mut dog_up = MetricGroup::new(
        "shep_dog_up",
        "1 when this dog is online, 0 otherwise.",
        "gauge",
    );
    let mut daemon_up = MetricGroup::new(
        "shep_daemon_up",
        "Always 1: the scrape reached the shepherd.",
        "gauge",
    );
    let mut daemon_pid = MetricGroup::new(
        "shep_daemon_pid",
        "The shepherd's own pid, so a restart is visible as a step change.",
        "gauge",
    );
    let mut host_memory_total = MetricGroup::new(
        "shep_host_memory_total_bytes",
        "Total physical memory on the host.",
        "gauge",
    );
    let mut host_memory_used = MetricGroup::new(
        "shep_host_memory_used_bytes",
        "Memory in use on the host, as the platform reports it.",
        "gauge",
    );
    let mut host_processes = MetricGroup::new(
        "shep_host_processes",
        "Number of processes running on the host, the flock included.",
        "gauge",
    );
    let mut host_uptime = MetricGroup::new(
        "shep_host_uptime_seconds",
        "Seconds since the host booted.",
        "gauge",
    );

    for info in &reading.flock {
        if let Some(source) = &info.dog {
            let pairs = [
                ("dog", info.name.as_str()),
                ("source", dog_source_label(source)),
            ];
            let up = i32::from(info.status == ProcStatus::Online);
            dog_up.push(&labels(&pairs), up);
            continue;
        }

        let fold = info.fold.as_deref().unwrap_or("");
        let id_string = info.id.to_string();
        let sheep_pairs = [
            ("sheep", info.name.as_str()),
            ("id", id_string.as_str()),
            ("fold", fold),
        ];
        let sheep_labels = labels(&sheep_pairs);

        if let Some(cpu_percent) = info.cpu_percent {
            cpu.push(&sheep_labels, cpu_percent);
        }
        if let Some(memory_bytes) = info.memory_bytes {
            memory.push(&sheep_labels, memory_bytes);
        }
        restarts.push(&sheep_labels, info.restarts);
        uptime.push(&sheep_labels, info.uptime_ms / 1000);

        for candidate in ALL_STATUSES {
            let candidate_string = candidate.to_string();
            let status_pairs = [
                ("sheep", info.name.as_str()),
                ("id", id_string.as_str()),
                ("fold", fold),
                ("status", candidate_string.as_str()),
            ];
            let value = i32::from(info.status == candidate);
            status.push(&labels(&status_pairs), value);
        }
    }

    daemon_up.push(&labels(&[("version", reading.daemon_version.as_str())]), 1);
    daemon_pid.push("", reading.daemon_pid);

    if let Some(host) = &reading.host {
        host_memory_total.push("", host.memory_total_bytes);
        host_memory_used.push("", host.memory_used_bytes);
        host_processes.push("", host.processes);
        host_uptime.push("", host.uptime_seconds);
    }

    let mut out = String::new();
    for group in [
        &cpu,
        &memory,
        &restarts,
        &uptime,
        &status,
        &dog_up,
        &daemon_up,
        &daemon_pid,
        &host_memory_total,
        &host_memory_used,
        &host_processes,
        &host_uptime,
    ] {
        group.render_into(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use shep_core::protocol::{DogSource, ProcessInfo};
    use shep_core::status::ProcStatus;

    use super::super::{HostReading, Reading};
    use super::render;

    /// A sheep fixture shared by every test below: id `3`, fold `backend`,
    /// online, with a CPU and memory sample. Both are fixed, since the
    /// assertions spell out `id="3"` and `fold="backend"`.
    fn sample_info(name: &str) -> ProcessInfo {
        ProcessInfo::builder(3, name, ProcStatus::Online)
            .pid(Some(4242))
            .restarts(2)
            .uptime_ms(65_000)
            .fold(Some("backend".to_string()))
            .cpu_percent(Some(1.5))
            .memory_bytes(Some(2048))
            .build()
    }

    /// A baseline [`Reading`] with an empty flock and a host sample. Tests
    /// override what they care about with `..reading()`.
    fn reading() -> Reading {
        Reading {
            flock: vec![],
            daemon_version: "9.9.9".to_string(),
            daemon_pid: 12345,
            host: Some(HostReading {
                memory_total_bytes: 16_000_000_000,
                memory_used_bytes: 8_000_000_000,
                processes: 200,
                uptime_seconds: 3600,
            }),
        }
    }

    #[test]
    fn a_sheep_with_no_reading_contributes_no_series() {
        let mut info = sample_info("web");
        info.cpu_percent = None;
        info.memory_bytes = None;
        let text = render(&Reading {
            flock: vec![info],
            ..reading()
        });
        assert!(!text.contains("shep_sheep_cpu_percent{"));
        assert!(!text.contains("shep_sheep_memory_bytes{"));
        // The counters do not depend on a sample and must still be there.
        assert!(text.contains("shep_sheep_restart_total{"));
    }

    #[test]
    fn status_is_a_label_with_one_series_per_state() {
        let mut info = sample_info("web");
        info.status = ProcStatus::Errored;
        let text = render(&Reading {
            flock: vec![info],
            ..reading()
        });
        assert!(text.contains(
            r#"shep_sheep_status{sheep="web",id="3",fold="backend",status="errored"} 1"#
        ));
        assert!(text.contains(r#"status="online"} 0"#));
    }

    #[test]
    fn a_label_value_is_escaped_so_one_odd_name_cannot_corrupt_the_response() {
        let text = render(&Reading {
            flock: vec![sample_info(r#"we"b\x"#)],
            ..reading()
        });
        // Every line must parse as `name{labels} value`. Only quotes not
        // preceded by a backslash count: a plain quote count is invariant
        // to escaping, so it balances on a broken implementation too.
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
        {
            let mut real_quotes = 0;
            let mut chars = line.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    chars.next(); // escaped char, not a delimiter
                } else if ch == '"' {
                    real_quotes += 1;
                }
            }
            assert_eq!(real_quotes % 2, 0, "unbalanced quotes: {line}");
        }
        assert!(text.contains(r#"sheep="we\"b\\x""#));
    }

    #[test]
    fn a_dog_that_is_down_reports_zero_rather_than_nothing() {
        let mut dead = sample_info("bark");
        dead.status = ProcStatus::Errored;
        dead.dog = Some(DogSource::BuiltIn);
        let text = render(&Reading {
            flock: vec![dead],
            ..reading()
        });
        assert!(text.contains(r#"shep_dog_up{dog="bark",source="built-in"} 0"#));
        assert!(
            !text.contains(r#"shep_sheep_status{sheep="bark""#),
            "a dog is not reported as a sheep"
        );
    }

    #[test]
    fn every_metric_name_carries_one_help_one_type_and_contiguous_series() {
        let mut dog = sample_info("bark");
        dog.dog = Some(DogSource::BuiltIn);
        let mut idle = sample_info("worker");
        idle.cpu_percent = None;
        idle.memory_bytes = None;
        let text = render(&Reading {
            flock: vec![sample_info("web"), idle, dog],
            ..reading()
        });
        let lines: Vec<&str> = text.lines().collect();

        let names = [
            "shep_sheep_cpu_percent",
            "shep_sheep_memory_bytes",
            "shep_sheep_restart_total",
            "shep_sheep_uptime_seconds",
            "shep_sheep_status",
            "shep_dog_up",
            "shep_daemon_up",
            "shep_daemon_pid",
            "shep_host_memory_total_bytes",
            "shep_host_memory_used_bytes",
            "shep_host_processes",
            "shep_host_uptime_seconds",
        ];

        let is_series_for = |line: &str, name: &str| {
            line.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('{') || rest.starts_with(' '))
        };

        for name in names {
            let help_count = lines
                .iter()
                .filter(|l| l.starts_with(&format!("# HELP {name} ")))
                .count();
            let type_count = lines
                .iter()
                .filter(|l| l.starts_with(&format!("# TYPE {name} ")))
                .count();
            assert_eq!(help_count, 1, "{name} must carry exactly one HELP line");
            assert_eq!(type_count, 1, "{name} must carry exactly one TYPE line");

            let series_indices: Vec<usize> = lines
                .iter()
                .enumerate()
                .filter(|(_, l)| is_series_for(l, name))
                .map(|(i, _)| i)
                .collect();
            assert!(
                !series_indices.is_empty(),
                "{name} must have at least one series in this fixture"
            );
            let first = *series_indices.first().unwrap();
            let last = *series_indices.last().unwrap();
            assert_eq!(
                last - first + 1,
                series_indices.len(),
                "{name}'s series are not contiguous: {series_indices:?}"
            );
        }
        assert!(text.ends_with('\n'), "exposition must end with a newline");
    }

    #[test]
    fn escape_label_value_handles_backslash_quote_and_newline() {
        assert_eq!(super::escape_label_value("plain"), "plain");
        assert_eq!(super::escape_label_value(r#"a"b"#), r#"a\"b"#);
        assert_eq!(super::escape_label_value(r"a\b"), r"a\\b");
        assert_eq!(super::escape_label_value("a\nb"), r"a\nb");
        assert_eq!(super::escape_label_value(r#"we"b\x"#), r#"we\"b\\x"#);
    }
}
