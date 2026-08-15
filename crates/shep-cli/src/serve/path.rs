//! Request-target resolution, pure tier.
//!
//! No `cfg`, no I/O: [`resolve`] takes the raw HTTP request target (the
//! bytes between the method and the protocol on the request line — an
//! opaque `&str` as far as `crate::http` is concerned) and returns either a
//! root-relative sequence of segments or the precise reason it refused the
//! target. Everything here compiles on every target this workspace ships,
//! Windows included, and is exercised by the Windows cross-check — the `\`
//! and `:` refusals below exist specifically because this module is not
//! `#[cfg(unix)]`.
//!
//! # Order, and why the obvious order is wrong
//!
//! Decoding the whole target and then splitting on `/` lets a `%2f` create a
//! separator after the fact, so `..%2f..%2fetc%2fpasswd` would become
//! `../../etc/passwd` *after* a traversal check already ran on a single
//! segment. [`resolve`] splits first and decodes each segment second, so a
//! decoded `/` is a forbidden byte inside a segment rather than a separator
//! between two.
//!
//! The filesystem half of request handling — the containment walk, the
//! symlink refusal, the open — lives in `serve::fs`, `#[cfg(unix)]` and
//! `async`: both are syscalls, and a syscall inside a tokio task must not be
//! the blocking `std::fs` kind.

use core::fmt;

/// Why a request target cannot be resolved. Every variant is a 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub enum Refusal {
    /// The target does not begin with `/` — an absolute-form target
    /// (`GET http://host/x`), the asterisk form (`OPTIONS *`), a bare
    /// relative path, or an empty string.
    NotAbsolute,
    /// A `%` escape that is not followed by exactly two hex digits.
    BadEscape,
    /// A decoded segment carries a byte a path segment may not: NUL, any
    /// other control byte (`< 0x20` or `0x7f`), `/`, `\`, or `:`.
    ///
    /// `:` is refused on every target, not only Windows: a segment carrying
    /// a drive prefix makes `PathBuf::push` replace the base path outright,
    /// and `x.txt:$DATA` names an NTFS alternate data stream. A colon in a
    /// served filename is worth less than that class of bug.
    ForbiddenByte,
    /// A decoded segment is not valid UTF-8.
    NotUtf8,
    /// A `..` with nothing left to pop — the target reaches above the root.
    AboveRoot,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAbsolute => "request target does not begin with \"/\"",
            Self::BadEscape => "a \"%\" escape is not followed by two hex digits",
            Self::ForbiddenByte => {
                "a decoded segment carries a byte a path segment may not: \
                 a control byte, \"/\", \"\\\", or \":\""
            }
            Self::NotUtf8 => "a decoded segment is not valid UTF-8",
            Self::AboveRoot => "the target reaches above the root",
        })
    }
}

impl core::error::Error for Refusal {}

/// Resolves a request target to a root-relative sequence of segments, or
/// refuses it.
///
/// 1. Refuses a target that does not start with `/`.
/// 2. Cuts the query at the first `?` and a fragment at the first `#`,
///    discarding both.
/// 3. Splits on `/`.
/// 4. Percent-decodes each segment, byte by byte, refusing a malformed
///    escape.
/// 5. Refuses a decoded segment containing a NUL byte, any other control
///    byte, `/`, `\`, or `:`, or one that is not valid UTF-8.
/// 6. Walks the segments: `""` and `"."` are skipped, `".."` pops, and a
///    `".."` with nothing left to pop is a refusal rather than a clamp to
///    root. Everything else is pushed.
///
/// An absolute-*looking* target such as `/etc/passwd` is never refused by
/// this function — every segment is pushed onto a stack that starts empty,
/// so `/etc/passwd` resolves to `["etc", "passwd"]` and is looked for
/// *inside* whatever root the caller joins it against. No lexical walk here
/// can produce a path outside the root; that is what makes rule 6 correct
/// rather than merely convenient.
///
/// `+` is never decoded as a space — that substitution belongs to
/// `application/x-www-form-urlencoded` query strings, not path segments.
///
/// # Errors
/// See [`Refusal`] for the precise condition behind each variant.
///
/// `resolve`'s only caller is `serve::fs`/`serve::worker` (Tasks 3 and 6),
/// which do not call into it outside this module's own tests yet — see
/// `http::HttpError::BadHeader`'s doc comment for the full reasoning behind
/// the `cfg_attr` below, the same shape used there for the same reason.
#[cfg_attr(not(test), allow(dead_code))]
pub fn resolve(target: &str) -> Result<Vec<String>, Refusal> {
    if !target.starts_with('/') {
        return Err(Refusal::NotAbsolute);
    }
    let cut = target.find(['?', '#']).unwrap_or(target.len());
    let path = &target[1..cut];

    let mut stack: Vec<String> = Vec::new();
    for raw_segment in path.split('/') {
        let decoded = decode_segment(raw_segment)?;
        if decoded.iter().copied().any(is_forbidden_byte) {
            return Err(Refusal::ForbiddenByte);
        }
        let segment = String::from_utf8(decoded).map_err(|_| Refusal::NotUtf8)?;
        match segment.as_str() {
            "" | "." => {}
            ".." => {
                if stack.pop().is_none() {
                    return Err(Refusal::AboveRoot);
                }
            }
            _ => stack.push(segment),
        }
    }
    Ok(stack)
}

