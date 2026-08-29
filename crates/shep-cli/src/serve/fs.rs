//! Filesystem containment and the leaf open, `serve`'s syscall tier.
//!
//! `#[cfg(unix)]` and `async` over `tokio::fs`, deliberately separate from
//! `serve::path`'s pure lexical walk: the lexical walk guarantees the
//! *requested* path is under the root, but says nothing about where the
//! filesystem actually sends it — a symlink is not a lexical property.
//! [`contain`] closes that without a per-request `canonicalize` (a blocking
//! syscall this design specifically avoids putting on every request), and
//! [`open_regular`] closes the window `contain`'s own walk cannot: a leaf
//! swapped for a symlink between the last check and the open.

use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use nix::fcntl::OFlag;

/// The stderr line an operator sees when [`contain`] refuses a component for
/// being a symlink in the default (non-`--follow-symlinks`) mode.
///
/// Pure and synchronous on purpose, the same reason `commands::serve`'s
/// `exposure_notice` (decision 8, Task 7) is: `contain` is async and
/// mid-request, and the message it writes needs to be assertable without
/// booting a server or capturing a live process's stderr. `contain` itself
/// does the `eprintln!`; this function only builds the string, so a test can
/// pin its content directly.
fn symlink_refusal_notice(path: &Path) -> String {
    format!(
        "shep serve: refused {} — a symlink is not permitted in the docroot; \
         pass --follow-symlinks to allow it (this reopens the check-then-open \
         race refusing symlinks closes)",
        path.display(),
    )
}

/// Joins `segments` onto `root`, refusing anything that leaves it.
///
/// `root` must already be canonical — `serve` canonicalizes it once at
/// startup, so a docroot that is itself a symlink (`dist ->
/// releases/2026-08-15`, the ordinary deploy shape) works, and every
/// comparison here is against the place the operator actually chose.
///
/// **Every component is `symlink_metadata`'d as the path is built, and any
/// component that is a symlink is refused — intermediate directories
/// included, unless `follow_symlinks` is set.** A leaf-only check misses the
/// swapped-directory case, which is the same escape one level up. There is
/// no `canonicalize` in this branch: it is a blocking syscall on a path this
/// function is about to walk anyway, and per-request canonicalization
/// would accept a TOCTOU this design does not need to accept. When the
/// walk refuses a component for being a symlink, it
/// writes one line to stderr, via [`symlink_refusal_notice`], naming the
/// refused path and `--follow-symlinks` — the sheep's own bleats, and the
/// only place an operator can tell "refused a symlink" apart from
/// "genuinely missing".
///
/// **`follow_symlinks: true` is the opt-out (`--follow-symlinks`), and it
/// changes what this function does after the walk, not during it.** The
/// per-component refusal above is skipped entirely — a symlink component is
/// pushed like any other — and once every segment is joined, the *whole*
/// result is canonicalized once and checked with [`Path::starts_with`]
/// against `root`. That is the per-request canonicalize this function
/// exists to avoid by default, reintroduced deliberately: it reopens the
/// window between the canonicalize and the eventual open, which is the
/// TOCTOU this decision closes when the flag is off. The path this mode
/// returns is therefore already fully resolved — no symlink component
/// remains in it — which is why [`open_regular`]'s `O_NOFOLLOW` is not
/// fighting this mode so much as covering the one instant after the
/// canonicalize that it cannot see.
///
/// Each segment is pushed only after `Path::new(segment).components()`
/// yields exactly one [`Component::Normal`]. `path::resolve`'s rule 5
/// already refuses every byte that could make that false; this is the
/// second lock, and it is the one that still holds if rule 5 is ever
/// loosened. On Windows it is what stops `PathBuf::push` honouring a drive
/// prefix and replacing the base path outright — this function is
/// `cfg(unix)` today, and the assertion costs one line against the day it is
/// not. This lock applies in both modes; it is the per-component symlink
/// refusal that `follow_symlinks` skips, not this one.
///
/// [`Path::starts_with`] compares **components**, not string prefixes: a
/// root of `/srv/www` does not contain `/srv/www-secret`, and a `to_str()`
/// prefix test would say it does. In the default mode it cannot fail given
/// the walk above, which is exactly why it is cheap to keep; in
/// `follow_symlinks` mode it is doing real work — it is the only thing
/// standing between a symlink that escapes the root and a body sent back
/// over HTTP.
///
/// # Errors
/// `None` for every refusal — missing, a symlink component (default mode
/// only), or outside `root` after resolution (either mode). The caller
/// answers 404 to all of them without distinguishing: a server that tells a
/// client which of "missing" and "forbidden" applies is a server that maps
/// its own filesystem on request. The stderr line above is for the
/// operator, not the response.
pub async fn contain(root: &Path, segments: &[String], follow_symlinks: bool) -> Option<PathBuf> {
    let mut joined = root.to_path_buf();
    for segment in segments {
        // Second lock (see the doc comment above): only a single `Normal`
        // component may reach `PathBuf::push`.
        let mut components = Path::new(segment).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => {}
            _ => return None,
        }

        joined.push(segment);

        if !follow_symlinks {
            match tokio::fs::symlink_metadata(&joined).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    eprintln!("{}", symlink_refusal_notice(&joined));
                    return None;
                }
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    if follow_symlinks {
        let canonical = tokio::fs::canonicalize(&joined).await.ok()?;
        return canonical.starts_with(root).then_some(canonical);
    }

    joined.starts_with(root).then_some(joined)
}

