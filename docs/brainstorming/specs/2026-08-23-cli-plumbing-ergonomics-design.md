# The CLI's error plumbing: design

**Date:** 2026-08-23
**Status:** design approved by Rin, awaiting spec review
**Scope:** `shep-cli`, plus `shep-core` and `shep-daemon` for Move 4. No behaviour change, no wire change, no new output.

## The ask

Rin, 2026-08-23, after writing `shep init` by hand:

> there was a lot of clunky, repeat code, like emit_error/emit_notice and match
> blocks like `Ok(target) => target, Err(code) => return code`. Maybe we could
> abstract these out to macros. I know having to always pass streams and fmt
> through doesn't make it easy.

She is right about the repetition. This spec argues the repetition is a symptom
of two structural things rather than a case for macros, and that fixing those
removes most of it in plain Rust.

## What is actually there, counted

| | |
|---|---|
| `emit_error` call sites | 91 |
| `emit_notice` call sites | 20 |
| of which discard the result with `let _ =` | **all of them** |
| `Ok(x) => x, Err(code) => return code` blocks | 38 |
| functions taking `streams: &mut Streams` | 84 |
| of those that ALSO take `fmt: Format` | **84** |
| `Streams { .. }` constructions in production | 5 |
| the same in test code | 92 |

Two of those numbers decide the design. **Every function that takes `streams`
also takes `fmt`**, so `fmt` is not an independent parameter; it is a
passenger. And **every** emit call discards its `io::Result`, so that decision
is uniform and can be made once.

## Why not macros

Not a matter of taste. Four specific costs, against a benefit two structural
changes deliver anyway.

- **A macro would bury the `let _ =`.** That is a real decision, not noise: a
  failed write to a closed stderr must not change the exit code. Hidden in an
  expansion it stops being reviewable. Named once on a documented method, it
  stays a decision.
- **Discoverability.** `streams.` autocompletes; `error!` does not. A method
  carries rustdoc, a signature, and go-to-definition.
- **Diagnostics.** A type error inside a macro expansion points at the macro.
- **`proc_macro` specifically** would mean a new crate in the workspace and
  its compile cost, bought for call-site ergonomics. This workspace already
  refuses `reqwest` over ninety crates of dependency; a proc-macro crate for
  this is the same trade in the wrong direction.

The residue after the three moves below may still want a small
`macro_rules!`. That is a better question asked against the residue than
against today's shape, and this spec deliberately does not answer it.

## Move 1: `fmt` lives in `Streams`

`Streams` already carries `style: Presentation`, and the field's own doc gives
the reason:

> Carried here rather than passed to `emit` on its own because `Streams`
> already reaches every command, and a global would break this crate's rule
> that presentation inputs are parameters, never a call inside the function
> that renders.

`fmt` is the same kind of value and got treated differently. `Format` is
`Copy`, so nothing about ownership objects.

**Checked before proposing it:** every call site that passes a `Format` other
than the ambient `fmt` is in test code (`status.rs`, `welcome.rs`,
`selector.rs`, `bleats.rs`, `output/mod.rs`). Production always forwards what
it was given. So no production call loses the ability to override, because
none of them does.

Effect: 84 signatures lose a parameter, and 91 emit sites lose an argument.
Cost: five production constructions and 92 test constructions gain a field.
The test cost is real and mechanical, and it makes those tests clearer, since
each now names the format it is exercising at the point it builds the streams.

**`emit` and `emit_error` keep their `Format` parameter.** They take a raw
`&mut dyn io::Write`, not a `Streams`, and one caller (`lib.rs:1272`) has no
`Streams` at all. They stay as they are; the methods below are an addition.

## Move 2: methods on `Streams`, replacing the four-argument call

```rust
impl Streams<'_> {
    /// Prints `message` as an error and hands back the code it printed.
    ///
    /// The write's own failure is discarded, deliberately: a closed stderr
    /// must not change what shep exits with.
    pub fn fail(&mut self, code: ExitCode, message: &str) -> ExitCode;

    /// Prints `message` as a notice.
    pub fn note(&mut self, code: &str, message: &str);
}
```

At a call site:

```rust
// before
let _ = emit_error(&mut *streams.err, fmt, ExitCode::Usage.code_str(), &message);
return ExitCode::Usage;

// after
return streams.fail(ExitCode::Usage, &message);
```

`fail` returning the code it printed is what collapses the two statements into
one. The code appears once instead of twice, so the pair cannot drift.

## Move 3: the match blocks are `?` in a costume

```rust
Ok(target) => target,
Err(code) => return code,
```

That is exactly what `?` does. It is written out because the enclosing
function returns `ExitCode` rather than `Result<_, ExitCode>`. The fix is the
ordinary Rust shape: a thin public wrapper over an inner function that returns
a `Result`.

```rust
pub async fn init(streams: &mut Streams<'_>, args: &InitArgs) -> ExitCode {
    match init_inner(streams, args).await {
        Ok(()) => ExitCode::Success,
        Err(code) => code,
    }
}

async fn init_inner(streams: &mut Streams<'_>, args: &InitArgs) -> Result<(), ExitCode> {
    let cwd = get_cwd(streams)?;
    let (path, format) = target(streams, &cwd, args)?;
    // ...
}
```