/// Whether any segment in a resolved target begins with `.` — `.env` at the
/// root and `.git/config` two levels down are the same leak, and this
/// predicate reads as hidden on either.
///
/// The refusal this feeds is the handler's, not this function's: a hidden
/// path is a 404 unless `--hidden` was given (decision 4). This is the pure
/// predicate both the handler and the listing renderer (`serve::listing`)
/// ask, so the two cannot drift.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_hidden(segments: &[String]) -> bool {
    segments.iter().any(|segment| segment.starts_with('.'))
}

/// Percent-decodes one already-split segment, byte by byte.
///
/// Operates on bytes rather than `char`s: a `%` escape can produce any byte
/// 0-255, including the leading byte of a multi-byte UTF-8 sequence that only
/// becomes valid once every byte in it has been decoded — [`resolve`]
/// assembles the whole segment's bytes first and validates UTF-8 once, over
/// the result, rather than per escape.
///
/// # Errors
/// [`Refusal::BadEscape`] if a `%` is not followed by exactly two hex
/// digits (case-insensitive).
#[cfg_attr(not(test), allow(dead_code))]
fn decode_segment(segment: &str) -> Result<Vec<u8>, Refusal> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex_pair = bytes
                .get(i + 1)
                .copied()
                .and_then(hex_value)
                .zip(bytes.get(i + 2).copied().and_then(hex_value));
            let (hi, lo) = hex_pair.ok_or(Refusal::BadEscape)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// The numeric value of one ASCII hex digit, either case.
