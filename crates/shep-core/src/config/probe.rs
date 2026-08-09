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
        ///
        /// Carried for Task 8 (the prober): connect with
        /// `(host.as_str(), port)`, not by formatting `"{host}:{port}"`
        /// into a `SocketAddr` parse — a stripped IPv6 literal has no
        /// brackets to make that string parseable. For the RFC 7230
        /// `Host:` header, re-bracket an IPv6 host (`format!("[{host}]")`)
        /// before writing it.
        host: String,
        /// The port; defaults to 80 when the authority carries none.
        port: u16,
        /// The path; defaults to `/` when the URL carries none.
        ///
        /// Free of whitespace and control characters whenever it came from
        /// [`ProbeTarget::parse`] — see [`ProbeTargetError::InvalidPath`].
        /// The prober writes this verbatim into a request line, so that is a
        /// security property, not a tidiness one; a caller that builds this
        /// variant by hand rather than parsing takes it on itself.
        path: String,
    },
    /// `host:port`.
    Tcp {
        /// The host or IP literal. A bracketed IPv6 literal (`[::1]`) has
        /// its brackets stripped.
        ///
        /// Carried for Task 8 (the prober): connect with
        /// `(host.as_str(), port)`, not by formatting `"{host}:{port}"`
        /// into a `SocketAddr` parse — a stripped IPv6 literal has no
        /// brackets to make that string parseable.
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
    /// - [`ProbeTargetError::InvalidHost`] — the host contains `@`,
    ///   whitespace, or an embedded `:`.
    /// - [`ProbeTargetError::InvalidPath`] — the path contains whitespace or
    ///   a control character.
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
    /// The host contains a character the grammar has no field for: `@`
    /// (userinfo), whitespace, or — outside a bracketed IPv6 literal, where
    /// a colon is part of the address itself — an embedded `:` (a sign the
    /// authority carried more than one `host:port` pair).
    InvalidHost {
        /// The target as written in the Flockfile.
        target: String,
    },
    /// The path contains whitespace or a control character. Both break the
    /// request line the prober builds around it: a `\r\n` appends
    /// Flockfile-chosen headers — or an entire second request — to what goes
    /// on the socket, and a space ends the path field early.
    InvalidPath {
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
            Self::InvalidHost { target } => write!(
                f,
                "probe target `{target}` has a host containing `@`, whitespace, or an embedded \
                 `:`"
            ),
            Self::InvalidPath { target } => write!(
                f,
                "probe target `{target}` has a path containing whitespace or a control character"
            ),
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
    // The empty check in `ProbeTarget::parse` trims before deciding whether
    // there's anything here at all; the scheme match trims too, so
    // `"  http://host/  "` is accepted the same as `"http://host/"` instead
    // of falling through to `NotHttpUrl`.
    let trimmed = target.trim();
    let Some(rest) = strip_prefix_ignore_ascii_case(trimmed, "http://") else {
        if strip_prefix_ignore_ascii_case(trimmed, "https://").is_some() {
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
    validate_host(host, authority, target)?;
    validate_path(path, target)?;
    let port = parse_port(port_str.unwrap_or("80"), target)?;

    Ok(ProbeTarget::Http {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

/// Splits an authority into `(host, port)`. The port is `None` when the
/// authority carries none, leaving "default it" (HTTP, to 80) versus
/// "require it" (TCP, [`ProbeTargetError::MissingPort`]) to the caller —
/// shared by [`parse_http`] and [`parse_tcp`] so both schemes agree on what
/// a bracketed IPv6 host looks like.
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
) -> Result<(&'a str, Option<&'a str>), ProbeTargetError> {
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
            Some(p) => Some(p),
            None if after.is_empty() => None,
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
        Some((host, port_str)) => Ok((host, Some(port_str))),
        None => Ok((authority, None)),
    }
}

/// Rejects a host containing `@` (a userinfo the grammar has no field for),
/// whitespace, or — outside a bracketed IPv6 literal, where a colon is part
/// of the address itself — an embedded `:` (a sign the authority carried
/// more than one `host:port` pair, e.g. `host:8080:9090`). All three parse
/// as a syntactically fine host and then simply never resolve at poll time.
fn validate_host(host: &str, authority: &str, target: &str) -> Result<(), ProbeTargetError> {
    let bracketed = authority.starts_with('[');
    let invalid = host.contains('@')
        || host.chars().any(char::is_whitespace)
        || (!bracketed && host.contains(':'));
    if invalid {
        return Err(ProbeTargetError::InvalidHost {
            target: target.to_string(),
        });
    }
    Ok(())
}

/// Rejects a path containing whitespace or a control character.
///
/// The prober writes this path verbatim into `GET {path} HTTP/1.1\r\n…`, so
/// a `\r\n` inside it is header injection: everything after it arrives at the
/// server as further headers, and a long enough payload smuggles a whole
/// second request. A space is the same defect one field earlier — it ends the
/// request line's path and hands the server whatever follows as the HTTP
/// version.
///
/// Nothing legitimate is lost. RFC 3986 has no spelling for either character
/// inside a path: a space is written `%20`, and a control character is
/// percent-encoded too.
fn validate_path(path: &str, target: &str) -> Result<(), ProbeTargetError> {
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(ProbeTargetError::InvalidPath {
            target: target.to_string(),
        });
    }
    Ok(())
}

/// Case-insensitively strips `prefix` from the start of `s`, so `HTTPS://`
/// is recognized the same as `https://` (schemes are case-insensitive per
/// RFC 3986 §3.1). `s.get(..prefix.len())` rather than a byte-index slice:
/// a multi-byte character straddling that boundary would otherwise panic.
fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then_some(&s[prefix.len()..])
}

/// Parses a TCP `host:port` target — the whole target is the authority,
/// since a TCP target carries no scheme or path.
fn parse_tcp(target: &str) -> Result<ProbeTarget, ProbeTargetError> {
    let (host, port_str) = split_authority(target, target)?;
    if host.is_empty() {
        return Err(ProbeTargetError::MissingHost {
            target: target.to_string(),
        });
    }
    validate_host(host, target, target)?;
    let Some(port_str) = port_str else {
        return Err(ProbeTargetError::MissingPort {
            target: target.to_string(),
        });
    };
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
        let err = ProbeTarget::parse(&probe_config(ProbeKind::Http, "https://x/")).unwrap_err();
        assert_eq!(
            err,
            ProbeTargetError::HttpsUnsupported {
                target: "https://x/".to_string()
            }
        );
        // fails if the settled "must say why" decision regresses to a
        // generic message — variant identity alone doesn't guard the text.
        assert!(err.to_string().contains("no TLS"), "{err}");
    }

    #[test]
    fn https_scheme_matched_case_insensitively() {
        // fails if scheme matching is case-sensitive: `HTTPS://` would then
        // fall through to the generic NotHttpUrl, losing the TLS-specific
        // explanation the settled decision requires
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "HTTPS://x/")).unwrap_err(),
            ProbeTargetError::HttpsUnsupported {
                target: "HTTPS://x/".to_string()
            }
        );
    }

    #[test]
    fn http_scheme_matched_case_insensitively() {
        // fails if scheme matching is case-sensitive: `HTTP://` would then
        // be rejected as NotHttpUrl instead of parsed
        let target = ProbeTarget::parse(&probe_config(ProbeKind::Http, "HTTP://host/x")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "host".to_string(),
                port: 80,
                path: "/x".to_string(),
            }
        );
    }

    #[test]
    fn surrounding_whitespace_trimmed_before_scheme_match() {
        // fails if only the empty-check trims (config.target.trim().is_empty())
        // while the scheme matcher runs on the untrimmed target — that
        // combination accepts "" but rejects "  http://host/  " as
        // NotHttpUrl, even though both are just whitespace-padded
        let target =
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "  http://host/  ")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "host".to_string(),
                port: 80,
                path: "/".to_string(),
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
    fn http_userinfo_in_host_rejected_as_invalid_host() {
        // fails if the host/port split's "last colon" rule is trusted at
        // face value: "user:pass@host:8080" would otherwise parse to host
        // "user:pass@host", a userinfo prefix the grammar has no field for
        // and that will never resolve at poll time
        assert_eq!(
            ProbeTarget::parse(&probe_config(
                ProbeKind::Http,
                "http://user:pass@host:8080/"
            ))
            .unwrap_err(),
            ProbeTargetError::InvalidHost {
                target: "http://user:pass@host:8080/".to_string()
            }
        );
    }

    #[test]
    fn http_whitespace_in_host_rejected_as_invalid_host() {
        // fails if a host containing a literal space is accepted outright
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://my host/")).unwrap_err(),
            ProbeTargetError::InvalidHost {
                target: "http://my host/".to_string()
            }
        );
    }

    #[test]
    fn http_second_colon_in_host_rejected_as_invalid_host() {
        // fails if "host:8080:9090" is trusted after a single rsplit_once —
        // that puts "9090" in the port and leaves "host:8080" as the host,
        // silently dropping the middle port instead of rejecting the target
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://host:8080:9090/"))
                .unwrap_err(),
            ProbeTargetError::InvalidHost {
                target: "http://host:8080:9090/".to_string()
            }
        );
    }

    #[test]
    fn http_crlf_in_path_rejected_as_invalid_path() {
        // fails if the path is carried through unvalidated the way the host
        // never was. This exact target was demonstrated putting `X-Injected:
        // yes` on the wire as a real header, with the probe still reporting
        // success — the prober writes the path verbatim into `GET {path}
        // HTTP/1.1\r\n…`, so a `\r\n` in it appends headers of the
        // Flockfile's choosing, and a longer payload a whole second request.
        // The trailing `yes` matters: a payload ending in the `\r\n` itself
        // would be removed by this parser's own trim and prove nothing.
        let target = "http://host:8080/health\r\nX-Injected: yes";
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, target)).unwrap_err(),
            ProbeTargetError::InvalidPath {
                target: target.to_string()
            }
        );
    }

    #[test]
    fn http_space_in_path_rejected_as_invalid_path() {
        // fails if the check looks for `\r` and `\n` alone: a space is the
        // same defect one field earlier, ending the request line's path and
        // handing the server `b` where the HTTP version belongs.
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://host/a b")).unwrap_err(),
            ProbeTargetError::InvalidPath {
                target: "http://host/a b".to_string()
            }
        );
    }

    #[test]
    fn http_control_character_in_path_rejected_as_invalid_path() {
        // fails if the check tests `is_whitespace` alone — a NUL is neither
        // whitespace nor printable, and no RFC 3986 path may carry one
        // unencoded.
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Http, "http://host/a\u{0}b")).unwrap_err(),
            ProbeTargetError::InvalidPath {
                target: "http://host/a\u{0}b".to_string()
            }
        );
    }

    #[test]
    fn http_query_and_percent_encoded_path_still_accepted() {
        // fails if the path check is widened into full URI validation: `?`,
        // `&`, `=` and `%20` are all ordinary text in the tail of a target,
        // and `%20` is precisely how the space rejected above is meant to be
        // written.
        let target = ProbeTarget::parse(&probe_config(
            ProbeKind::Http,
            "http://host/health?a=1&b=%20x",
        ))
        .unwrap();
        assert_eq!(
            target,
            ProbeTarget::Http {
                host: "host".to_string(),
                port: 80,
                path: "/health?a=1&b=%20x".to_string(),
            }
        );
    }

    #[test]
    fn tcp_userinfo_in_host_rejected_as_invalid_host() {
        // fails if validate_host is wired only into parse_http and not
        // parse_tcp, which shares the same split_authority call
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Tcp, "user:pass@host:5432")).unwrap_err(),
            ProbeTargetError::InvalidHost {
                target: "user:pass@host:5432".to_string()
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
    fn tcp_bracketed_ipv6_with_port_accepted() {
        // fails if parse_tcp still hands its target straight to
        // rsplit_once(':') instead of routing through split_authority: that
        // would put "1]" in the host, and separately `("[::1]", 8080)` fails
        // DNS lookup where `("::1", 8080)` succeeds — the conventional
        // bracketed spelling would silently fail every poll forever
        let target = ProbeTarget::parse(&probe_config(ProbeKind::Tcp, "[::1]:5432")).unwrap();
        assert_eq!(
            target,
            ProbeTarget::Tcp {
                host: "::1".to_string(),
                port: 5432,
            }
        );
    }

    #[test]
    fn tcp_bracketed_ipv6_without_port_rejected_as_missing_port() {
        // fails if the bracket branch's "no port" case is defaulted to 80
        // for TCP the way it is for HTTP — TCP has no default port
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Tcp, "[::1]")).unwrap_err(),
            ProbeTargetError::MissingPort {
                target: "[::1]".to_string()
            }
        );
    }

    #[test]
    fn tcp_unbracketed_ipv6_rejected_as_invalid_host() {
        // The odd spelling that happened to resolve before this round of
        // fixes (splitting on the last colon put "::1" in the host and
        // "5432" in the port) is now rejected instead: an embedded `:`
        // outside brackets is exactly what Minor 3's validate_host rejects
        // everywhere else, and the carried Task 8 note only adjudicates the
        // *bracketed* spelling correct. Closing this loophole means the
        // ambiguous form now fails loudly at config time instead of
        // resolving by accident.
        assert_eq!(
            ProbeTarget::parse(&probe_config(ProbeKind::Tcp, "::1:5432")).unwrap_err(),
            ProbeTargetError::InvalidHost {
                target: "::1:5432".to_string()
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
