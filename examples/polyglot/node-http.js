#!/usr/bin/env node
// One file, two runtimes: `examples/Flockfile.polyglot.toml` runs this same
// script three times -- once with `interpreter = "node"`, once with
// `interpreter = "bun"`, and once with `interpreter = "node"` again but
// `instances = 3` -- to show that `interpreter` in a Flockfile is a plain
// program name, not a language shep has special-cased anywhere.
//
// Usage: node-http.js <base-port>
//
// Binds 127.0.0.1:<base-port + SHEP_INSTANCE>. The offset is what stands in
// for shep's own `reuse_port`, which is accepted in the schema but refused
// at load today (see the Flockfile's own comment on the clustered entry) --
// so N instances here means N ports, each one a distinct listener, not one
// port shared four ways.

const http = require("node:http");

const basePort = Number(process.argv[2]);
if (!Number.isInteger(basePort) || basePort <= 0) {
  console.error("usage: node-http.js <base-port>");
  process.exit(1);
}
const instance = Number(process.env.SHEP_INSTANCE ?? "0");
const port = basePort + instance;
const runtime = typeof Bun !== "undefined" ? "bun" : "node";

const server = http.createServer((req, res) => {
  res.writeHead(200, { "Content-Type": "text/plain" });
  res.end(`OK from ${runtime} pid=${process.pid} instance=${instance}\n`);
});

server.listen(port, "127.0.0.1", () => {
  console.log(
    `node-http (${runtime}) pid=${process.pid} listening on 127.0.0.1:${port} instance=${instance}`,
  );
});
