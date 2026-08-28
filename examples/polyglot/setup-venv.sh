#!/bin/sh
# Creates the venv `examples/Flockfile.polyglot.toml`'s python-app entry
# points its `interpreter` field at directly. Run once, from anywhere:
#
#   $ examples/polyglot/setup-venv.sh
#
# python-app.py uses nothing beyond the standard library, so nothing is
# `pip install`ed here -- the point of this script is the venv's own
# interpreter binary existing at a fixed path, not any package inside it.
set -eu
cd "$(dirname "$0")"
python3 -m venv --without-pip venv
echo "venv created at $(pwd)/venv -- polyglot/venv/bin/python3 is what the Flockfile runs"
