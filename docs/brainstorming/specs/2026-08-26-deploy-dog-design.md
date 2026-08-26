# Deploy dog design

Brainstormed with Rin, 2026-08-26, the evening shep 0.1.0 published.

## Goal

A dog that redeploys a sheep when a watched git branch gets a new commit:
fetch, build in an isolated release directory, swap to it, reload, verify
against the sheep's own readiness probe, and roll back on its own if the new
release does not come up.

Rin's framing was "a mini Vercel", explicitly not a Vercel replacement. Her
stated evidence is the strongest argument for building it at all: "Do you know
how many janky updating/deploying scripts I've written over the years?" Anyone
running a flock has written the pull-build-restart script, and written it
slightly differently each time.

## Why this exists as a design rather than a feature request

Two things make it more than a wrapper around `git pull && npm run build`.

**It can verify.** shep already runs readiness probes and already reloads
blue/green. A shell script cannot tell whether the app it just restarted is
actually serving; shep can, and it already does. Auto-rollback gated on the
probe is the thing every hand-rolled script skips because it is hard.

**It can be shipped by upstream.** ReactMap and Koji have self-hosting users
who each perform the same fiddly setup slightly differently. If the repo
carries its own app definition, "how do I run this" stops being a README
section people misread and becomes a file. That was Rin's argument and it is
better than the convenience one.

## Non-goals

Named explicitly, because "mini Vercel" invites scope creep:

- **Preview deploys per branch.** Dynamic routing plus N live environments is
  the single largest piece of Vercel and none of it is needed here.
- **Build caching as a service**, multi-tenancy, a web UI.
- **Multi-host anything.** shep-v1.md §1 already cuts it and nothing here
  revisits that.
- **Containers.** Also a documented v1 non-goal, and unchanged.
- **Static-site targets.** Long-running processes only. A static site has no
  process to restart and wants a different design; revisit separately if ever.

Note that [shep-v1.md](../../specs/shep-v1.md) §1 lists "deployment tooling"
as a v1 non-goal, meaning `pm2 deploy`: host lists, revision directories, `ref`
and `repo` per environment, remote execution over SSH. This design is much
smaller and lives outside the shep binary. Reconsidering the cut is not
reversing it.

## Where the line sits

**shep owns processes. The dog puts files on disk and tells shep what to run.**

shep learns nothing new about git, builds, releases, symlinks or branches. The
dog drives shep exclusively over the socket that already exists:

| direction | what crosses | exists today |
|---|---|---|
| dog → shep | `Start { apps: Vec<AppConfig> }` to register a bootstrapped app | yes; daemon re-normalises because peer input is untrusted |
| dog → shep | `Reload` after a swap, `Restart` as fallback | yes |
| dog → shep | `Describe` / `Subscribe` to watch for `Online` | yes |
| shep → dog | `DogConfig` for the dog's own config section | yes |

The one shep-side addition this design asks for is smits, below. Everything
else is a `shep-core` + `shep-client` consumer, which makes this the
validating use case for the dog contract rather than an exception carved out
of it.

## Smits

Rin's requirement, stated as non-negotiable: `shep flock` must show which sheep
are being watched. Making the whole feature internal would buy that, at the
price of shep's core learning what a deployment is.

Instead, a general mechanism: **a dog may attach a short string to a sheep, and
`shep flock` renders it without understanding it.** Fred sets `▲ main@a1b2c3`;
shep stores a string and paints a column. Reusable by any dog.

Precedent for the shape: `ProcessInfo::last_exit` plus an EXIT column landed
2026-08-19 as a wire change. This is the same move.

**Naming.** `smit` is the real shepherding term for a paint mark identifying
which flock a sheep belongs to, and unlike a brand it is deliberately
temporary, which is exactly what this is. `badge` is the plain alias, per the
lexicon's established pairing (`bleats`/`logs`, `stock`/`scale`,
`whisper`/`sendline`). `brand` was considered and rejected as permanent-by
-metaphor; `badge` alone was rejected as primary because the README already
carries seven shields.io badges and one word should not mean two things in one
project. Rin approved `smit` on the precedent of `muster` and `thatlldo`.

## Layout

