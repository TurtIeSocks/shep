//! Directory listing rendering, `shep serve`'s pure autoindex tier.
//!
//! No `cfg`, no I/O: the worker (Task 6) reads a directory, drops any name
//! starting with `.` unless `--hidden` (decision 4), sorts directories
//! first, and hands the result to [`render`]. This module only turns that
//! list into HTML — it never touches the filesystem itself.

/// Whether an [`Entry`] names a file or a directory.
///
/// A directory's link and label both carry a trailing `/`: the href so the
/// browser requests the directory rather than a same-named file one level
/// up, and the label so the listing reads like every other directory
/// listing on the web.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
}

/// One row in a directory listing: a name, and whether it names a file or a
/// directory.
///
/// Built with [`Entry::file`] or [`Entry::dir`]. [`render`] does not stat
/// anything — the caller already read the directory and knows which
/// constructor applies to each name.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub struct Entry {
    name: String,
    kind: Kind,
}

impl Entry {
    /// An entry naming a regular file.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: Kind::File,
        }
    }

    /// An entry naming a directory.
    #[must_use]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: Kind::Dir,
        }
    }
}

/// Renders a directory listing as one HTML document.
///
/// `entries` are file names, already sorted, directories first. `prefix` is
/// the request path the listing is for, always ending in `/` — the caller
/// redirects a directory request without a trailing slash before it ever
/// gets here.
///
/// Two escapes, and they are different escapes for different sinks. The name
/// is HTML-escaped for the text node (`&`, `<`, `>`, `"`, `'`), because a
/// file named `<script>alert(1)</script>` is a thing a build tool produces
/// by accident and a stored-XSS otherwise. The href is percent-encoded via
/// [`encode_segment`], because a name with a space, a `#` or a `?` produces
/// a link that goes somewhere else.
///
/// `entries` reaches here already filtered: the caller drops any name
/// starting with `.` unless `--hidden` (decision 4). A listing that names
/// `.env` and then 404s on it has still leaked the filename, which is the
/// whole reason listing is off by default.
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub fn render(prefix: &str, entries: &[Entry]) -> String {
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html><head><meta charset=\"utf-8\">");
    out.push_str("<title>Index of ");
    out.push_str(&escape_html(prefix));
    out.push_str("</title></head><body><h1>Index of ");
    out.push_str(&escape_html(prefix));
    out.push_str("</h1><ul>");
    if prefix != "/" {
        out.push_str("<li><a href=\"../\">../</a></li>");
    }
    for entry in entries {
        let suffix = if entry.kind == Kind::Dir { "/" } else { "" };
        out.push_str("<li><a href=\"");
        out.push_str(&encode_segment(&entry.name));
        out.push_str(suffix);
        out.push_str("\">");
        out.push_str(&escape_html(&entry.name));
        out.push_str(suffix);
        out.push_str("</a></li>");
    }
    out.push_str("</ul></body></html>");
    out
}

/// Percent-encodes one path segment for a URL.
///
/// Public within `serve` because there are **two** sinks, not one: this
/// module's `href`s, and the trailing-slash redirect's `Location` in the
/// worker. A directory named with any non-ASCII byte would otherwise produce
/// a `Location` that `http::write_head`'s control-byte check refuses, and
/// the operator would get a 500 on a directory that exists.
///
/// The safe set is `encodeURIComponent`'s: ASCII alphanumerics plus
/// `- _ . ~ ! * ' ( )`. Everything else, byte by byte, becomes `%XX` —
/// including every byte of a multi-byte UTF-8 sequence, so a non-ASCII name
/// still produces a header value that is entirely printable ASCII.
#[must_use]
#[cfg_attr(not(test), allow(dead_code))]
pub fn encode_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if is_safe_segment_byte(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// Whether `byte` may appear unescaped in a percent-encoded path segment.
#[cfg_attr(not(test), allow(dead_code))]
fn is_safe_segment_byte(byte: u8) -> bool {
    matches!(byte,
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' |
        b'-' | b'_' | b'.' | b'~' |
        b'!' | b'*' | b'\'' | b'(' | b')'
    )
}

/// HTML-escapes `s` for use in a text node.
#[cfg_attr(not(test), allow(dead_code))]
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a filename reaches the text node unescaped.
    #[test]
    fn a_filename_that_is_html_is_escaped_in_the_text_and_encoded_in_the_href() {
        let html = render("/", &[Entry::file("<script>alert(1)</script>")]);
        assert!(!html.contains("<script>alert(1)"), "{html}");
        assert!(html.contains("&lt;script&gt;alert(1)"), "{html}");
        assert!(html.contains("href=\"%3Cscript%3Ealert(1)"), "{html}");
    }

    /// fails if a name with a space or a `#` produces a broken link — the
    /// second half of the escaping pair, and the half that is about
    /// correctness rather than security.
    #[test]
    fn a_filename_with_a_space_or_a_hash_produces_a_link_that_resolves() {
        let html = render("/docs/", &[Entry::file("release notes #2.md")]);
        assert!(
            html.contains("href=\"release%20notes%20%232.md\""),
            "{html}"
        );
    }

    /// fails if a non-ASCII name produces bytes a header cannot carry. The
    /// same encoder feeds the worker's redirect `Location`, where an
    /// unencoded byte is a 500 on a directory that exists rather than a
    /// cosmetic problem.
    #[test]
    fn a_non_ascii_name_encodes_to_printable_ascii() {
        let encoded = encode_segment("документы");
        assert!(
            encoded.bytes().all(|b| (0x20..=0x7e).contains(&b)),
            "{encoded}"
        );
        assert!(encoded.starts_with('%'), "{encoded}");
    }

    /// fails if a directory entry stops getting the trailing slash that
    /// tells the browser to request it as a directory rather than a
    /// same-named file one level up.
    #[test]
    fn a_directory_entry_gets_a_trailing_slash_on_both_href_and_label() {
        let html = render("/", &[Entry::dir("assets")]);
        assert!(html.contains("href=\"assets/\">assets/</a>"), "{html}");
    }
}
