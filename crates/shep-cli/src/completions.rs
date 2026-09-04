//! Static shell completion scripts (spec §9): `shep completions <shell>`.
//!
//! Pure tier, no `#[cfg(unix)]` and no client: it names only `cli.rs`,
//! `clap` and `clap_complete`, all of which compile everywhere, so its tests
//! run on the Windows leg like the rest of the parse surface (`cli.rs`'s own
//! tests).
//!
//! Static only — sheep names, fold names and other daemon-side identifiers
//! are never completed. Dynamic completion would need an already-connected
//! daemon at completion time, and clap_complete's dynamic-completion engine
//! is `unstable-dynamic` upstream. Recorded here as deliberately deferred,
//! rather than letting it quietly drop off spec §9's list.

use clap::CommandFactory;

use crate::cli::{Cli, CompletionArgs};
use crate::exit::ExitCode;

/// Writes the completion script for `args.shell` to `out`.
///
/// Its own tests call it on every target, and `run`'s `Commands::Completions`
/// arm calls it unconditionally too: there is one `run`, not a per-platform
/// pair, so production code reaches this function on every target the crate
/// ships for.
pub fn completions(out: &mut dyn std::io::Write, args: &CompletionArgs) -> ExitCode {
    clap_complete::aot::generate(args.shell, &mut Cli::command(), "shep", out);
    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use clap_complete::aot::Shell; // hoisted: both tests name it

    use super::*;

    /// Routed through `completions`, not through `clap_complete::generate`
    /// directly. A test that called the upstream function would pass against a
    /// `completions` that printed nothing, wrote to the wrong stream, ignored
    /// `args.shell` and always emitted bash, or returned `Failure` — it would
    /// be testing clap_complete, which is upstream's job.
    #[test]
    fn completions_generate_a_named_script_for_every_supported_shell() {
        let mut scripts = Vec::new();
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut buf = Vec::new();
            let code = completions(&mut buf, &CompletionArgs { shell });
            assert_eq!(code, ExitCode::Success, "{shell}");
            let script = String::from_utf8(buf).unwrap();
            assert!(!script.is_empty(), "{shell} produced nothing");
            assert!(
                script.contains("shep"),
                "{shell} script must name the binary"
            );
            scripts.push((shell, script));
        }

        // Every check above passes for a `completions` that ignores
        // `args.shell` and always emits (say) bash: the output would still
        // be non-empty and name the binary. Only comparing the scripts
        // pairwise catches that — each shell's grammar is different enough
        // that two real scripts never collide.
        for i in 0..scripts.len() {
            for j in (i + 1)..scripts.len() {
                let (shell_i, script_i) = &scripts[i];
                let (shell_j, script_j) = &scripts[j];
                assert_ne!(
                    script_i, script_j,
                    "{shell_i} and {shell_j} produced identical scripts — args.shell is being ignored"
                );
            }
        }
    }

    /// A `completions` that emitted a hard-coded stub rather than OUR command
    /// tree would satisfy the test above and fail this one.
    #[test]
    fn completions_cover_the_visible_aliases() {
        let mut buf = Vec::new();
        completions(&mut buf, &CompletionArgs { shell: Shell::Bash });
        let script = String::from_utf8(buf).unwrap();
        for verb in ["flock", "list", "ls", "bleats", "logs"] {
            assert!(script.contains(verb), "{verb} missing from the bash script");
        }
    }
}
