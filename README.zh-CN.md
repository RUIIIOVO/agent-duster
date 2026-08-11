<div align="center">

# Agent Duster

**AI Agent 的资源管理器。**

统一检索、去重、清理、迁移散落在 Claude Code、Codex、Gemini CLI、omp
等各家 AI 工具里的 MCP、Skills、记忆与会话。

[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/built_with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-macOS-lightgrey.svg)]()
[![Status](https://img.shields.io/badge/status-开发中-yellow.svg)]()

[English](README.md) · **简体中文**

</div>

---

## 为什么需要它

每个 AI 编码 agent 都在 `~` 下囤积自己的数据孤岛：

- **同一个 MCP server** 在 3 个 agent 里声明了 3 遍，3 种格式（JSON / TOML / JSON）。
- **同一个 skill** 复制进每个 agent 的目录——然后悄悄各改各的，漂移到行为不一致。
- **记忆文件**（`CLAUDE.md`、`AGENTS.md`、`GEMINI.md`…）写了 N 遍，改一处其余不同步。
- 陈旧会话、日志、缓存、`node_modules` 堆出**好几个 GB**，分不清哪些能删。
- 换个工具，一切从零重配。

Agent Duster 用一个 CLI 让你看清、搜到、管住这一切。

## 特性

- 🔍 **统一检索** — 一条命令跨所有 agent 搜会话正文、记忆、skill、MCP 配置，中文可搜。
- 🧬 **去重与漂移检测** — 找出逐字节相同的副本和同名但内容已分叉的 skill，收敛为一份链接引用的唯一来源。
- 🧹 **分级清理** — 从无损（SQLite VACUUM）到重资产（`node_modules`）共 5 级，每级独立开关，每项都解释清楚。
- 🚚 **跨 agent 迁移** — MCP / skills / 记忆整体搬家，先出计划再执行；有损转换明确标注，绝不静默丢弃。
- 🩺 **体检** — 定位明文 API key（只显示掩码，绝不完整打印）、失效引用、配置语法错误。
- 🔒 **默认安全** — 默认 dry-run，删除进回收站，可逐字节还原。完全离线、零遥测、零账号。

## 快速开始

> ⚠️ 开发中，尚未发布。

```bash
duster scan          # 发现 agent 并建立索引
duster status        # 总览：agent / 体积 / 问题数
duster search "鉴权中间件"
duster clean --older-than 30d   # 默认 dry-run
```

## 命令一览

| 命令 | 作用 |
|---|---|
| `duster scan` | 探测已安装 agent，索引 MCP / skills / 记忆 / 会话。默认增量；不认识的路径只上报、不触碰。 |
| `duster status` | 每个 agent 的磁盘占用、资源数量、检测到的问题。 |
| `duster search <query>` | 跨 agent 全文检索，支持 `--agent` / `--kind` / `--project` / 时间过滤；`duster open <hit-id>` 直接打开原文件。 |
| `duster clean [--level L0..L2] [--older-than 30d]` | 分级清理（见下表）。默认 dry-run，`--yes` 才执行，删除全部进回收站。**永不卸载已安装的软件**。 |
| `duster restore [<id>]` | 从回收站原路还原，逐字节一致。 |
| `duster doctor [--secrets]` | 体检：明文凭据（掩码显示）、MCP 可达性、skill 元数据缺失、配置语法错误、SQLite 完整性。 |
| `duster skill list \| dedupe \| drift \| link \| unlink \| remove` | 跨 agent skill 管理。`link` 把重复项收敛到内容寻址库——一处修改，全局生效。 |
| `duster mcp list \| show \| sync \| diff \| ping \| remove` | 全局 MCP 注册表视图。`sync <name> --to codex,gemini` 一次分发到多个 agent，自动完成格式转换。 |
| `duster memory list \| show \| merge \| export` | 把 `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` 等归一为统一记忆视图，支持合并与按 agent 导出。 |
| `duster session list \| search \| show \| export \| prune` | 按项目 / 时间 / 体积 / agent 浏览会话；导出 Markdown / JSON；`prune --older-than 90d --to-trash` 批量清理。 |
| `duster migrate --from <A> --to <B>` | 跨 agent 迁移。先输出计划：新增 / 跳过 / **有损** / 冲突 / 不支持；幂等，执行前自动快照。 |
| `duster diff <a> <b>` | 任意两个资源的并排对比。 |

### 清理级别

| 级别 | 清什么 | 代价 | 默认 |
|---|---|---|---|
| **L0** | SQLite 空闲页（VACUUM）、孤儿 WAL/SHM、`.tmp-*` 崩溃残留 | 无损，数据一条不少 | 开 |
| **L1** | 缓存、日志、临时目录、运行时残留 | 自动重建，agent 照常用 | 开 |
| **L2** | 超过 N 天没碰过的 skill / MCP、过期的备份与归档；超过 N 天的会话改为压缩存档（不删） | 功能不缺，但重建有人工代价（例如要重新登录） | 关 —— 需 `--older-than` + 二次确认 |

`--older-than` 支持 `30d` / `60d` / `90d` 三个档位，也可以直接写 `<N>d`。
L2 一定先列出全部条目、再问一次才动手。

`duster status` 报的是**能拿回多少**，不是**占了多少**——这两个数在 L0 上不相等：
一个 784 MB 的 SQLite 日志库若 98% 是空闲页，报的是 774 MB，因为里面的活数据还在。
表格的 `CLEANABLE` 列给占用量，摘要行给真实回收量。

**软件本体永不清理。** 扩展、插件、随附二进制、`node_modules` 归为 `install` 类：
`duster status` 里算体积，`duster clean` 里看不见。判据只有一条——
删了要重新安装的，duster 就不删，没有级别、没有开关、没有例外。

全局开关：`--dry-run`（破坏性命令默认开）· `--yes` · `--json` · `--agent <id>` · `--quiet` · `--no-color`

## 原则

1. **只做管理，不做运行** — 不代理请求、不托管模型、不常驻后台。
2. **零遥测、零账号、零云端** — 默认完全离线。
3. **默认只读** — 破坏性操作必须显式开启，删除永远可恢复。
4. **不理解的东西不动** — 只操作适配器声明过所有权的路径。
5. **数据是你的** — 索引库随时可删，重扫即可重建。

## 支持的 Agent

Claude Code · Codex · oh-my-pi (omp) · Gemini CLI · Cursor · GitHub Copilot CLI · Kimi CLI · OpenCode · Qoder · 通用 MCP 客户端（VS Code / Windsurf / Cline，*规划中*）

新增一个 agent 通常只需要**一个声明式 TOML 清单——不写代码、不重新编译**。

## 许可证

[MIT](LICENSE)
