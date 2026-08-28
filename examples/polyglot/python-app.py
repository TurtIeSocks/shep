#!/usr/bin/env python3
"""A plain HTTP server, standard library only.

Run this under a venv's own interpreter rather than a bare "python3" that
resolves on whoever's PATH the daemon happens to have: the classic failure
this example exists to head off is an app that runs fine from a shell (your
own venv is active) and fails, or silently runs against the wrong
interpreter, under a supervisor that never activated it.
`examples/Flockfile.polyglot.toml` points its `interpreter` field straight
at `polyglot/venv/bin/python3` -- see `polyglot/setup-venv.sh`.

Usage: python-app.py <port>
"""

import http.server
import os
import sys


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: python-app.py <port>")
    port = int(sys.argv[1])

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802 - http.server's own name
            body = f"OK from python pid={os.getpid()}\n".encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with http.server.HTTPServer(("127.0.0.1", port), Handler) as httpd:
        print(f"python-app pid={os.getpid()} listening on 127.0.0.1:{port}")
        httpd.serve_forever()


if __name__ == "__main__":
    main()
