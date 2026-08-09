//! `ProbeTarget` — parsing a probe's `target` once, at config time
//!
//! `ProbeConfig::target` is free-form text whose grammar depends on
//! `ProbeConfig::kind`: an `http://` URL, a `host:port` pair, or a shell
//! command line. Parsing it here, at Flockfile-normalize time, means a
//! malformed target fails `shep start` with a message naming the Flockfile
//! field it came from — not the daemon's first poll ten seconds after the
//! sheep comes online, which is where an unparsed target would otherwise
//! surface.
//!
//! No URL crate: the grammar `http://host[:port][/path]` needs no userinfo,
//! query, fragment, IDN, or percent-decoding, so a hand-rolled split covers
//! it without pulling `url` — and the `idna`/Unicode tables it drags in —
//! into a daemon whose stated goal is single-digit-MB RSS.

use core::fmt;

use crate::config::app::{ProbeConfig, ProbeKind};

/// A probe's `target` after validation — the form the prober consumes.
///
/// Parsing here rather than in the daemon means a malformed target fails the
/// Flockfile, not the first poll ten seconds after the sheep is online.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTarget {
    /// `http://host[:port]/path` — port defaults to 80, path to `/`.
    Http {
        /// The host or IP literal. A bracketed IPv6 literal (`[::1]`) has
        /// its brackets stripped.
        host: String,
        /// The port; defaults to 80 when the authority carries none.
        port: u16,
        /// The path; defaults to `/` when the URL carries none.
        path: String,
    },
    /// `host:port`.
    Tcp {
        /// The host or IP literal.
        host: String,
        /// The port.
        port: u16,
    },
    /// A command line, run through the platform shell.
    Exec {
        /// The command line exactly as written in the Flockfile.
        command: String,
    },
}

impl ProbeTarget {
    /// Parses `config.target` according to `config.kind`.
    ///
    /// # Errors
    ///
    /// - [`ProbeTargetError::Empty`] — the target is empty or all whitespace.
    /// - [`ProbeTargetError::HttpsUnsupported`] — an `https://` URL.
    /// - [`ProbeTargetError::NotHttpUrl`] — no `http://` scheme.
    /// - [`ProbeTargetError::MissingHost`] — the authority has no host.
    /// - [`ProbeTargetError::MissingPort`] — a TCP target with no `:port`.
    /// - [`ProbeTargetError::BadPort`] — the port is not a `u16`.
    pub fn parse(config: &ProbeConfig) -> Result<Self, ProbeTargetError> {
        if config.target.trim().is_empty() {
            return Err(ProbeTargetError::Empty);
        }
        match config.kind {
            ProbeKind::Http => parse_http(&config.target),
            ProbeKind::Tcp => parse_tcp(&config.target),
            ProbeKind::Exec => Ok(Self::Exec {
                command: config.target.clone(),
            }),
        }
    }
}

/// Why a probe target was rejected.
///
/// Growth is expected: a future `https` probe removes one variant's reason for
/// existing and a unix-socket probe would add several (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeTargetError {
    /// The target is empty or all whitespace.
    Empty,
    /// An `https://` URL. TLS probe targets are not supported.
    HttpsUnsupported {
        /// The target as written in the Flockfile.
        target: String,
    },
    /// An HTTP probe target with no `http://` scheme.
    NotHttpUrl {
        /// The target as written in the Flockfile.
        target: String,
    },
    /// The authority is empty — `http:///path`.
    MissingHost {
        /// The target as written in the Flockfile.
        target: String,
    },
    /// A TCP target with no `:port`.
    MissingPort {
        /// The target as written in the Flockfile.
        target: String,
    },
    /// The port is not a `u16`.
    BadPort {
        /// The target as written in the Flockfile.
        target: String,
    },
}

impl fmt::Display for ProbeTargetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("probe target is empty"),
            Self::HttpsUnsupported { target } => write!(
                f,
                "probe target `{target}` uses https://, which shep's probe client does not \
                 support (no TLS)"
            ),
            Self::NotHttpUrl { target } => {
                write!(f, "probe target `{target}` is not an http:// URL")
            }
            Self::MissingHost { target } => write!(f, "probe target `{target}` has no host"),
            Self::MissingPort { target } => write!(f, "probe target `{target}` has no port"),
            Self::BadPort { target } => {
                write!(
                    f,
                    "probe target `{target}` has a port that is not a valid u16"
                )
            }
        }
    }
}

impl core::error::Error for ProbeTargetError {}

