//! `shep serve`'s basic-auth check: a creds file, and a constant-time
//! comparison of what it holds against what a client presented.
//!
//! [`load`]'s permission-mode refusal is the only unix-only piece,
//! isolated in [`check_mode`] so the rest of the module compiles on every
//! target.
//!
//! Checked before path resolution in `serve::worker`, so a 401 never
//! distinguishes a guessed path that exists from one that does not.

use core::fmt;
use std::path::{Path, PathBuf};

use ring::hmac;

/// A raw `Authorization` header value longer than this is refused before
/// it is base64-decoded. No legitimate `user:password` pair approaches this
/// size; a client sending one this large is not presenting a credential.
const MAX_HEADER_LEN: usize = 1024;

/// One `user:password` pair, read from a file.
///
/// No `Debug` derive: nothing in shep ever needs to print a credential.
pub struct Credentials {
    expected: Vec<u8>,
}

impl Credentials {
    /// Builds the expected credential from an already-split `user` and
    /// `password`.
    fn from_pair(user: &str, password: &str) -> Self {
        Self {
            expected: format!("{user}:{password}").into_bytes(),
        }
    }

    /// Whether `header`, the raw `Authorization` value, satisfies these
    /// credentials.
    ///
    /// Compares through [`ring::hmac::verify`], not a raw byte compare:
    /// see [`credentials_match`].
    #[must_use]
    pub fn satisfies(&self, header: Option<&str>) -> bool {
        let Some(header) = header else {
            return false;
        };
        let Some(encoded) = header.strip_prefix("Basic ") else {
            return false;
        };
        if encoded.len() > MAX_HEADER_LEN {
            return false;
        }
        let Some(presented) = base64_decode(encoded) else {
            return false;
        };
        credentials_match(&presented, &self.expected)
    }
}

/// Why [`load`] refused a creds file. Every message names the path and
/// the problem, never a byte of the contents.
///
/// Not `#[non_exhaustive]`: nothing outside this crate can match on it.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug)]
pub enum AuthError {
    /// Reading `path` failed at the OS level: missing, a directory, or a
    /// permissions failure the OS itself enforces.
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying IO failure.
        source: std::io::Error,
    },
    /// `path`'s permission bits are readable by the group or the world
    /// (`mode & 0o077 != 0`). Unix only, see [`check_mode`].
    Mode {
        /// The path whose mode was refused.
        path: PathBuf,
        /// The mode bits read from the file, for the operator's `chmod`.
        mode: u32,
    },
    /// `path` has no non-empty line, holds more than one, or its one line
    /// has no `:`. One variant for all three: distinguishing them would
    /// need quoting the line to explain which rule it broke.
    Malformed {
        /// The path whose contents did not parse.
        path: PathBuf,
    },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            // `mode` is the raw `st_mode`, `S_IFREG` or'd onto the
            // permission bits. `& 0o7777` keeps only the bits `chmod`
            // accepts.
            Self::Mode { path, mode } => write!(
                f,
                "{}: mode {:03o} is readable by the group or the world; \
                 chmod 600 it",
                path.display(),
                mode & 0o7777
            ),
            Self::Malformed { path } => write!(
                f,
                "{}: expected exactly one non-empty line of the form user:password",
                path.display()
            ),
        }
    }
}

impl core::error::Error for AuthError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Mode { .. } | Self::Malformed { .. } => None,
        }
    }
}

