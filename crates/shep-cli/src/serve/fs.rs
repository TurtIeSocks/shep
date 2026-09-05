//! Filesystem containment and the leaf open, `serve`'s syscall tier.
//!
//! Separate from `serve::path`'s pure lexical walk: a lexical walk cannot
//! see where the filesystem actually sends a symlink. [`contain`] closes
//! that without a per-request `canonicalize`; [`open_regular`]'s
//! `O_NOFOLLOW` closes the remaining window, a leaf swapped for a symlink
//! between the walk and the open. An intermediate directory swapped into
//! that same window still escapes; closing it needs descriptor-relative
//! opens, which this module does not do.

use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use nix::fcntl::OFlag;

/// The stderr line an operator sees when [`contain`] refuses a component
/// for being a symlink in the default mode.
///
/// Pure and synchronous, so a test can pin its content without booting a
/// server: `contain` does the `eprintln!`, this only builds the string.
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
/// `root` must already be canonical. Each component is refused if it is
/// a symlink, intermediate directories included, unless `follow_symlinks`
/// is set, which instead canonicalizes the whole joined path once and
/// checks it against `root` with [`Path::starts_with`] (components, not
/// string prefixes). Every segment must resolve to exactly one
/// [`Component::Normal`], so a `..` or drive prefix cannot reach
/// [`PathBuf::push`].
///
/// # Errors
/// `None` for missing, a refused symlink, or a result outside `root`.
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

/// Opens a leaf for reading without following a symlink or blocking on a
/// fifo.
///
/// `O_NOFOLLOW` closes the race [`contain`]'s walk cannot: a leaf swapped
/// for a symlink after the check fails the open with `ELOOP` rather than
/// being followed. `O_NONBLOCK` stops an open on a fifo from blocking
/// forever on a writer. Metadata comes from the open handle (`fstat`),
/// never a second stat of the path.
///
/// # Errors
/// `None` if the open failed, or if the result is not a regular file.
pub async fn open_regular(path: &Path) -> Option<(tokio::fs::File, u64)> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        options.custom_flags(OFlag::O_NOFOLLOW.bits() | OFlag::O_NONBLOCK.bits());
    }
    // `FILE_FLAG_OPEN_REPARSE_POINT` is the Windows analogue of
    // `O_NOFOLLOW`: it opens the reparse point itself, so a swapped
    // symlink or junction is refused by the `is_file` check below.
    #[cfg(windows)]
    {
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

// unix only: the cases build symlinks with `std::os::unix::fs::symlink`.
// Windows coverage is in `tests/cli_e2e.rs`.
#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;

    /// fails if a symlinked leaf that points outside the root is served.
    /// The lexical walk cannot catch this: the target has no `..` in it.
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

    /// fails if only the leaf is checked: `www/link` is a symlink and
    /// `www/link/x` is an ordinary file on the far side of it.
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

    /// fails if `O_NOFOLLOW` is not on the open: drives `open_regular`
    /// directly on a symlink the walk never saw.
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

    /// fails if a fifo in the docroot can hang the task that opens it.
    /// Without `O_NONBLOCK` this test never finishes rather than failing,
    /// so the timeout is what forces it to.
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

    /// fails if containment is written as a string prefix.
    /// `/srv/www-secret` starts with the characters of `/srv/www` and is a
    /// different directory. No symlink here: the symlink refusal would
    /// answer for the wrong reason.
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

    /// fails if the notice stops naming the path or the flag.
    #[test]
    fn symlink_refusal_notice_names_the_path_and_the_flag() {
        let notice = symlink_refusal_notice(Path::new("/srv/www/current"));
        assert!(notice.contains("/srv/www/current"), "{notice}");
        assert!(notice.contains("--follow-symlinks"), "{notice}");
    }

    /// fails if `follow_symlinks` does not let an in-docroot deploy
    /// symlink through, `current -> releases/...`, a symlink that stays
    /// inside the root the whole way.
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

    /// fails if `follow_symlinks` trusts `canonicalize` without also
    /// checking `starts_with(root)` afterwards.
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