/// Opens a leaf for reading without ever following a symlink, and without
/// ever blocking on a fifo.
///
/// `O_NOFOLLOW` is the lock on the race the walk in [`contain`] cannot close
/// on its own: a leaf swapped for a symlink after it was checked fails the
/// open with `ELOOP` instead of being followed into somebody else's file.
/// It is safe, it needs no new dependency (`nix::fcntl::OFlag::O_NOFOLLOW`,
/// and nix is already a `cfg(unix)` dependency of this crate), and unlike
/// `openat2(RESOLVE_BENEATH)` it works on macOS.
///
/// `O_NONBLOCK` is the second flag and it is not decoration. Opening a fifo
/// read-only **blocks until a writer appears** — the task is gone for the
/// life of the process, a denial of service with no error message. With it
/// the open returns immediately and the type check below answers 404. On a
/// regular file it is a no-op.
///
/// **The metadata comes from the open handle, never from a second stat of
/// the path.** `File::metadata` is an `fstat` on the descriptor already
/// held, so the "regular file" answer is about the bytes that are actually
/// going to be streamed, not about whatever the path named a moment ago.
///
/// # Errors
/// `None` if the open failed for any reason, or if the thing opened is not a
/// regular file. Every one of them is the caller's 404.
pub async fn open_regular(path: &Path) -> Option<(tokio::fs::File, u64)> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(OFlag::O_NOFOLLOW.bits() | OFlag::O_NONBLOCK.bits());
    }
    // `FILE_FLAG_OPEN_REPARSE_POINT` is the Windows analogue of
    // `O_NOFOLLOW`: it opens the reparse point itself rather than following
    // it, so a symlink or directory junction swapped in after `contain`'s
    // walk is opened as the link and then refused by the `is_file` check
    // below rather than silently followed out of the docroot.
    //
    // Worth naming because the threat model is not identical: on Windows a
    // directory JUNCTION needs no privilege to create, where a file symlink
    // does, so the race this closes is cheaper for an attacker to attempt
    // here than on unix. That makes the flag more load-bearing on this
    // platform, not less.
    #[cfg(windows)]
    {
        /// `FILE_FLAG_OPEN_REPARSE_POINT`.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(OPEN_REPARSE_POINT);
    }
    let file = options.open(path).await.ok()?;
    let metadata = file.metadata().await.ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some((file, metadata.len()))
}

