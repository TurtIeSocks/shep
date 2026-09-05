//! Extension-to-content-type lookup, `shep serve`'s pure MIME tier.
//!
//! No `cfg`, no I/O, no allocation on the lookup path: [`content_type`] takes
//! the request path already resolved by `serve::path` and returns the
//! `Content-Type` header value the worker writes for it.

/// Extension → content type. About twenty-five entries, ASCII-lowercased on
/// lookup, `application/octet-stream` for anything else.
///
/// A fixed table rather than `mime_guess`: this is a `match` over the
/// extensions a static site actually contains, and a dependency for it would
/// be a crate in the tree for a lookup, plus a second opinion about `.js`
/// nobody asked for.
///
/// `charset=utf-8` is on the text types: without it a browser falls back
/// to a locale-dependent encoding and a UTF-8 page renders as mojibake.
const TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("htm", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("txt", "text/plain; charset=utf-8"),
    ("md", "text/markdown; charset=utf-8"),
    ("xml", "application/xml"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("wasm", "application/wasm"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
];

/// The answer for any extension [`TYPES`] does not name, and for a path with
/// no extension at all.
const FALLBACK: &str = "application/octet-stream";

/// The `Content-Type` header value for a request path, by its extension.
///
/// The extension is everything after the last `.` (`archive.tar.gz` looks
/// up `gz`), matched against [`TYPES`] case-insensitively. A path with no
/// `.` at all, or an extension [`TYPES`] does not list, gets
/// [`FALLBACK`].
#[must_use]
pub fn content_type(path: &str) -> &'static str {
    let Some((_, extension)) = path.rsplit_once('.') else {
        return FALLBACK;
    };
    TYPES
        .iter()
        .find(|(ext, _)| ext.eq_ignore_ascii_case(extension))
        .map_or(FALLBACK, |(_, mime)| mime)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the table lookup, rather than a substring match, stops being
    /// what answers a request for a known extension.
    #[test]
    fn a_css_file_gets_the_css_content_type() {
        assert_eq!(content_type("style.css"), "text/css; charset=utf-8");
    }

    /// fails if an unknown extension or a missing one stops falling back
    /// to the generic binary type.
    #[test]
    fn an_unknown_extension_and_no_extension_both_fall_back() {
        assert_eq!(content_type("archive.wat"), FALLBACK);
        assert_eq!(content_type("Makefile"), FALLBACK);
    }

    /// fails if the lookup stops being case-insensitive.
    #[test]
    fn the_lookup_is_case_insensitive() {
        assert_eq!(content_type("INDEX.HTML"), "text/html; charset=utf-8");
        assert_eq!(content_type("Index.Html"), "text/html; charset=utf-8");
    }

    /// fails if a double extension resolves on the first dot instead of
    /// the last.
    #[test]
    fn a_double_extension_uses_only_the_last_one() {
        assert_eq!(content_type("archive.tar.gz"), FALLBACK);
        assert_eq!(content_type("archive.tar.zip"), "application/zip");
    }
}