#[cfg_attr(not(test), allow(dead_code))]
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Whether a decoded byte is one rule 5 forbids in a path segment: a control
/// byte (`< 0x20` or `0x7f`, which covers NUL), `/`, `\`, or `:`.
#[cfg_attr(not(test), allow(dead_code))]
fn is_forbidden_byte(byte: u8) -> bool {
    byte < 0x20 || byte == 0x7f || matches!(byte, b'/' | b'\\' | b':')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five shapes this phase was asked to refuse by name, plus the
    /// response-splitting one, plus a positive control in the same test so a
    /// resolver that refuses everything cannot pass.
    #[test]
    fn the_traversal_shapes_are_each_refused_for_their_own_reason() {
        // positive control: an ordinary nested asset resolves, and `..` that
        // stays inside the root is allowed to pop.
        assert_eq!(
            resolve("/assets/app.css").unwrap(),
            vec!["assets", "app.css"]
        );
        assert_eq!(
            resolve("/assets/../index.html").unwrap(),
            vec!["index.html"]
        );
        assert_eq!(resolve("/").unwrap(), Vec::<String>::new());
        assert_eq!(resolve("/a//b/./c").unwrap(), vec!["a", "b", "c"]);

        assert_eq!(resolve("/../../etc/passwd"), Err(Refusal::AboveRoot));
        assert_eq!(
            resolve("/%2e%2e/%2e%2e/etc/passwd"),
            Err(Refusal::AboveRoot)
        );
        assert_eq!(
            resolve("/..%2f..%2fetc/passwd"),
            Err(Refusal::ForbiddenByte)
        );
        assert_eq!(resolve("/x%00.png"), Err(Refusal::ForbiddenByte));
        assert_eq!(
            resolve("/a%0d%0aSet-Cookie:%20x"),
            Err(Refusal::ForbiddenByte)
        );
        assert_eq!(resolve("../etc/passwd"), Err(Refusal::NotAbsolute));
        assert_eq!(
            resolve("http://elsewhere/etc/passwd"),
            Err(Refusal::NotAbsolute)
        );
        assert_eq!(resolve("/a%zz"), Err(Refusal::BadEscape));
        assert_eq!(resolve("/a%2"), Err(Refusal::BadEscape));
        assert_eq!(resolve("/a%ff%fe"), Err(Refusal::NotUtf8));
    }

    /// An absolute-looking path is NOT a refusal — it is resolved inside the
    /// root, which is the whole structural argument. `GET /etc/passwd` looks
    /// for `<root>/etc/passwd` and 404s; it never reaches `/etc/passwd`.
    #[test]
    fn an_absolute_looking_target_resolves_inside_the_root() {
        assert_eq!(resolve("/etc/passwd").unwrap(), vec!["etc", "passwd"]);
        assert_eq!(resolve("//etc/passwd").unwrap(), vec!["etc", "passwd"]);
    }

    /// fails if the query or fragment reaches the filesystem as part of a
    /// filename.
    #[test]
    fn the_query_and_fragment_are_cut_before_anything_else() {
        assert_eq!(resolve("/index.html?v=2").unwrap(), vec!["index.html"]);
        assert_eq!(resolve("/index.html#top").unwrap(), vec!["index.html"]);
        assert_eq!(resolve("/?../../etc/passwd").unwrap(), Vec::<String>::new());
    }

    /// fails if decoding runs before splitting. `%2f` must stay a byte inside
    /// one segment, never become a separator — and since `/` is forbidden in
    /// a segment, it is refused rather than silently renamed.
    #[test]
    fn decoding_happens_after_splitting_and_never_creates_a_separator() {
        assert_eq!(resolve("/a%2fb"), Err(Refusal::ForbiddenByte));
        // Double-encoded: decodes ONCE, to the literal three characters
        // `%2e%2e`, which is an ordinary (odd) filename and not a traversal.
        assert_eq!(resolve("/%252e%252e/x").unwrap(), vec!["%2e%2e", "x"]);
    }

    /// A backslash is a separator on Windows and this module compiles there.
    /// Refusing it costs a filename nobody has and closes a resolver that
    /// would be wrong the day someone builds the Windows tier. Both forms:
    /// the percent-encoded one, and the raw one a client can simply type.
    #[test]
    fn a_backslash_segment_is_refused_on_every_target() {
        assert_eq!(resolve("/a%5c..%5cetc"), Err(Refusal::ForbiddenByte));
        assert_eq!(resolve("/\\..\\..\\etc"), Err(Refusal::ForbiddenByte));
    }

    /// fails if a drive prefix reaches `PathBuf::push`. On Windows a segment
    /// carrying `:` REPLACES the base path rather than extending it, so
    /// `GET /C:/Windows/System32/config/SAM` would resolve entirely outside
    /// the docroot before any containment check saw a path derived from it.
    /// Same byte covers the NTFS alternate-data-stream form and the
    /// drive-relative one. This module compiles on Windows and this test is
    /// what the cross-check exercises.
    #[test]
    fn a_windows_drive_prefix_or_a_data_stream_is_refused() {
        assert_eq!(
            resolve("/C:/Windows/System32/config/SAM"),
            Err(Refusal::ForbiddenByte)
        );
        assert_eq!(resolve("/C:foo"), Err(Refusal::ForbiddenByte));
        assert_eq!(resolve("/x.txt:$DATA"), Err(Refusal::ForbiddenByte));
        assert_eq!(resolve("/a%3ab"), Err(Refusal::ForbiddenByte));
    }

    /// fails on the decoder's own edges: hex is case-insensitive, an overlong
    /// UTF-8 encoding of `.` is not a `.`, and the two target forms that are
    /// not paths at all are refused before anything else runs.
    #[test]
    fn the_decoder_and_the_target_form_have_no_soft_edges() {
        // Uppercase hex decodes the same as lowercase — a resolver that only
        // handles one case refuses `%2E%2E` as a filename and serves it.
        assert_eq!(resolve("/%2E%2E/x"), Err(Refusal::AboveRoot));
        assert_eq!(resolve("/%2e%2E/x"), Err(Refusal::AboveRoot));
        // Overlong UTF-8 for `.` (`%c0%ae`): invalid UTF-8, never a dot.
        assert_eq!(resolve("/%c0%ae%c0%ae/x"), Err(Refusal::NotUtf8));
        // Neither of these is a path. `*` is the asterisk-form target; the
        // empty string is what a malformed request line leaves behind.
        assert_eq!(resolve(""), Err(Refusal::NotAbsolute));
        assert_eq!(resolve("*"), Err(Refusal::NotAbsolute));
    }

    /// fails if the hidden-path predicate misses a dot anywhere but the first
    /// segment. `.env` at the root and `.git/config` two levels down are the
    /// same leak, and `--hidden` is the only thing that serves either.
    /// The refusal itself is the handler's (a 404, decision 4); this pins the
    /// predicate the handler asks.
    #[test]
    fn a_dot_leading_segment_anywhere_reads_as_hidden() {
        assert!(is_hidden(&resolve("/.env").unwrap()));
        assert!(is_hidden(&resolve("/.git/config").unwrap()));
        assert!(is_hidden(&resolve("/a/.b/c").unwrap()));
        assert!(!is_hidden(&resolve("/index.html").unwrap()));
        assert!(!is_hidden(&resolve("/a.b/c").unwrap()));
    }
}
