# shep — name & terminology

Approved by Rin 2026-08-07. **shep** = the project, the binary, the brand. A sheepdog
watching your processes. Playful shepherd/sheep/sheepdog terminology runs through the
CLI, docs, and type names.

Crates: `shep-core`, `shep-daemon`, `shep-client`, `shep-cli`. Binary: `shep`.
Free on crates.io (verified 2026-08-07): `shep`, `shepd`, plus reserves `bleat`,
`sheepdog`, `fleece`, `crook` if satellite crates ever want them.

## The lexicon

| Concept | Conventional | shep says | Where it applies |
|---|---|---|---|
| the daemon | daemon/supervisor | **the shepherd** (affectionately: the dog) | docs, log messages, TUI header |
| managed processes (plural/list) | process list | **the flock** — ALWAYS the plural term; never bare plural "sheep" in docs/CLI (kills sg/pl ambiguity; ruled 2026-08-07) | `shep flock` (list), `Flock` type, docs |
| one managed process (singular) | process/app | **a sheep** (singular ONLY) / process (precise) | docs may say sheep; API types stay `Process`-clear. RESERVED for managed processes |
| plugin process (first-party in-binary, or third-party speaking the client protocol) | plugin/module | **lamb** (pl. lambs) | `shep enable metrics`, `shep enable --exec <path> <name>` (third-party), `shep lambs` (list), hidden `shep lamb <name>` runs one; `lamb`-tagged in flock listing. Decided 2026-08-07 |
| namespace / group | namespace | **fold** (also: paddock) | `shep fold <name>`, `Fold` type |
| app config file | ecosystem.config.js | **Flockfile** (`Flockfile.toml` / `.yaml` / `.json`) | config discovery, docs |
| logs | logs | **bleats** | `shep bleats [--follow]`; `shep logs` stays as alias |
| webhook alert | alert/notification | **bark** 🐕 | `[bark]` config section, `shep barks` history, alert module |
| MCP agentic interface | MCP server | **the whistle** | `shep whistle` (serves MCP), docs metaphor: agents whistle commands to the dog |
| graceful shutdown | stop | **`shep thatlldo [target]`** | easter-egg alias for graceful stop — real herding command for "work's done" |
| resurrect saved state | resurrect | **muster** | `shep muster`, snapshot = the muster roll |
| TUI dashboard | monit/dash | **lookout** | `shep lookout`; `shep dash` alias |
| host machine | host | **the heft** (sheep bound to their hill) | subtle: docs + host-metrics naming |
| zero-downtime reload | reload | reload (verb stays) — strategies **come-bye** / **away** if we ever name them | reload internals, maybe strategy flags |
| kill escalation | kill | kill (stays — clarity beats cuteness on destructive ops) | — |

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