/// Reads `path`'s permission bits and refuses one the group or the world
/// can read.
///
/// Unix only: Windows has no `mode & 0o077` to read.
#[cfg(unix)]
fn check_mode(path: &Path) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|source| AuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(AuthError::Mode {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// No mode bits to check on a non-unix target: a no-op so [`load`] reads
/// identically on every target.
#[cfg(not(unix))]
fn check_mode(_path: &Path) -> Result<(), AuthError> {
    Ok(())
}

/// Reads `path`, refusing a file the box can read.
///
/// # Errors
/// - [`AuthError::Io`] if `path` cannot be read;
/// - [`AuthError::Malformed`] if it is empty, holds more than one non-empty
///   line, or its one line has no `:`;
/// - [`AuthError::Mode`] if its mode is group- or world-readable
///   (`mode & 0o077 != 0`, unix only).
///
/// Mode is checked last, after the content parses: a freshly written
/// tempfile carries the umask's default mode, which would trip the mode
/// check before a mode-agnostic test ever exercises the content.
pub fn load(path: &Path) -> Result<Credentials, AuthError> {
    let contents = std::fs::read_to_string(path).map_err(|source| AuthError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut lines = contents.lines().filter(|line| !line.trim().is_empty());
    let malformed = || AuthError::Malformed {
        path: path.to_path_buf(),
    };
    let line = lines.next().ok_or_else(malformed)?;
    if lines.next().is_some() {
        return Err(malformed());
    }
    let (user, password) = line.split_once(':').ok_or_else(malformed)?;
    check_mode(path)?;
    Ok(Credentials::from_pair(user, password))
}

/// Compares two credentials in constant time.
///
/// Uses [`ring::hmac::verify`] rather than a raw byte compare or
/// `ring::constant_time`, which is deprecated and disclaims side-channel
/// safety. The key is fixed and public: `hmac::sign` serves only as a
/// length-normalizing constant-time compare here, not for HMAC's
/// authentication property.
fn credentials_match(presented: &[u8], expected: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, &[0u8; 32]);
    let tag = hmac::sign(&key, presented);
    hmac::verify(&key, expected, tag.as_ref()).is_ok()
}

/// Decodes standard base64 (RFC 4648, `=`-padded). `None` for anything that
/// is not exactly that: wrong length, a byte outside the alphabet, or a `=`
/// outside the last two positions of its four-byte group.
///
/// Written here rather than pulled in from a crate: the `Authorization`
/// header is the only base64 anywhere in shep-cli.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks_exact(4) {
        let mut sextets = [0u8; 4];
        let mut pad = 0u8;
        for (i, &byte) in chunk.iter().enumerate() {
            sextets[i] = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' if i >= 2 => {
                    pad += 1;
                    0
                }
                _ => return None,
            };
        }
        out.push((sextets[0] << 2) | (sextets[1] >> 4));
        if pad < 2 {
            out.push((sextets[1] << 4) | (sextets[2] >> 2));
        }
        if pad < 1 {
            out.push((sextets[2] << 6) | sextets[3]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes standard base64, the inverse of [`base64_decode`].
    fn base64(input: &str) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = input.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied();
            let b2 = chunk.get(2).copied();
            out.push(ALPHABET[(b0 >> 2) as usize] as char);
            out.push(ALPHABET[(((b0 << 4) | (b1.unwrap_or(0) >> 4)) & 0x3f) as usize] as char);
            out.push(match b1 {
                Some(b1) => {
                    ALPHABET[(((b1 << 2) | (b2.unwrap_or(0) >> 6)) & 0x3f) as usize] as char
                }
                None => '=',
            });
            out.push(match b2 {
                Some(b2) => ALPHABET[(b2 & 0x3f) as usize] as char,
                None => '=',
            });
        }
        out
    }

    /// fails if any of the four rejection shapes is accepted.
    #[test]
    fn only_the_exact_pair_is_accepted() {
        let creds = Credentials::from_pair("alice", "s3cret");
        let ok = format!("Basic {}", base64("alice:s3cret"));
        assert!(creds.satisfies(Some(&ok)));
        assert!(!creds.satisfies(None));
        assert!(!creds.satisfies(Some(&format!("Basic {}", base64("alice:s3cres")))));
        assert!(!creds.satisfies(Some(&format!("Basic {}", base64("alicf:s3cret")))));
        assert!(!creds.satisfies(Some("Basic")), "no credentials at all");
        assert!(
            !creds.satisfies(Some(&format!("Bearer {}", base64("alice:s3cret")))),
            "the scheme is part of the check"
        );
    }

    /// fails if a creds file the group or the world can read is accepted.
    #[cfg(unix)]
    #[test]
    fn a_group_readable_creds_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds");
        std::fs::write(&path, "alice:s3cret\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        // `.err().unwrap()`: `Result::unwrap_err` needs `T: Debug`, and
        // `Credentials` has none.
        let err = load(&path).err().unwrap();
        assert!(matches!(err, AuthError::Mode { .. }), "{err:?}");
        // positive control: the same file at 0600 loads.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&path).is_ok());
    }

    /// fails if `AuthError::Mode`'s `Display` prints the raw `st_mode`
    /// instead of the permission bits alone.
    #[test]
    fn a_mode_error_prints_the_permission_bits_not_the_raw_st_mode() {
        let err = AuthError::Mode {
            path: PathBuf::from("/srv/creds"),
            mode: 0o100_644,
        };
        assert_eq!(
            err.to_string(),
            "/srv/creds: mode 644 is readable by the group or the world; chmod 600 it"
        );
    }

    /// fails if a failure message ever carries the file's contents.
    #[test]
    fn no_error_message_quotes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds");
        std::fs::write(&path, "no-colon-here-s3cret\n").unwrap();
        let message = load(&path).err().unwrap().to_string();
        assert!(!message.contains("s3cret"), "{message}");
        assert!(
            message.contains("creds"),
            "it must still name the file: {message}"
        );
    }
}
