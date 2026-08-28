#!/bin/sh
# Builds the static binary `examples/Flockfile.polyglot.toml`'s go-http
# entry runs directly, with no `interpreter` -- a compiled program needs
# none. Run once, from anywhere:
#
#   $ examples/polyglot/go-http/build.sh
set -eu
cd "$(dirname "$0")"
go build -o go-http .
echo "built polyglot/go-http/go-http"
