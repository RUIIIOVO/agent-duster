<div align="center">

# Agent Duster

**The resource manager for your AI agents.**

Search, share, clean, and migrate the MCP servers, skills, memories, and sessions
scattered across Claude Code, Codex, Gemini CLI, omp, and more.

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/built_with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-macOS-lightgrey.svg)]()
[![Status](https://img.shields.io/badge/status-in_development-yellow.svg)]()

**English** · [简体中文](README.zh-CN.md)

</div>

---

## Why

Every AI coding agent hoards its own silo under `~`:

- The **same MCP server** declared 3 times in 3 formats (JSON / TOML / JSON).
- The **same skill** copied into every agent's directory — silently drifting apart.
- **Memories** (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`…) written N times, never in sync.
- **Gigabytes** of stale sessions, logs, caches, and `node_modules` you can't tell apart.
- Switching tools means reconfiguring everything from scratch.

Agent Duster gives you one CLI to see, search, and manage all of it.

## Highlights

- 🔍 **Unified search** — one query across every agent's conversations, then `open` to read the whole turn. CJK-friendly.
- 🧬 **Duplicate & drift detection** — `skill copies` finds byte-identical copies and same-name-but-diverged skills, wherever they live; `skill link` converges them into one linked source of truth.
- 🧹 **Three cleanup verbs** — `clean` for regenerable junk, `prune` for stale resources, `uninstall` for a whole agent. Split by what it costs you if the tool guesses wrong, and everything is explained.
- 🔁 **One declaration, many agents** — `mcp sync` copies an MCP server into other agents and rewrites it into each one's format; `skill link` shares one copy on disk. A conversion that would drop a field is refused, never silently truncated.
- 🩺 **Doctor** — six checks in one pass: plaintext credentials (masked, never printed in full), MCP reachability, skill metadata, config syntax, dangling references, SQLite integrity.
- 🔒 **Safe by default** — dry-run first; every item is listed and justified before it goes; **deletion is permanent**, and anything non-regenerable is archived to a path you can see first. Fully offline, zero telemetry, zero accounts.

## Quick Start

> ⚠️ Alpha. Every command in the table below runs today. What is still missing is listed
> under [Not built yet](#not-built-yet).

```bash
duster                          # no arguments on a terminal: pick from the menu
duster scan                     # discover agents & index resources
duster status                   # overview: agents / sizes / issues
duster search "auth middleware"
duster mcp list                 # every MCP server, merged across the agents declaring it
duster session list             # conversations by project, age and size
duster clean                    # caches and logs, dry-run by default
duster prune --older-than 90d   # stale skills / sessions, item by item
```

## Commands

| Command | What it does |
|---|---|
| `duster` | With no arguments on a terminal: a menu. Pick a command and duster asks for whatever it needs, then runs the same code path the flags do. Piped, or with `--json`: prints help instead. |
| `duster scan` | Detect installed agents, index MCP / skills / memories / sessions. Incremental by default; unknown paths are reported, never touched. |
| `duster status` | Per-agent disk usage, resource counts, and when each was last scanned. |
| `duster search <query>` | Full-text search across every agent's conversations; `--agent` narrows it (comma-separated for several), `--limit` sizes it. Hits are the extracted conversation text, not the raw transcript line. `duster open <id>` prints the whole turn a hit came from — hand it a conversation id instead and it prints the conversation, saying so. |
| `duster session list \| show \| export \| prune` | Browse conversations across all agents, newest first; filter by `--agent` / `--project` / `--older-than` / `--min-bytes`. `show` prints one conversation as prose: tool output collapses to one line each (`## tool · read · 4.3 KB`) and long bodies stop at 40 lines, because you came to read the conversation, not the return value of `read()`; `--full` gives you everything. `export` writes it out as Markdown or JSON, always in full. `prune --older-than 90d` compresses in place to `.zst` and deletes the original only after the decompressed bytes hash back to the same BLAKE3 — search and open still read it afterwards. |
| `duster memory list \| show` | Read-only view over `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`, Qoder's per-project memory trees and SQLite-backed stores alike. `list` gives you the key, `show` prints that one memory. |
| `duster mcp list \| show \| diff \| sync \| ping` | One row per server, merged across every agent that declares it, with each agent's dialect. `show` prints one in full with env values and headers masked. `diff --from A --to B` compares the same server as two agents declare it. `sync <name> --to codex,opencode` copies one declaration into other agents, converting formats; dry-run by default, snapshots each file before writing, and **refuses** any target whose format would drop a field. `ping` starts each server once, says `initialize`, and hangs up. |
| `duster skill copies \| link` | Cross-agent skill management. `copies` lists skills that exist in more than one place — including two copies inside one agent — and which of them drifted; `link` converges duplicates onto one copy on disk — edit once, effective everywhere. |
| `duster diff <a> <b>` | Compare two files or two folders, line by line. `--no-line-level` keeps it to which entries differ; `--include-same` also lists the identical ones. |
| `duster doctor` | Six checks in one pass: `secrets` (needs `--secrets`), `skill-metadata`, `config-syntax`, `dangling-reference`, `sqlite-integrity`, `mcp-reachability` (needs `--ping`). `--check <name>` runs only the ones you name. Every check gets a line in the report — the ones that were skipped say so, and say which flag turns them on. |
| `duster clean` | Reclaim **regenerable junk**: SQLite free pages, orphan WAL/SHM, caches, logs, temp leftovers. Dry-run by default, `--yes` executes, **really deleted, no copy kept**. Finishes by reporting the other two buckets (stale resources / installed software) and where they go. Never removes installed software. |
| `duster prune --older-than 30d` | Reclaim **stale resources you created**: skills that neither changed for N days nor appear in any *invocation record* (a name mentioned in prose is not evidence — see below), expired backups and archives; sessions older than N days get compressed, not deleted. `--keep-generations` additionally clears surplus copies of per-generation resources such as database backups, which are redundant by count rather than by age. Always lists every item first (what it is / why it's stale / what breaks), then deletes permanently — non-regenerable content is archived to `~/agent-duster-exports/` beforehand. |
| `duster uninstall <agent>` | **Remove one agent entirely**, in three parts: the folders and files the manifest declares as its own; its keys inside config files *other* agents own (removed surgically, whole-file snapshot first); and how the software itself was installed — that uninstall command is printed and **never run**, unless you pass `--run-package-manager`. `--data-only` opts out of the last two and reports how many edits and hints it skipped. Requires typing the agent id to confirm; `--export-first` (on by default) archives sessions and memory, `--keep sessions,memory` leaves them in place. |

### Not built yet

- `duster migrate --from <A> --to <B>` — move everything between two agents in one planned, idempotent pass.
- `duster mcp remove` and `duster skill unlink \| remove` — deleting a declaration still means editing the file yourself, or `uninstall`ing the whole agent.
- `duster memory merge \| export` — the memory view is read-only today.
- Pruning stale MCP declarations. `prune` handles files; dropping one server out of a config file others still use rides on the same machinery as `mcp sync` and lands with it.
- `search` over memories, skills and MCP configs. Today it searches conversation text only.

### Three verbs, split by what a wrong guess costs you

| Command | Targets | Who made it | Cost of a wrong delete | How it deletes |
|---|---|---|---|---|
| `clean` | SQLite free pages (VACUUM), orphan WAL/SHM, caches, logs, temp leftovers | The program itself | Zero — it rebuilds | Really deleted, no copy |
| `prune` | Skills untouched for N days **with no invocation on record**, surplus backup generations (with `--keep-generations`); sessions older than N days get compressed | **You configured or wrote it** | Reconfiguration, possibly unrecoverable | List every item → confirm → archive → permanent delete |
| `uninstall` | One agent's entire data and config | The installer | Reinstall + reconfigure + history gone | Type the agent id → export → permanent delete |

These are three commands rather than three tiers of one command because the **consent model
differs**: `clean` needs no per-item approval, `prune` demands it. Muscle memory attaches to
commands, not flags — someone who has typed `duster clean --yes` fifty times has trained
themselves to stop reading the output, and a confirmation prompt buried in that same verb is
arguing with a reflex.

`clean` splits internally into `l0` (lossless: VACUUM, orphan WAL) and `l1` (regenerable:
caches, logs); both are on by default. `--older-than` belongs to `prune` only and accepts
`30d` / `60d` / `90d` or any `<N>d`.

In the menu, `clean` and `prune` both show what they would touch as one aligned checkbox table
(agent / kind / size it frees / days idle / path) instead of the full report: space toggles,
Enter runs the checked rows, everything starts checked, and whatever you uncheck is dropped from
the run. The full `what / why / impact` report is the command-line path (`--dry-run`), which
stays redirectable to a file.

Every other list view in the menu is a browser rather than a dump: `session list`,
`memory list`, `mcp list`, `skill copies` and `search` page through their rows (↑/↓ to move,
←/→ to page, Enter to open, Esc to go back) and Enter runs the detail command on the row you
picked — no hand-copying a key into a second command. Anything scoped by `--agent` asks with
a checklist of your agents carrying each one's size and reclaimable bytes, **everything checked
to start** — uncheck what you want left alone. Space toggles the row under the cursor, `a`
toggles the whole list, and the first row (`All agents`) really does check every row with it;
submitting with all of them checked is exactly like omitting the flag. Every checklist marks
the cursor with `❯` in its own column, separate from the `[x]` box, so the row you are on
survives a copy-paste and a terminal that drops color.

Every list in the menu — the command menu itself included — is the same widget: it never draws
more rows than the terminal has, and it reports where you are (`2-20 of 20`) whenever the list
is taller than one screen. ←/→ page in every one of them; a list that fits on one screen says
so by not offering the keys.

`duster status` reports **reclaimable** bytes, not occupied bytes. They differ for L0: a
784 MB SQLite log store whose pages are 98% free reports 774 MB reclaimable, because the
live rows stay. The per-agent `CLEANABLE` column shows occupancy; the summary line shows
what you actually get back.

**Installed software is never cleaned.** Extensions, plugins, bundled binaries, and
`node_modules` are classified as `install`: counted in `duster status`, invisible to both
`clean` and `prune`. If removing it would mean reinstalling, neither command touches it —
no tier, no flag, no exceptions. The only way to move it out is `duster uninstall <agent>`,
a separately named verb that makes you name your target.

**"Used" means invoked, not mentioned.** A skill is only stale if nothing invoked it — an
actual invocation record in a transcript: Claude Code's `Skill` tool call, an `skill://<name>`
argument, a read of that skill's `SKILL.md`. The name turning up in prose is explicitly *not*
evidence, because skill names are ordinary words (`pdf`, `docx`, `frontend`) and because every
agent lists its whole skill catalogue in the instructions of every session — counting that
would mark all of them as used forever. Until `duster scan` has collected invocation evidence,
no skill is judged stale at all: "we haven't looked" and "we looked and found nothing" are
different facts and duster will not print one as the other.

**Backups are judged by count, not by age.** A resource can declare `keep_generations = 2` in
its manifest: the newest two survive and the rest are surplus. The 8th copy of one database is
redundant whether it is 16 days or 16 months old, so `--older-than` never reaches it — which is
why surplus generations need `--keep-generations`, list separately, and say in their own `why`
that they were picked by count. Without the flag, prune reports how many surplus copies exist
and how much they weigh, and touches nothing.

### On a trash can: there isn't one

duster has no trash. A cleanup tool that leaves `df` unchanged has committed the worst
error available to it — and the trash itself becomes the next thing that needs cleaning.
Four stronger guarantees replace it:

- **You always see it first.** Every item carries what it is, why it was judged removable,
  what breaks, and whether it made it into the archive — not a bare list of paths.
  `--yes` skips the question, **never the list**.
- **Non-regenerable content is packed before it goes.** Into
  `~/agent-duster-exports/<op>-<date>.tar.zst`, with the path and size printed in the output;
  `tar -xf` is the restore. It belongs to you, and duster will never garbage-collect it.
  It compresses well — the user-authored content of all 77 skills on this machine packs to 7.9 MB.
- **Sessions are compressed, not deleted.** The original is removed only after the archive
  decompresses to a byte-identical copy. Verifiable correctness beats undo.
- **Config edits take a different path.** Dropping one MCP server out of `~/.claude.json`
  isn't a file deletion — it rewrites a file that still holds twenty other things. Those
  operations snapshot the whole file first.

`--json` is global and turns any command into one line of JSON. The rest belong to the
commands that have them: `--yes` on `clean` / `prune` / `session prune` / `mcp sync`,
`--dry-run` on all of those except `mcp sync` (which has no plan-only flag because plan-only
is what it does without `--yes`), `--confirm <agent>` on `uninstall`, `--full` on `session show`
and `open`, `--agent <id>[,<id>]` wherever
a scope makes sense. In `--json` mode nothing destructive runs without `--yes`: there is no
terminal to ask at, so duster prints the plan and exits 4.

## Principles

1. **Manage, never run** — no request proxying, no model hosting, no background daemon.
2. **Zero telemetry, zero accounts, zero cloud** — fully offline by default.
3. **Read-only by default** — destructive ops require explicit flags; every item is listed before it goes, and non-regenerable content is archived first.
4. **Don't touch what you don't understand** — only adapter-claimed paths are operated on.
5. **Your data stays yours** — the index is disposable; delete it and rescan anytime.

## Supported Agents

Claude Code · Codex · oh-my-pi (omp) · Gemini CLI · Cursor · GitHub Copilot CLI · Kimi CLI · OpenCode · Qoder · generic MCP clients (VS Code / Windsurf / Cline, *planned*)

Adding a new agent usually takes **one declarative TOML manifest — no code, no recompile**.

## License

[MIT](LICENSE)
