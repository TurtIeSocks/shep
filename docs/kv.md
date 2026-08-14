# The kv store — a junk drawer with a lock on it

Three verbs — `shep set`, `shep get`, `shep unset` — for the small stuff
that has nowhere else to live. This is the practical guide to using them.
The store gets one line in spec [§5](specs/shep-v1.md#5-configuration) —
"file-locked JSON; not the primary config path" — and this doc is the rest
of that sentence.

## What this is for, and what it is not

Two other files already configure everything shep runs: a **Flockfile**
configures a sheep (its script, its instances, its restart policy), and
`shep.toml` configures the shepherd itself and its dogs. Neither has a
field for "the port the last on-call engineer picked" or "a feature flag a
provisioning script wants to leave a note about." That is what this store
is for — ad-hoc operator notes and runtime tweaks a dog reads, not
anything that shapes how a sheep is supervised.

If you are reaching for this store to configure a *sheep*, put it in the
Flockfile instead. If you are reaching for it to configure a *dog*, put it
under `[dog.<name>]` in `shep.toml`. This store is what is left over —
small, flat, and explicitly not the primary config path.

## The three verbs

```
shep set bark.cooldown 30s
shep get bark.cooldown
shep get
shep unset bark.cooldown
shep unset --all
```

`shep set <key> <value>` writes a key, replacing whatever was there.
`shep get <key>` prints one value; `shep get` with no key lists everything.
`shep unset <key>` removes one key; `shep unset --all` empties the store —
a flag, not a key named `all`, because nothing stops you having a key
called `all` and `shep unset all` would then mean something different
depending on your own store.

A key that was never there answers `NotFound` on `get` and on `unset`
(exit code 3): a script can write `shep get feature.flag || echo default`
and trust the exit code rather than parsing output to tell "empty" from
"missing."

## The key grammar

A key is `[A-Za-z0-9._-]`, one to 128 bytes, and cannot start with a dot.
**A dot is part of a name, not a path.** `bark.cooldown` is one flat key;
there is no nested object behind it, and `shep get bark` will not find it.
This is a deliberate departure from pm2's own dotted/colon store — a
nesting grammar here would be a second config language, with its own
quoting rules, for a store that is explicitly not the primary one. The
narrow alphabet is also why `shep get $key` never needs quoting in a
script.

## Values

A value is a string, capped at 4 KiB. The store is read whole on every
access, and the cap is what keeps `kv.json` from quietly becoming a blob
store — it is the smallest file under `$SHEP_HOME`, and it should stay
that way.

## Where it lives

`$SHEP_HOME/kv.json`, mode `0600`, and safe to keep in a dotfiles
repository: it is written through a `BTreeMap`, so keys are always in
sorted order and two writes of the same content produce byte-identical
files. Diff it, `git add` it, whatever you'd do with any other small
config file.

## No shepherd required

`shep set`/`get`/`unset` never touch the socket — they read and write the
file directly, the same way `shep enable` writes `shep.toml` with nothing
listening. That is what makes this store usable during provisioning,
before any shepherd has ever booted on the machine.

A dog reads the same store through `shep_core::kv` rather than over the
wire, for one reason: a `0600` file inside a `0700` `$SHEP_HOME`, opened by
a process running as the same user, already has every property the socket
would have bought it, so the socket would have cost a round trip for
nothing.

## Concurrent writers

Two `shep set` invocations racing — two provisioning scripts, or an
operator and a dog — are serialized by an exclusive advisory lock on a
sibling `kv.json.lock`, so neither one's write is silently lost. Whichever
loses the race simply waits its turn.
