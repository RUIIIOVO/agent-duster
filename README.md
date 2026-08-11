<div align="center">

# Agent Duster

**The resource manager for your AI agents.**

Search, dedupe, clean, and migrate the MCP servers, skills, memories, and sessions
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

- 🔍 **Unified search** — one query across every agent's sessions, memories, skills, and MCP configs. CJK-friendly.
- 🧬 **Dedupe & drift detection** — find byte-identical copies and same-name-but-diverged skills; converge them into one linked source of truth.
- 🧹 **Tiered cleanup** — from lossless (SQLite vacuum) to heavy assets (`node_modules`), each tier opt-in, everything explained.
- 🚚 **Migration** — move MCP / skills / memory between agents with an explicit plan; lossy conversions are reported, never silent.
- 🩺 **Doctor** — locate plaintext API keys (masked, never printed in full), broken references, and config errors.
- 🔒 **Safe by default** — dry-run first, trash-based deletion, byte-exact restore. Fully offline, zero telemetry, zero accounts.

## Quick Start

> ⚠️ In development — not yet released.

```bash
duster scan          # discover agents & index resources
duster status        # overview: agents / sizes / issues
duster search "auth middleware"
duster clean --older-than 30d   # dry-run by default
```

## Commands

| Command | What it does |
|---|---|
| `duster scan` | Detect installed agents, index MCP / skills / memories / sessions. Incremental by default; unknown paths are reported, never touched. |
| `duster status` | Per-agent disk usage, resource counts, and detected issues. |
| `duster search <query>` | Full-text search across all agents. Filter by `--agent` / `--kind` / `--project` / time. `duster open <hit-id>` jumps to the source file. |
| `duster clean [--level L0..L2] [--older-than 30d]` | Tiered cleanup (see below). Dry-run by default; `--yes` executes; deletions go to trash. Never removes installed software. |
| `duster restore [<id>]` | Restore from trash — byte-identical to before. |
| `duster doctor [--secrets]` | Health check: plaintext credentials (masked), unreachable MCP, missing skill metadata, config syntax errors, SQLite integrity. |
| `duster skill list \| dedupe \| drift \| link \| unlink \| remove` | Cross-agent skill management. `link` converges duplicates into a content-addressed store — edit once, effective everywhere. |
| `duster mcp list \| show \| sync \| diff \| ping \| remove` | Global MCP registry. `sync <name> --to codex,gemini` distributes one server to multiple agents with automatic format conversion. |
| `duster memory list \| show \| merge \| export` | Unified view over `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` etc.; merge and re-export per agent. |
| `duster session list \| search \| show \| export \| prune` | Browse sessions by project / time / size / agent; export to Markdown / JSON; prune with `--older-than 90d --to-trash`. |
| `duster migrate --from <A> --to <B>` | Migrate between agents. Outputs a plan first: create / skip / **LOSSY** / conflict / unsupported. Idempotent, snapshot before execution. |
| `duster diff <a> <b>` | Side-by-side diff of any two resources. |

### Cleanup tiers

| Tier | Target | Cost | Default |
|---|---|---|---|
| **L0** | SQLite free pages (VACUUM), orphan WAL/SHM, `.tmp-*` crash leftovers | Lossless — not a single row is lost | On |
| **L1** | Caches, logs, temp dirs, runtime leftovers | Auto-rebuilt, agent keeps working | On |
| **L2** | Skills & MCP servers untouched for N days, expired backups & archives; sessions older than N days get compressed, not deleted | Nothing breaks, but rebuilding costs manual work (e.g. logging back in) | Off — needs `--older-than` + confirmation |

`--older-than` accepts `30d` / `60d` / `90d` or any `<N>d`. L2 always shows the full item
list and asks again before touching anything.

`duster status` reports **reclaimable** bytes, not occupied bytes. They differ for L0: a
784 MB SQLite log store whose pages are 98% free reports 774 MB reclaimable, because the
live rows stay. The per-agent `CLEANABLE` column shows occupancy; the summary line shows
what you actually get back.

**Installed software is never cleaned.** Extensions, plugins, bundled binaries, and
`node_modules` are classified as `install`: counted in `duster status`, invisible to
`duster clean`. If removing it would mean reinstalling, duster won't remove it — no tier,
no flag, no exceptions.

Global flags: `--dry-run` (default for destructive ops) · `--yes` · `--json` · `--agent <id>` · `--quiet` · `--no-color`

## Principles

1. **Manage, never run** — no request proxying, no model hosting, no background daemon.
2. **Zero telemetry, zero accounts, zero cloud** — fully offline by default.
3. **Read-only by default** — destructive ops require explicit flags; deletions are always recoverable.
4. **Don't touch what you don't understand** — only adapter-claimed paths are operated on.
5. **Your data stays yours** — the index is disposable; delete it and rescan anytime.

## Supported Agents

Claude Code · Codex · oh-my-pi (omp) · Gemini CLI · Cursor · GitHub Copilot CLI · Kimi CLI · OpenCode · Qoder · generic MCP clients (VS Code / Windsurf / Cline, *planned*)

Adding a new agent usually takes **one declarative TOML manifest — no code, no recompile**.

## License

[MIT](LICENSE)