Everything the dog owns lives under `$SHEP_HOME`, keyed by **sheep name**, not
by repository. Rin runs `bpm`, `ctm` and `opm` as three deployments of one
ReactMap repo, so repository is the wrong key.

```
$SHEP_HOME/deploy/<sheep>/
├── git/                      one clone; object store shared by every worktree
├── releases/<sha>/           a git worktree per release
└── current -> releases/<sha>
```

There is deliberately **no `shared/` directory**. The user's own checkout plays
that role: they followed the project's normal setup instructions, so their
config files already sit exactly where the app expects them, and the symlinks
point back there. An earlier draft had a `shared/` tree plus a `shared = [...]`
array in the Flockfile; Rin rejected both, correctly, as machinery for
something the user has already done by hand.

**The cost, recorded because it is real:** the user's checkout becomes
load-bearing forever. Deleting or moving it leaves every release with dangling
symlinks, and a `git clean -xdf` in it removes the config every release depends
on. That is a comprehensible failure with an obvious cause, which is why it is
acceptable, but it should be documented for operators rather than discovered.

The sheep's `cwd` is `<root>/current`, permanently. Swapping a release is
`rename(2)` on a symlink, which is atomic, so there is no instant where
`current` points at nothing.

**Why `$SHEP_HOME` rather than the user's own directory.** The user's existing
checkout is never restructured, moved, or written to. One uniform layout per
app, ownership and permissions already settled by whoever owns `$SHEP_HOME`,
and trivially backed up. An earlier draft put releases beside the user's clone
and required a migration; that was worse and is dropped.

## Configuration

Three files, and it matters which of them upstream controls.

**`Flockfile.toml`, committed in the repo.** Upstream owns it. The app
definition: name, script, probe, instances, and a build block. This is the file
that makes the feature worth building, because upstream shipping it is what
saves every self-hoster the same fiddly setup.

**`Flockfile.override.toml`, gitignored, in the repo.** The user owns it. Deep
merged over the committed file, override wins. Because it is gitignored, the
sharing mechanism below carries it into every release automatically, with no
special case.

**`[dog.<name>]` in `shep.toml`.** The operator's dog-level settings: poll
interval, how many releases to keep.

### Pinning, and an honest account of what it buys

Deep merge with override priority is the whole mechanism. A user who pins
`script` and `cwd` in their override is protected from those fields changing
underneath them; a user who writes no override follows upstream on every
deploy.

**`user` and `group` are pinned permanently and cannot be set by the committed
file at all.** Privilege is not a recommendation, and it is the one thing a
compromised build genuinely cannot escalate on its own.

**Pinning anything else is defence in depth, not a boundary.** If upstream is
compromised they already run arbitrary code in `bun install`'s postinstall or
in `make build`. They do not need the Flockfile. The two places pinning earns
its keep are the privilege carve-out above and persistence, since a changed
`script` survives forever while a build-time payload runs once. The real
boundary is which user the build runs as.

### What gets shared, and how

A fresh worktree contains nothing that git ignores, so it cannot run: ReactMap
needs `config/local.json`, `config/areas.json`, `server/src/configs/**` and a
generated masterfile, all ignored. Something must put them there.

**The rule:** whatever is ignored-and-present **in the user's checkout**, minus
`.shepignore`, gets symlinked from there into each new release.

- **Enumerate with `git status --ignored --porcelain`, never by parsing
  `.gitignore`.** Parsing gets negations (`!server/src/configs/.gitkeep`),
  anchored globs (`/docker-compose.yml`) and nested ignore files wrong. Git
  already answers this question correctly.
- **`.shepignore`, committed in the repo, subtracts from that set.** Same
  syntax as `.gitignore`, same idiom as `.dockerignore`. Its entries start
  empty in every release instead of being shared.

ReactMap's is one line, `dist`. Koji's is one line, `target`.

**Why build outputs must be subtracted.** Symlink `dist/` back to a single
shared copy and release B's `vite build` writes through the symlink, replacing
the assets release A is currently serving, mid-build. Rolling back to A then
gets B's `dist`. Blue/green and rollback both die on that one line. The
`.gitignore` list conflates config, caches and build outputs because git
ignores all three for the same reason; `.shepignore` is where that distinction
gets made, by the person who knows it.

