# Reading the dog index from the CLI: design

**Date:** 2026-08-21
**Status:** delegate-mode design, approved by Rin
**Scope:** `shep dogs --available`. Discovery only. Not `shep install`.

Follows [the community dog index](2026-08-20-community-dog-index-design.md),
which landed the JSON and the page. This is the shep-side half Rin named when
she proposed it: the index is machine-readable, so the CLI can read it.

## Scope, and why discovery ships alone

`shep dogs --available` lists what an operator could adopt. It never downloads
a binary and never runs one.

That is sequencing rather than timidity. **The genuinely new capability is
shep making an outbound request at all.** The CLI's only network code today is
bark's webhook POST, which reads a status line and deliberately never reads a
body. Building discovery first proves that plumbing under a feature whose
worst case is a wrong table, and leaves install to be additive on ground that
already works, with its own spec and its own security review.

Installing is a larger step for a reason recorded in `docs/specs/deferred.md`:
`shep adopt` vets by executing the candidate, and today the operator is the
one who chose to install it. An index makes shep the chooser.

## 1. The verb

A flag on the existing verb rather than a forty-first verb.

```
shep dogs                    # the dogs you have        (needs a shepherd)
shep dogs --available        # the dogs you could have  (needs no shepherd)
shep dogs --available spot   # exactly one match: the detail view
shep dogs --available log    # several matches: a filtered table
```

The optional positional filter is a case-insensitive substring match over
`name`, `package` and `description`. It exists because the table is box-drawn
at a terminal now, so piping to `grep` mangles it. The filter has to be inside
the command.

The filter applies in every output mode, `--format json` included: it narrows
the data, not the rendering. **A filter matching nothing is not an error.** It
prints an empty listing and says so (`no dog matches "wombat"`), and exits 0,
because "there is no such dog" is an answer rather than a failure. A filter
matching exactly one entry gives the detail view; two or more give the table.

**`--available` needs no daemon**, and that is worth documenting rather than
treating as a quirk. It is the only `dogs` mode that works on a machine with
no shepherd running, which is precisely the machine somebody is on when they
are shopping for one. The `Commands::Dogs` arm must therefore skip
`connect_client` when the flag is set, rather than connecting and ignoring the
result.

`--format json` and `-q` are global and apply unchanged. The JSON form emits
the index entries as shep parsed them, after sanitising, so a script sees the
same bytes the table was built from.

### The detail view

One match prints the whole entry, which is where `adopt_as` earns its place:

```
Spot . shep-log-rotate . logs
Rotates grown log files and asks the shepherd to reopen them.
MIT OR Apache-2.0 . https://github.com/TurtIeSocks/shep-log-rotate

  $ cargo install --git https://github.com/TurtIeSocks/shep-log-rotate
  $ shep adopt log-rotate ~/.cargo/bin/shep-log-rotate
```

The second line is the point of the whole feature. A dog cannot learn the name
it was adopted under, and `DogConfig` for a name nobody adopted returns the
empty string, indistinguishable from a registered dog with no section. So an
operator who guesses the name from the package or the dog's own name discards
their entire configuration for it, silently. Printing the right line is the
fix.

The install-path line carries the same caveat the web page does: a crate's
binary target need not share its package name, and `CARGO_INSTALL_ROOT` moves
the destination. A wrong path is loud, because `shep adopt` refuses a file
that is not there. A wrong name is silent.

## 2. The fetch

`GET https://shep.turtlesocks.dev/dogs.json`, hardcoded, overridable with
`SHEP_DOG_INDEX` for testing and self-hosting. Without an override the
integration tests cannot point at a local server, and an environment variable
is trusted input by the project's own threat model.

Measured against the real host on 2026-08-21, so the client can be small:

- **`Content-Length`, never chunked.** Checked on a 404 (9,379 bytes) and a
  40,856-byte page. The client parses `Content-Length` and reads that many
  bytes. It **refuses** a `Transfer-Encoding: chunked` response rather than
  mis-parsing one.
- **No `Content-Encoding`** unless the client asks. It does not ask, so there
  is no gzip to decode.
- **A file path does not redirect.** GitHub Pages 301s a directory path
  without a trailing slash, but `/dogs.json` returns 200 directly.

Bounds, all deliberate:

- **HTTPS only.** No plaintext, no downgrade.
- **Redirects refused, not followed.** A 3xx is an error naming its
  `Location`. Safe only because the URL is an exact file path; if the index
  ever moves, shep is updated. This removes redirect handling, and with it a
  class of bug, from a hand-rolled client.
- **1 MiB cap** on the response, refused beyond, so a hostile or broken server
  cannot make shep read forever.
- **10 second total timeout.**
- **No caching in v1.** The host sends `etag` and `cache-control: max-age=600`
  and a future install path would want both. A command run occasionally does
  not, and an unused cache is a place for staleness bugs to live.

## 3. The index is untrusted input, and that is the real work