// `unix` because the containment cases build symlinks with `std::os::unix::fs::symlink` and assert `O_NOFOLLOW` — guarantees the Windows tier
// deliberately makes differently, each argued at its own call site
// above. What Windows claims instead is covered by `tests/cli_e2e.rs`
// and by the real-flock verification in the Windows port's own notes;
// this module's unix coverage is unchanged.
#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;

    /// fails if a symlinked LEAF inside the root that points outside it is
    /// served. The lexical walk cannot catch this one — the target has no
    /// `..` in it at all.
    #[tokio::test]
    async fn a_symlinked_leaf_pointing_outside_the_root_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("www");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("ok.txt"), b"served").unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, b"not served").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("escape.txt")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();

        // positive control, in the same test: the ordinary neighbour works.
        assert!(
            contain(&root, &["ok.txt".to_string()], false)
                .await
                .is_some()
        );
        assert!(
            contain(&root, &["escape.txt".to_string()], false)
                .await
                .is_none()
        );
    }

    /// fails if only the leaf is checked. A symlinked INTERMEDIATE directory
    /// is the same escape one level up, and it is the case a leaf-only walk
    /// misses — `www/link` is a symlink and `www/link/x` is an ordinary file
    /// on the far side of it.
    #[tokio::test]
    async fn a_symlinked_intermediate_directory_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("www")).unwrap();
        std::fs::create_dir(dir.path().join("elsewhere")).unwrap();
        std::fs::write(dir.path().join("elsewhere/x"), b"not served").unwrap();
        std::fs::write(dir.path().join("www/ok.txt"), b"served").unwrap();
        let root = std::fs::canonicalize(dir.path().join("www")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), root.join("link")).unwrap();

        assert!(
            contain(&root, &["ok.txt".to_string()], false)
                .await
                .is_some()
        );
        assert!(
            contain(&root, &["link".to_string(), "x".to_string()], false)
                .await
                .is_none()
        );
    }

    /// fails if `O_NOFOLLOW` is not on the open. This is the lock on the race
    /// `contain`'s walk cannot close: it drives `open_regular` directly on a
    /// symlink the walk never saw, which is what a leaf swapped between the
    /// check and the open looks like from the open's side. Deterministic —
    /// there is no race to win here, only a flag to assert.
    #[tokio::test]
    async fn open_regular_refuses_a_symlink_the_walk_never_saw() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, b"not served").unwrap();
        let ordinary = dir.path().join("ok.txt");
        std::fs::write(&ordinary, b"served").unwrap();
        let link = dir.path().join("escape.txt");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        assert!(open_regular(&ordinary).await.is_some(), "positive control");
        assert!(open_regular(&link).await.is_none());
    }

    /// fails if a fifo in the docroot can hang the task that opens it. Without
    /// `O_NONBLOCK` this test does not fail — it never finishes, which is the
    /// production failure exactly: one request and that task is gone for the
    /// life of the process. The deadline is the forcing mechanism (IR-46) and
    /// it wraps the async call, not a synchronous one.
    #[tokio::test]
    async fn a_fifo_is_refused_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let opened = tokio::time::timeout(Duration::from_secs(5), open_regular(&fifo))
            .await
            .expect("opening a fifo must not block");
        assert!(opened.is_none(), "a fifo is not a regular file");
    }

    /// fails if containment is written as a string prefix. `/srv/www-secret`
    /// starts with the characters of `/srv/www` and is a different directory.
    /// No symlink in this one, deliberately: the symlink refusal would answer
    /// it for the wrong reason and the `starts_with` lock would go untested.
    #[tokio::test]
    async fn a_sibling_whose_name_extends_the_roots_name_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("www")).unwrap();
        std::fs::create_dir(dir.path().join("www-secret")).unwrap();
        std::fs::write(dir.path().join("www-secret/x"), b"x").unwrap();
        let root = std::fs::canonicalize(dir.path().join("www")).unwrap();
        // `..` never reaches here from `resolve`; this drives `contain`
        // directly, which is what the second lock is for.
        assert!(
            contain(
                &root,
                &["..".to_string(), "www-secret".to_string(), "x".to_string()],
                false
            )
            .await
            .is_none()
        );
    }

    /// fails if the pure notice string stops naming the path or the flag. It
    /// is `contain`'s only side effect on refusal, and it is what an operator
    /// reads in `shep bleats web` to tell "refused a symlink" apart from
    /// "genuinely missing" — the two events this decision's stderr line
    /// exists to distinguish.
    #[test]
    fn symlink_refusal_notice_names_the_path_and_the_flag() {
        let notice = symlink_refusal_notice(Path::new("/srv/www/current"));
        assert!(notice.contains("/srv/www/current"), "{notice}");
        assert!(notice.contains("--follow-symlinks"), "{notice}");
    }

    /// fails if `follow_symlinks` does not actually let an in-docroot deploy
    /// symlink through. This is the exact shape Rin's ruling names:
    /// `current -> releases/2026-08-15`, a symlink that stays inside the
    /// root the whole way. Default mode refuses it (already covered by
    /// `a_symlinked_intermediate_directory_is_not_contained`'s sibling case);
    /// this pins that the flag actually flips the outcome for the one layout
    /// it exists for.
    #[tokio::test]
    async fn an_in_docroot_deploy_symlink_is_contained_only_with_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("www");
        std::fs::create_dir_all(root.join("releases/2026-08-15")).unwrap();
        std::fs::write(root.join("releases/2026-08-15/index.html"), b"home").unwrap();
        std::os::unix::fs::symlink(root.join("releases/2026-08-15"), root.join("current")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let target = vec!["current".to_string(), "index.html".to_string()];

        assert!(
            contain(&root, &target, false).await.is_none(),
            "default mode still refuses it"
        );
        assert!(
            contain(&root, &target, true).await.is_some(),
            "the flag is what lets it through"
        );
    }

    /// fails if `follow_symlinks` trusts `canonicalize` without also checking
    /// `starts_with(root)` afterwards. Without this test the closing
    /// containment check could be deleted from the `follow_symlinks` branch
    /// and nothing above would notice, because every other `follow_symlinks`
    /// case in this file stays inside the root.
    #[tokio::test]
    async fn follow_symlinks_still_refuses_a_symlink_that_escapes_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("www");
        std::fs::create_dir(&root).unwrap();
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, b"not served").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("escape.txt")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();

        assert!(
            contain(&root, &["escape.txt".to_string()], true)
                .await
                .is_none(),
            "the flag permits a symlink component; it does not permit escaping the root"
        );
    }
}