Absent a `.shepignore`, everything ignored is shared, which is the
zero-configuration case.

**A user wanting fresh dependencies** adds `node_modules` to `.shepignore`.
Same file, same syntax they already know, no array to maintain.

## The deploy sequence

1. `git fetch` in `<root>/git`. Compare the tracked branch's remote head to the
   last deployed sha. No change, nothing happens.
2. `git worktree add releases/<sha> <sha>`. Shares the object store, so no
   re-download and no second `.git`.
3. Symlink the shared set (above) into the new worktree.
4. Run the build in the worktree, if the Flockfile declares one. Wait for it to
   exit. A non-zero exit aborts here: `current` never moves and the running app
   is untouched, because it lives in a different directory.
5. `rename(2)` `current` onto the new release.
6. `Reload` the sheep.
7. Verify (below). On failure, point `current` back at the previous release and
   reload again.
8. On success, prune worktrees beyond the retention count.

**Steps 1 through 4 never touch the running app.** That alone is a large
improvement over building in place.

### Blue/green comes free

shep's reload runs `SpawnNew → AwaitReady → DrainOld → ReapOld`. The
replacement is spawned and reaches readiness before the old instance drains, so
the old release serves throughout. `ProcStatus::Stopping` is documented as
reachable from exactly that one path.

For `ctm`, which runs `bun .` and compiles the client with vite's API at
startup, this means the compile happens **in the new instance while the old one
still serves**. Today that compile happens while the process is down. That is
the single biggest win available here and it needs no new machinery.

Zero downtime requires the app to bind with SO_REUSEPORT. Without it, `Reload`
degrades and the design falls back to `Restart`, with downtime equal to boot
time.

### Verify

`verify` is an enum on the target, not a boolean:

- **`probed`** (default): wait for the sheep to reach `Online`, which shep
  reports only once the readiness probe passes. Refuse to deploy a sheep with
  no probe configured, naming the missing probe.
- **`alive`**: wait N seconds and confirm the process is still running.

Rin's construction, and better than the `verify = false` it replaced: choosing
`alive` is a visible, deliberate downgrade that still checks something, rather
than an opt-out that silently checks nothing.

### Build environment and artifacts

Two fields on the Flockfile's build block, both driven by Koji:

- **`build.env`** — environment for the build. `CARGO_TARGET_DIR` pointed at a
  shared cache keeps Rust compilation warm across releases, which matters
  because a from-scratch Koji build per deploy is not acceptable.
- **`build.artifacts`** — paths copied out of the build environment into the
  release afterwards. With a shared `CARGO_TARGET_DIR` the binary lands outside
  the release, so `script = ./target/release/koji` needs it copied back for the
  script to resolve and for rollback to have a real artifact.

Node needs neither: bun, yarn and pnpm all keep global caches outside the repo,
so a fresh worktree install is mostly cache hits.

**Partially symlinking `target/` was considered and rejected.** Cargo owns that
directory's layout and a half-symlinked one invites confusing breakage.

## Bootstrap and first opt-in

The dog surveys the flock via `ListFlock` and reports, without deploying
anything. Discovery is read-only; deploying requires opt-in.

```
reactmap   needs setup     git checkout, declares a deploy block
koji       eligible        git checkout, nothing declares a deploy
legacy     not eligible    /opt/legacy is not a git repository
```

Opting in creates the tree, clones, links from the user's existing checkout,
builds the first release, and swaps `current`. Then the cutover: start a new
sheep pointing at `current`, wait for `Online`, stop and delete the old
registration.

**The first cutover specifically may have downtime.** The new instance cannot
bind the port while the old one holds it unless the app uses SO_REUSEPORT.
Every deploy after the first is zero-downtime; the first is not necessarily.

A sheep whose cwd is not a git checkout is reported as not eligible and left
entirely alone. Turning a directory into a checkout is the operator's decision,
not a dog's.

## Retention and teardown

