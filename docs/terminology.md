# shep — name & terminology

Approved by Rin 2026-08-07. **shep** = the project, the binary, the brand. A sheepdog
watching your processes. Playful shepherd/sheep/sheepdog terminology runs through the
CLI, docs, and type names.

Crates: `shep-core`, `shep-daemon`, `shep-client`, `shep`. Binary: `shep`.
Free on crates.io (verified 2026-08-07): `shep`, `shepd`, plus reserves `bleat`,
`sheepdog`, `fleece`, `crook` if satellite crates ever want them.

## The lexicon

| Concept | Conventional | shep says | Where it applies |
|---|---|---|---|
| the daemon | daemon/supervisor | **the shepherd** — ONLY the shepherd; "dog" now means plugins (collision removed 2026-08-07) | docs, log messages, TUI header |
| managed processes (plural/list) | process list | **the flock** — ALWAYS the plural term; never bare plural "sheep" in docs/CLI (kills sg/pl ambiguity; ruled 2026-08-07) | `shep flock` (list), `Flock` type, docs |
| one managed process (singular) | process/app | **a sheep** (singular ONLY) / process (precise) | docs may say sheep; API types stay `Process`-clear. RESERVED for managed processes |
| plugin process (first-party in-binary, or third-party speaking the client protocol) | plugin/module | **dog** (pl. dogs) | `shep enable metrics` (built-in); `shep adopt <name> <path>` (third-party — `enable --exec` is a hidden pm2-spelled alias); `shep dogs` (list), hidden `shep dog <name>` runs one; `dog`-tagged in flock listing. Decided 2026-08-07 |
| child process of a sheep (process-tree member) | child process | **lamb** (pl. lambs) | `shep describe` tree view ("the sheep and her lambs"), tree-kill docs. Decided 2026-08-07 |
| namespace / group | namespace | **fold** (also: paddock) | `shep fold <name>`, `Fold` type |
| app config file | ecosystem.config.js | **Flockfile** (`Flockfile.toml` / `.yaml` / `.json`) | config discovery, docs |
| logs | logs | **bleats** | `shep bleats [--follow]`; `shep logs` stays as alias |
| webhook alert | alert/notification | **bark** 🐕 | `[bark]` config section, `shep barks` history, alert module |
| MCP agentic interface | MCP server | **the whistle** | `shep whistle` (serves MCP), docs metaphor: agents whistle, the shepherd and dogs respond |
| graceful shutdown | stop | **`shep thatlldo [target]`** | easter-egg alias for graceful stop — real herding command for "work's done" |
| freeze the running list | save | **save** | `shep save` writes the muster roll |
| register a third-party plugin | install | **adopt** | `shep adopt <name> <path>`; `enable --exec` is a hidden alias |
| drop a third-party plugin | uninstall | **rehome** | `shep rehome <name>` — unlike `disable`, it forgets the registration |
| resurrect saved state | resurrect | **muster** | `shep muster` assembles the flock from the roll |
| TUI dashboard | monit/dash | **lookout** | `shep lookout`; `shep dash` alias |
| host machine | host | **the heft** (sheep bound to their hill) | subtle: docs + host-metrics naming |
| graceful reload (an overlap, not zero downtime) | reload | reload (verb stays) — strategies **come-bye** / **away** if we ever name them | reload internals, maybe strategy flags |
| kill escalation | kill | kill (stays — clarity beats cuteness on destructive ops) | — |
| change instance count | scale | **stock** — "stocking rate" is the real husbandry term for how many animals a piece of land runs, not a pun; `shep scale` stays as an alias | `shep stock <name> <count>` |
| send a signal to one process | signal | **signal** — stays plain, deliberately: `shep signal web SIGKILL` is a loaded gun, and rule 2 below keeps destructive/precise operations free of whimsy, the same reason `kill` stayed `kill` | `shep signal <selector> <signal>` |
| write to a process's stdin | sendline | **whisper** — completes the pair `bleats` already started: bleats is what the sheep says to you, whisper is what you say to the sheep, down a channel nobody else hears; `shep sendline` stays as an alias | `shep whisper <selector> <line>` |
| ad-hoc key/value store | kv store | **set** / **get** / **unset** — plain names, not themed | `shep set`, `shep get`, `shep unset` |
| a marker a dog attaches to a sheep | deploy tag | **smit** (a farmer's paint mark on a sheep) | `ProcessInfo.smit`, the SMIT column in `shep flock`/lookout; shep stores and paints the string verbatim and never parses it |

## Usage rules (readability > theme)

1. **Straight verbs always work.** `start`, `stop`, `restart`, `list`, `logs`, `delete`
   are first-class aliases forever. Sheep terms are the personality layer, not a wall.
   (Open question in goals.md: which set leads in `--help`.)
2. **Destructive/precise operations keep plain names.** `kill`, `delete`, exit codes,
   error messages — zero whimsy where misreading costs a process.
3. **Types may be themed when self-evident** (`Flock`, `Fold`, `Bark`), never when
   opaque (`Heft` as a struct name = no; "host" it is).
4. **Docs voice**: playful in prose and examples, exact in reference material. The
   README can say "shep keeps your flock alive"; the config reference says "process".
5. **Log/error output**: technical register. The dog barks in webhooks, not in stderr.
