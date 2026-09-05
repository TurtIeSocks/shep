#!/usr/bin/env bash
#
# Regenerates web/src/data/cli-reference.generated.txt from the real `shep`
# binary's own --help output, so nothing in this file is hand-typed.
#
# Usage, from the repo root:
#   cargo build --release
#   ./web/scripts/generate-cli-reference.sh
#
# Re-run whenever a verb, alias, or flag changes in crates/shep-cli/src/cli.rs.
# A stale copy is not a build failure: `web/src/data/cliReference.ts` parses
# it at Astro build time, so `git diff` after running is the check.

set -euo pipefail

# Clean environment: clap renders an `[env: VAR=]` line showing the
# variable's CURRENT value, so a generator run with e.g. SHEP_HOME set would
# bake that path into the published reference.
shep() { env -u SHEP_HOME -u SHEP_STYLE -u NO_COLOR "$BIN" "$@"; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# `shep` on unix, `shep.exe` on Windows: checked in that order rather than
# branching on `$OSTYPE`, since this runs under Git Bash and WSL too.
BIN="$REPO_ROOT/target/release/shep"
if [[ ! -x "$BIN" && -x "$BIN.exe" ]]; then
  BIN="$BIN.exe"
fi
OUT="$SCRIPT_DIR/../src/data/cli-reference.generated.txt"

if [[ ! -x "$BIN" ]]; then
  echo "error: $BIN not found or not executable — run 'cargo build --release' first" >&2
  exit 1
fi

# Verb order matches the Commands enum's declaration order. `resurrect`, a
# hidden alias of `muster` (clap `alias`, not `visible_alias`), stays out on
# purpose so it doesn't appear in generated docs. `help` is clap's own and
# is the other deliberate omission.
#
# `every_visible_verb_reaches_the_docs_site_generator` in cli.rs checks this
# list against the binary's own visible subcommands, so a new verb fails a
# test rather than going undocumented.
VERBS=(
  start add serve stop restart reload delete stock flock dogs enable disable
  adopt rehome describe trigger signal whisper fold bleats lookout whistle
  reopen flush barks set get unset ping kill save muster runtime dev
  import startup unstartup completions init style welcome
)

{
  # No version line here: the reference page reads the workspace version
  # from Cargo.toml at Astro build time (web/src/data/workspaceVersion.ts),
  # so what is committed here changes only when the CLI surface itself does.
  echo "@@TOPLEVEL@@"
  shep --help
  for v in "${VERBS[@]}"; do
    echo "@@VERB:$v@@"
    shep "$v" --help
  done
} > "$OUT"

echo "wrote $OUT ($(wc -l < "$OUT" | tr -d ' ') lines, ${#VERBS[@]} verbs)"