Worktrees are removed after a **successful** deploy, keeping N releases
(default 5, configurable in the dog's own config).

Never removed: the current release, the previous release (it is the rollback
target), or any release a running instance still points at.

Removal is `git worktree remove --force` — a built tree is always dirty, so
plain `remove` always refuses — followed by `git worktree prune`.

## Security posture

**The build is the dangerous step.** It executes code from a repository, and
postinstall scripts are the most common supply-chain vector in the Node
ecosystem.

**Builds run as the target sheep's `user`, never as the shepherd's.** shep
already resolves `user`/`group` per app, so the machinery exists. A compromised
ReactMap build gets ReactMap's privileges and nothing more. This is the single
requirement in this section that is not optional.

Recommended beyond that, in increasing order of effort: a system user per app,
so a compromise in one cannot read another's config; running the shepherd as
root only so that it can drop privileges; or running the whole shepherd
unprivileged with a reverse proxy for ports below 1024.

**A dog holds full socket authority.** It can issue `Delete` as easily as
`Restart`. Nothing here changes that, and it is why the confinement work
recorded under `shep install` in [deferred.md](../../specs/deferred.md) matters
more once this exists.

**Private repositories inherit the build user's git auth.** If git cannot reach
it, the dog cannot. No credential handling of its own, deliberately.

## Concurrency

A push landing mid-deploy aborts the in-flight deploy and starts again at the
newer commit — **but only before the swap**. Once `current` has moved, the
deploy finishes and verifies, and the newer commit deploys after. Tearing down
mid-cutover is how you end up with nothing running.

Rin's reasoning for aborting rather than queueing: the common case is a hotfix
chasing a bad deploy, and the superseded build was not wanted anyway.

## Prerequisites

Three shipped-CLI issues, found by Rin using v0.1.0, are prerequisites rather
than related work. All three are reproduced and are being fixed on
`fix/dog-cli-ergonomics`:

1. **`shep adopt` cannot take a binary name or a `~/` path.** Both fail with
   "no file exists at that path", so a dog installed with `cargo install`
   cannot be adopted by name.
2. **The dog's name is a required positional.** Sheep already take an optional
   `--name` defaulting from the filename; adopt should match, stripping a
   leading `shep-` the way cargo strips `cargo-`.
3. **`shep <dogname> [args]` does not run an adopted dog.** git and cargo both
   do this. It is how the operator drives this dog at all: `shep deploy koji`,
   `shep deploy --rollback bpm`.

Number 3 resolves a question this design would otherwise have to work around,
since dogs cannot add CLI verbs.

## Open questions

- **Poll interval default.** 30 seconds is adequate for a single host and needs
  no inbound port, no HMAC verification and no public exposure. Webhooks are
  deliberately deferred: Vercel needs them because it is multi-tenant at scale.
  Un-exposing a port after the fact is harder than adding one later.
- **The `CARGO_TARGET_DIR` artifact copy is specified but not proven.** It
  should be validated against Koji before the plan commits to it.
- **Whether a `.shepignore` shipped by upstream is itself a risk.** Adding
  `config/local.json` to it would stop a user's config being linked. Low
  severity, since the app would visibly fail to start, but unrecorded
  until now.
- **Smit rendering when the terminal is narrow.** `shep flock` already drops
  columns adaptively; where a smit sits in that priority order is undecided.

## Worked examples

**Koji.** `.shepignore` is `target`. Build is `make build` with
`CARGO_TARGET_DIR` pointed at a shared cache and `target/release/koji` copied
back. `.env` is ignored-and-present, so it links automatically. Script is
`./target/release/koji`.

**ReactMap with a build script.** `.shepignore` is `dist`. Build is
`bun install && bun run build`. Config, areas, `server/src/configs/**` and the
masterfile all link automatically. Script is `bun run ./server/src/serve.ts`.
This replaces the current arrangement of a `bpm_client` entry whose script is
`yarn build` with `autorestart: false`, a `pm2 restart`, a hardcoded 30-second
sleep, and then a second restart. The dog waits for the build to exit instead
of sleeping, and a failed build never restarts anything.

**ReactMap without a build script.** No build block at all. `bun .` compiles
the client with vite's API at startup, and the readiness probe already covers
that: the sheep stays `Starting` until the server answers. The dog does not
need to know a compile happened.

The three scenarios differ only in the build block. That is the test of whether
this design is the right shape.