One match per command instead of 38 across the crate, and each body reads
straight through.

**Not every command wants this.** A verb whose whole body is a single
`connect_client` match gains nothing, and converting it would add a function
to save nothing. The rule: convert a command when it has **two or more** early
returns. Leave the rest.

## Move 4: `From` impls for the conversions that carry nothing else

Raised separately by Rin, 2026-08-23: there are a lot of `map_err` call sites,
and would a crate like `thiserror` map them automatically.

Counted across all three crates: **220 `map_err` sites.** They do two
different jobs, and only one of them is boilerplate.

| | count | can a `From` impl erase it |
|---|---|---|
| pure conversion, `.map_err(KvError::Io)?` | 72 | yes |
| adds context: a path, a URL, a field name | 148 | **no, and it must not** |

**Two thirds of them are not boilerplate.** `TargetError::Read { source, path }`
exists so the error names the file it could not read, and the source
`io::Error` does not carry that. Erasing those loses the path.

That is also load-bearing rather than incidental. This project already relies
on the absence of a blanket conversion: `shep-log-rotate`'s `Error` has no
`From<std::io::Error>` specifically so a caller cannot `?` past the point
where a path should be named. A `From` impl is exactly the thing that lets
somebody do that by accident later.

### Why not `thiserror`

Its two halves land differently and neither fits.

- **`#[error("...")]` collides with a rule already written.** IR-19 in
  `docs/idiomatic-rust.md` specifies "manual `Display` via
  `f.write_str(match ...)`". Adopting the derive means changing IR-19, which
  is a separate decision about house style and not one this cleanup should
  make on the way past.
- **`#[from]` is the half worth having**, and all it generates is a `From`
  impl, which is three hand-written lines. No dependency, no proc macro, no
  compile cost, no conflict.

So the crate would buy the half that is not wanted and collide with a rule
that already exists. `anyhow` is separately ruled out by IR-18, which permits
it only inside `shep`.

### What to write

**26 of shep's own error variants cover 66 of the 72 sites.** `KvError::Io`
appears eight times, `SinkError::Transport` seven, `FetchError::Transport`
six. Each becomes one `From` impl and the call sites become a bare `?`.

The remaining six are conversions into foreign types (`std::io::Error::other`,
`serde::de::Error::custom`). The orphan rule blocks a `From` impl for those,
so they stay as they are.

### The guard rail, which is the point of this move

**Add a `From` only where the variant carries the source and nothing else.**

The moment a variant has a second field, the explicit `map_err` is the
feature, not the noise: it is what forces the next person to supply the path.
So this move is not "convert every `map_err` it can reach", it is "convert
exactly the ones where there was never anything else to say".

Writing a `From` is also a claim about the future: it says this variant will
never want context. Where that looks doubtful, leave the `map_err`. A missing
`From` costs one visible line; a wrong one costs a path that quietly stops
being reported.

### Sequencing

Independent of Moves 1 to 3 and of no fixed order relative to them, since it
touches error construction rather than the streams plumbing. Its own commit,
or one per crate if the diff reads better split.

## What must not change

- **Every byte on stderr.** Same code string, same message, same format in
  both `--format table` and `--format json`.
- **Every exit code**, including the ones only reached when a write fails.
- **`emit`, `emit_error`, `emit_notice`** stay public with their current
  signatures. Callers outside a `Streams` still need them.
- **No new dependency.**

## Sequencing, and why it is three commits

Each move is independently green and independently revertable. That matters
more than usual here because this touches 91 emit sites, 84 signatures and 38
matches, which is exactly the diff shape where a behaviour change hides in the
noise.

1. **`fmt` into `Streams`.** Largest mechanical diff, zero logic.
2. **`fail`/`note` methods**, and the call sites moved onto them.
3. **Inner functions**, one command at a time, only where there are two or
   more early returns.
4. **`From` impls** for the 26 source-only variants. Independent of the other
   three and orderable anywhere among them.

## Testing

The suite already covers behaviour; the risk here is silent drift, so the
useful test is a **before-and-after byte comparison**.

- Before starting, snapshot a representative error in both formats: one
  refusal from a command with a `Streams`, one from `lib.rs:1272`'s
  `Streams`-less path, and one notice. `insta` is already a dev-dependency of
  this crate.
- Those snapshots must be byte-identical at the end of all three commits. A
  changed snapshot is a bug in the refactor, never an update to accept.
- `cargo test --workspace --all-features` green after each commit, not only
  at the end.

## Assumptions

1. **Not macros, at least not first.** Revisit against the residue.
2. **`proc_macro` is out** regardless.
3. **`fmt` moves into `Streams` rather than becoming a global**, keeping this
   crate's stated rule that presentation inputs are parameters.
4. **`emit_error` and friends stay** as free functions.
5. **Inner functions only where there are two or more early returns**, so the
   refactor does not add ceremony to commands that do not need it.
6. **The 92 test constructions get the field rather than a `Streams::new`
   helper.** A constructor would hide which format a test exercises, and that
   is the thing worth reading in a test that asserts on rendered output.
7. **No error crate.** `thiserror` collides with IR-19's manual `Display`, and
   the half of it worth having is three lines written by hand.
8. **`From` only for source-only variants.** The 148 context-carrying
   `map_err` sites stay exactly as they are.