This is the part that is easy to miss on a read-only feature, and it is the
reason this design spends more words on printing than on fetching.

**Every string in that JSON reaches a terminal.** A `description` containing
`\x1b[2J` clears the screen. `\x1b]0;` rewrites the window title. Well-placed
escapes can imitate shep's own output, and shep emits colour itself now, so a
reader has no way to tell shep's bytes from an entry's. Without a guard, "a
pull request added a row to a table" becomes "a pull request can drive your
terminal."

**Every string from the index is sanitised before it reaches a stream.**
Control characters, escape sequences and anything not printable are stripped.
An entry that needed stripping still lists, with its stripped text, and the
footer says how many were affected (`1 entry contained control characters`)
alongside the skipped count. It is reported rather than quietly cleaned
because silently repairing hostile input teaches nobody anything, and a
maintainer reading that footer has a reason to look at the pull request that
added the row.

Stripping rather than escaping: rendering `^[[2J` literally is arguably more
honest, but strip is simpler to get right, and nothing anybody wants to read
is lost.

**shep re-validates rather than trusting the site's build.** Required fields
present, category known, `repo` and any `source` URL HTTPS. The site's
validator is a different program on a different machine and may be older,
newer, or bypassed by whoever is serving `SHEP_DOG_INDEX`.

**A malformed entry is skipped and counted, not fatal.** One bad row must not
blank the listing. The count prints in the footer (`2 entries skipped`) so the
skip is never silent.

## 4. Code shape

**Extract `crates/shep-cli/src/http.rs`** from `dog/bark/sinks.rs`. Moving:
the `tls_connector()` static, URL parsing (`parse_sink_url` and its
`SinkTarget`, renamed for a caller that is not a sink), and the connect step
of `deliver_inner`. bark keeps `build_request`, `write_and_read`,
`read_response` and `parse_status_code`, which encode its own POST-and-status
semantics and are not the index's.

This is duplication of meaning, not incidental shape: TLS setup and host/port
parsing are one rule. The extraction is deliberately narrow because
`sinks.rs` is working, tested code on a path that pages people.

**A new module for the index** holding the fetch, the parse, the validator and
the sanitiser, so every security-relevant line sits in one file a reviewer can
hold in their head at once.

**Note for whoever implements this:** `Commands::Dogs` is a **unit variant**
today (`crates/shep-cli/src/lib.rs:1125`), so adding the flag means giving it
a `DogsArgs`, which touches the `Commands` enum and the wiring test around
`lib.rs:1696`. `crates/shep-cli/src/cli.rs` also carries **Rin's own
uncommitted work** near `DevArgs` (`cli.rs:1097`). Different region, so no
conflict, but stage by name and **never run `git checkout` on that file**.

## 5. Testing

Unit:

- The sanitiser against a table of hostile strings: a screen clear, a title
  setter, a bare `\r`, a lone `\x1b`, a colour sequence, a nul byte. Each
  asserts the output contains no escape and that the entry was reported as
  sanitised.
- The validator's skip-and-count, including that a good entry beside a bad one
  still lists.
- `Content-Length` parsing; refusal of `Transfer-Encoding: chunked`; refusal
  of a 3xx naming its `Location`; refusal past the 1 MiB cap; refusal of a
  non-HTTPS URL.
- The filter's substring match across all three fields, case-insensitively.

Integration, against a local server on an ephemeral port with `SHEP_DOG_INDEX`
pointed at it:

- The table renders a known index; the detail view renders both commands with
  `adopt_as` in the adopt line.
- **An entry whose description carries an ANSI escape, asserting the escape
  never reaches the output stream.** This is the test the feature exists to
  pass.
- A server that returns 500, one that closes mid-body, and one that never
  responds, each producing a clear error naming the URL rather than a panic or
  a hang.
- `--available` succeeds with **no shepherd running**, which is the property
  the flag promises.

## 6. Assumptions

Judgement calls made on Rin's behalf, per delegate mode. All were presented
and approved before this document was written.

1. **Discovery only.** `shep install` is deferred to its own spec.
2. **A flag on `dogs`, not a new verb.** The CLI has forty already.
3. **Hardcoded URL with a `SHEP_DOG_INDEX` override**, without which the
   integration tests cannot run.
4. **No caching in v1**, despite `etag` and `max-age` being available.
5. **Redirects refused rather than followed.** Correct only while the URL is
   an exact file path, which is why that fact is recorded above rather than
   assumed.
6. **A malformed entry is skipped and counted, not fatal.**
7. **Sanitising strips rather than escapes.**
8. **The `http.rs` extraction is narrow**, taking TLS setup, URL parsing and
   connect, and leaving bark's response handling where it is.
9. **The JSON output emits sanitised strings**, not raw ones. A script reading
   `--format json` gets the same bytes the table was built from, rather than a
   second, unsanitised surface.
10. **A filter matching nothing exits 0.** It is an answer, not a failure, and
    a script asking "is there a dog for this" should not have to distinguish a
    no from a broken command.