/// Parses an `http://` target into host, port and path.
fn parse_http(target: &str) -> Result<ProbeTarget, ProbeTargetError> {
    let Some(rest) = target.strip_prefix("http://") else {
        if target.starts_with("https://") {
            return Err(ProbeTargetError::HttpsUnsupported {
                target: target.to_string(),
            });
        }
        return Err(ProbeTargetError::NotHttpUrl {
            target: target.to_string(),
        });
    };

    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(ProbeTargetError::MissingHost {
            target: target.to_string(),
        });
    }

    let (host, port_str) = split_authority(authority, target)?;
    if host.is_empty() {
        return Err(ProbeTargetError::MissingHost {
            target: target.to_string(),
        });
    }
    let port = parse_port(port_str, target)?;

    Ok(ProbeTarget::Http {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// Splits an HTTP authority into `(host, port)`, the port left as text
/// (defaulted to `"80"` when absent) for [`parse_port`] to finish.
///
/// Bracketed IPv6 (`[::1]:8080`) is matched before the general "split on the
/// last colon" rule runs. Splitting `[::1]:8080` on the *last* colon alone
/// puts `1]` in the host and `8080` in the port; splitting on the *first*
/// puts an empty host and `:1]:8080` in the port. Neither is right, so the
/// bracket is checked for explicitly and the colon search happens only
/// inside it.
fn split_authority<'a>(
    authority: &'a str,
    target: &str,
) -> Result<(&'a str, &'a str), ProbeTargetError> {
    if let Some(inner) = authority.strip_prefix('[') {
        // A missing closing bracket leaves no host to extract; report it the
        // same way an empty authority is reported.
        let close = inner
            .find(']')
            .ok_or_else(|| ProbeTargetError::MissingHost {
                target: target.to_string(),
            })?;
        let host = &inner[..close];
        let after = &inner[close + 1..];
        let port_str = match after.strip_prefix(':') {
            Some(p) => p,
            None if after.is_empty() => "80",
            // Trailing characters after `]` that are neither `:port` nor
            // nothing — e.g. `[::1]x` — have no valid port to report.
            None => {
                return Err(ProbeTargetError::BadPort {
                    target: target.to_string(),
                });
            }
        };
        return Ok((host, port_str));
    }
    match authority.rsplit_once(':') {
        Some((host, port_str)) => Ok((host, port_str)),
        None => Ok((authority, "80")),
    }
}

/// Parses a TCP `host:port` target.
fn parse_tcp(target: &str) -> Result<ProbeTarget, ProbeTargetError> {
    let Some((host, port_str)) = target.rsplit_once(':') else {
        return Err(ProbeTargetError::MissingPort {
            target: target.to_string(),
        });
    };
    if host.is_empty() {
        return Err(ProbeTargetError::MissingHost {
            target: target.to_string(),
        });
    }
    if port_str.is_empty() {
        return Err(ProbeTargetError::MissingPort {
            target: target.to_string(),
        });
    }
    let port = parse_port(port_str, target)?;
    Ok(ProbeTarget::Tcp {
        host: host.to_string(),
        port,
    })
}

/// Parses a port string into a `u16`, reporting the original target (not
/// just the port substring) so the error names the line the user has to edit.
fn parse_port(port_str: &str, target: &str) -> Result<u16, ProbeTargetError> {
    port_str
        .parse::<u16>()
        .map_err(|_| ProbeTargetError::BadPort {
            target: target.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::UpDuration;

    fn probe_config(kind: ProbeKind, target: &str) -> ProbeConfig {
        ProbeConfig {
            kind,
            target: target.to_string(),
            interval: UpDuration::from_millis(10_000),
            timeout: UpDuration::from_millis(5_000),
            failure_threshold: 3,
        }
    }

    #[test]
    fn empty_target_rejected_for_every_kind() {
        // fails if the empty check lives inside one kind's parser instead of
        // running before the kind dispatch — in particular, if Exec's "only
        // emptiness is rejected" carve-out is implemented by skipping the
        // check entirely rather than by skipping every OTHER check
        for kind in [ProbeKind::Http, ProbeKind::Tcp, ProbeKind::Exec] {
            assert_eq!(
                ProbeTarget::parse(&probe_config(kind, "")).unwrap_err(),
                ProbeTargetError::Empty
            );
            assert_eq!(
                ProbeTarget::parse(&probe_config(kind, "   ")).unwrap_err(),
                ProbeTargetError::Empty
            );
        }
    }

    #[test]
    fn http_full_url_with_port_and_path_accepted() {
        // fails if authority/path splitting is off by one around the first `/`
        let target = ProbeTarget::parse(&probe_config(
            ProbeKind::Http,
            "http://127.0.0.1:8080/healthz",
        ))
        .unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "127.0.0.1".to_string(),
                port: 8080,
                path: "/healthz".to_string(),
            }
        );
    }

    #[test]
    fn http_missing_port_defaults_to_80_and_path_defaults_to_root() {
        // fails if the no-colon authority branch forgets to default the port
        let target =
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://localhost/")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "localhost".to_string(),
                port: 80,
                path: "/".to_string(),
            }
        );
    }

    #[test]
    fn http_missing_path_defaults_to_root() {
        // fails if the no-slash case (rest.find('/') == None) is not handled,
        // e.g. by treating the whole remainder as the authority and leaving
        // the path empty instead of "/"
        let target =
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://localhost:3000")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "localhost".to_string(),
                port: 3000,
                path: "/".to_string(),
            }
        );
    }

    #[test]
    fn http_bracketed_ipv6_with_port_and_path_accepted() {
        // fails if the host/port split uses a plain rsplit_once(':') without
        // checking for the bracket first — that would split "[::1]:8080"
        // into host "1]" (wrong) or worse
        let target =
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://[::1]:8080/x")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "::1".to_string(),
                port: 8080,
                path: "/x".to_string(),
            }
        );
    }

    #[test]
    fn http_bracketed_ipv6_without_port_defaults_to_80() {
        // fails if the bracket branch requires a following `:port` instead of
        // treating "nothing after the bracket" as "default port"
        let target = ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://[::1]/x")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "::1".to_string(),
                port: 80,
                path: "/x".to_string(),
            }
        );
    }

    #[test]
    fn https_scheme_rejected_as_unsupported() {
        // fails if https:// is treated as a generic "not http" failure
        // instead of its own named variant
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "https://x/")).unwrap_err(),
            ProbeTargetError::HttpsUnsupported {
                target: "https://x/".to_string()
            }
        );
    }

    #[test]
    fn scheme_missing_rejected_as_not_http_url() {
        // fails if a target with no recognized scheme at all is mishandled
        // (e.g. parsed as if "http://" were implied)
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "x/")).unwrap_err(),
            ProbeTargetError::NotHttpUrl {
                target: "x/".to_string()
            }
        );
    }

    #[test]
    fn non_http_scheme_rejected_as_not_http_url() {
        // fails if scheme detection only checks for the ABSENCE of "http://"
        // rather than also rejecting a different, unrelated scheme
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "ftp://x/")).unwrap_err(),
            ProbeTargetError::NotHttpUrl {
                target: "ftp://x/".to_string()
            }
        );
    }

    #[test]
    fn empty_authority_rejected_as_missing_host() {
        // fails if an empty authority ("http:///path") is handed to the
        // colon-splitting logic instead of being caught first, which would
        // report a confusing BadPort or silently produce an empty host
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http:///path")).unwrap_err(),
            ProbeTargetError::MissingHost {
                target: "http:///path".to_string()
            }
        );
    }

    #[test]
    fn non_numeric_port_rejected_as_bad_port() {
        // fails if the port substring is stored as-is without ever being
        // parsed into a u16
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://host:notaport/"))
                .unwrap_err(),
            ProbeTargetError::BadPort {
                target: "http://host:notaport/".to_string()
            }
        );
    }

    #[test]
    fn port_out_of_u16_range_rejected_as_bad_port() {
        // fails if the port is parsed as a wider integer type (e.g. u32) and
        // then truncated/cast into u16 instead of rejected
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://host:99999/")).unwrap_err(),
            ProbeTargetError::BadPort {
                target: "http://host:99999/".to_string()
            }
        );
    }

    #[test]
    fn tcp_host_and_port_accepted() {
        let target = ProbeTarget::parse(&probe_config(ProbeKind::Tcp, "db.internal:5432")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Tcp {
                host: "db.internal".to_string(),
                port: 5432,
            }
        );
    }

    #[test]
    fn tcp_no_colon_rejected_as_missing_port() {
        // fails if a TCP target with no colon at all is mistaken for a bare
        // hostname with a default port, instead of being rejected — TCP has
        // no default port
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Tcp, "host")).unwrap_err(),
            ProbeTargetError::MissingPort {
                target: "host".to_string()
            }
        );
    }

    #[test]
    fn tcp_trailing_colon_rejected_as_missing_port() {
        // fails if an empty port substring after the colon is parsed as "0"
        // or otherwise accepted instead of rejected
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Tcp, "host:")).unwrap_err(),
            ProbeTargetError::MissingPort {
                target: "host:".to_string()
            }
        );
    }

    #[test]
    fn tcp_missing_host_rejected() {
        // fails if an empty host substring before the colon is accepted as a
        // literal empty hostname instead of rejected
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Tcp, ":8080")).unwrap_err(),
            ProbeTargetError::MissingHost {
                target: ":8080".to_string()
            }
        );
    }

    #[test]
    fn exec_arbitrary_command_line_accepted_unmodified() {
        // fails if Exec narrows the accepted grammar beyond emptiness — e.g.
        // rejecting shell metacharacters or splitting on whitespace
        let command = "sh -c 'curl -f http://localhost/ || exit 1'";
        let target = ProbeTarget::parse(&probe_config(ProbeKind::Exec, command)).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Exec {
                command: command.to_string()
            }
        );
    }
}
