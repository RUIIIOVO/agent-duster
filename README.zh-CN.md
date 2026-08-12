<div align="center">

# Agent Duster

**AI Agent 的资源管理器。**

统一检索、共享、清理、迁移散落在 Claude Code、Codex、Gemini CLI、omp
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

- 🔍 **统一检索** — 一条命令跨所有 agent 搜会话正文，`open` 读出完整那一轮，中文可搜。
- 🧬 **重复与漂移检测** — `skill copies` 找出逐字节相同的副本和同名但内容已分叉的 skill（同一个 agent 里的两份也算）；`skill link` 把它们收敛为一份链接引用的唯一来源。
- 🧹 **分档清理** — `clean` 收可再生垃圾、`prune` 收陈旧资源、`uninstall` 整体卸载。三个动词按"删错了要付什么代价"划分，每一项都解释清楚。
- 🔁 **一处声明，多家生效** — `mcp sync` 把一个 MCP server 复制进别的 agent 并改写成各家的格式；`skill link` 让多家共用磁盘上的同一份。转换会丢字段的目标一律拒写，绝不静默截断。
- 🩺 **体检** — 一趟六项：明文凭据（只显示掩码，绝不完整打印）、MCP 可达性、skill 元数据、配置语法、失效引用、SQLite 完整性。
- 🔒 **默认安全** — 默认 dry-run；删除前一定逐条列清单说明；**删除即永久**，不可再生的内容会先压缩归档到你看得见的地方。完全离线、零遥测、零账号。

## 快速开始

> ⚠️ Alpha。下表里的命令今天都能跑。还缺的部分列在
> [还没做的](#还没做的)。

```bash
duster                          # 终端里不带参数：从菜单里挑
duster scan                     # 发现 agent 并建立索引
duster status                   # 总览：agent / 体积 / 问题数
duster search "鉴权中间件"
duster mcp list                 # 每个 MCP server 一行，合并所有声明它的 agent
duster session list             # 按项目、时间、体积浏览会话
duster clean                    # 清缓存与日志，默认 dry-run
duster prune --older-than 90d   # 清陈旧 skill / 会话，逐条确认
```

## 命令一览

| 命令 | 作用 |
|---|---|
| `duster` | 在终端里不带参数：进菜单。挑一条命令，缺什么它当场问，然后走的是和旗标完全相同的那条代码路径。管道里或带 `--json`：改为打印帮助。 |
| `duster scan` | 探测已安装 agent，索引 MCP / skills / 记忆 / 会话。默认增量；不认识的路径只上报、不触碰。 |
| `duster status` | 每个 agent 的磁盘占用、资源数量、上次扫描时间。 |
| `duster search <query>` | 跨 agent 全文检索会话正文，`--agent` 缩范围（逗号分隔可给多个）、`--limit` 定条数。摘要给的是抽取后的对话正文，不是原始那行 JSONL。`duster open <id>` 打印命中所在的那一整轮；喂给它一个会话 id 也认，会说明一句再打印整场对话。 |
| `duster session list \| show \| export \| prune` | 跨 agent 浏览会话，最近用过的在前；支持 `--agent` / `--project` / `--older-than` / `--min-bytes` 过滤。`show` 把一整场对话印成散文：工具输出各折叠成一行（`## tool · read · 4.3 KB`），过长正文停在 40 行——你来这儿是读对话，不是给 `read()` 的返回值做回放；要全文加 `--full`。`export` 导出 Markdown 或 JSON，永远是全量。`prune --older-than 90d` 原地压成 `.zst`，解压后 BLAKE3 与原文件逐字节一致才删原件——之后 search 和 open 照样读得出来。 |
| `duster memory list \| show` | 只读的统一记忆视图：`CLAUDE.md` / `AGENTS.md` / `GEMINI.md`、Qoder 的分项目记忆树、SQLite 里的记忆库一视同仁。`list` 给 key，`show` 打印那一条。 |
| `duster mcp list \| show \| diff \| sync \| ping` | 一个 server 一行，合并所有声明它的 agent，并标出各家的方言。`show` 打印全文，env 值与 header 一律掩码。`diff --from A --to B` 比对同一个 server 在两家的声明。`sync <name> --to codex,opencode` 把一条声明复制进别的 agent 并转换格式；默认只出计划，写之前对每个文件做整文件快照，目标格式装不下的字段一律**拒写**。`ping` 把每个 server 起一次，说一句 `initialize` 就挂断。 |
| `duster skill copies \| link` | 跨 agent skill 管理。`copies` 列出存在于多处的 skill——同一个 agent 里的两份也算——以及哪些副本已经漂移；`link` 把重复项收敛到磁盘上的同一份——一处修改，全局生效。 |
| `duster diff <a> <b>` | 逐行比较两个文件或两个目录。`--no-line-level` 只说哪些条目不同，`--include-same` 把相同的也列出来。 |
| `duster doctor` | 一趟六项检查：`secrets`（需要 `--secrets`）、`skill-metadata`、`config-syntax`、`dangling-reference`、`sqlite-integrity`、`mcp-reachability`（需要 `--ping`）。`--check <name>` 只跑点名的那几项。每一项在报告里都占一行——没跑的那几项会说自己没跑，以及要加哪个旗标才跑。 |
| `duster clean` | 清**可再生垃圾**：SQLite 空闲页、孤儿 WAL/SHM、缓存、日志、临时残留。默认 dry-run，`--yes` 执行，**真删不留副本**。执行完会报出另外两桶（陈旧资源 / 软件本体）的体量与去处。**永不卸载已安装的软件**。 |
| `duster prune --older-than 30d` | 清**陈旧的用户资源**：N 天没改过、也没有任何**调用记录**的 skill（名字在正文里被提到一句不算证据，见下），过期的备份与归档；N 天以上的会话改为压缩存档（不删内容）。加 `--keep-generations` 还会清掉数据库备份这类「一代一份」资源的超编副本——它们冗余是因为份数多，不是因为旧。一定先逐条列出「是什么 / 为何判定陈旧 / 删了什么后果」，确认后永久删除；不可再生的内容会先打包到 `~/agent-duster-exports/`。 |
| `duster uninstall <agent>` | **整体卸载一个 agent**，分三块：清单声明为它独占的目录与文件；它写在**别人家**配置文件里的键（外科式摘除，改之前先做整文件快照）；以及软件本体当初是怎么装的——那条卸载命令只打印、**绝不代跑**，除非你自己加 `--run-package-manager`。`--data-only` 是这后两块的退出开关，并会报出跳过了几条改键与几条安装提示。要求逐字输入 agent id 确认；`--export-first`（默认开）先导出会话与记忆，`--keep sessions,memory` 保留原地。 |

### 还没做的

- `duster migrate --from <A> --to <B>` — 两个 agent 之间整体搬家，一次出计划、幂等执行。
- `duster mcp remove` 与 `duster skill unlink \| remove` — 现在删一条声明还得自己改文件，或者整体 `uninstall` 那个 agent。
- `duster memory merge \| export` — 今天的记忆视图是只读的。
- 清理陈旧的 MCP 声明。`prune` 管的是文件；从别人还在用的配置文件里摘掉一条 server，走的是和 `mcp sync` 同一套机器，与它同批落地。
- 对记忆、skill、MCP 配置的检索。今天 `search` 只搜会话正文。

### 三个动词，按"删错了要付什么代价"分

| 命令 | 清什么 | 谁产生的 | 删错的代价 | 怎么删 |
|---|---|---|---|---|
| `clean` | SQLite 空闲页（VACUUM）、孤儿 WAL/SHM、缓存、日志、临时残留 | 程序自己 | 零，自动重建 | 真删，不留副本 |
| `prune` | N 天没改过、**且没有任何调用记录**的 skill，超编的备份代际（需 `--keep-generations`）；N 天以上的会话压缩存档 | **你配置或写的** | 要重新配置，可能找不回来 | 逐条列出 → 确认 → 先归档 → 永久删除 |
| `uninstall` | 某个 agent 的全部数据与配置 | 安装器 | 重装 + 重配 + 历史全没 | 逐字输入 agent id → 先导出 → 永久删除 |

它们是三个命令而不是一个命令的三个档位，因为**同意模型不同**：clean 无需逐项同意，
prune 必须逐项过目。肌肉记忆是按命令建立的，不是按旗标——`duster clean --yes` 敲过
五十次之后就不会再读输出了，这时候把"要读的清单"塞进同一个动词里，那道确认挡不住什么。

`clean` 内部分 `l0`（无损：VACUUM / 孤儿 WAL）与 `l1`（可再生：缓存 / 日志），两档默认都开。
`--older-than` 只属于 `prune`，接受 `30d` / `60d` / `90d` 或任意 `<N>d`。

菜单里的 `clean` 与 `prune` 都把要动的东西做成**一张对齐的勾选表**（agent / 类别 /
能回收多少 / 闲置多少天 / 路径），不再甩全量报告：空格切换、Enter 执行勾选的、默认全勾，
取消掉的那几条整条剔除、不参与本次执行。全量的「是什么 / 为何可清 / 清后影响」属于
命令行路径（`--dry-run`），那份报告照旧可以重定向成文件逐条核对。

菜单里其余的列表视图一律是**浏览器**而不是转储：`session list`、`memory list`、
`mcp list`、`skill copies`、`search` 都按终端高度翻页（↑/↓ 移动、←/→ 翻页、
Enter 打开、Esc 退回），Enter 直接对选中那一行跑详情命令——不用再手抄一个 key
去敲第二条命令。凡是能按 `--agent` 缩范围的地方，问的都是一张 agent 勾选表，
每行带该 agent 的总量与可回收量，**默认全勾**——把不想动的那几个取消掉就行。
空格切当前行，`a` 切全表，第一行 `All agents` 勾上时其余行会真的跟着勾上；
勾满提交与不给这个旗标同义。每张勾选表的光标都用 `❯` 单独占一列（与 `[x]`
勾选框分开），复制粘贴出去、或者终端不认颜色时，「我在哪一行」也不会丢。

菜单里每一张列表——连命令菜单自己——都是同一个控件：画的行数永远不超过终端
高度，比一屏高时末行报「我在哪一段」（`2-20 of 20`）。←/→ 在每一张表里都真的
翻页；一屏装得下的表不会提这两个键——提了就是撒谎。

`duster status` 报的是**能拿回多少**，不是**占了多少**——这两个数在 L0 上不相等：
一个 784 MB 的 SQLite 日志库若 98% 是空闲页，报的是 774 MB，因为里面的活数据还在。
表格的 `CLEANABLE` 列给占用量，摘要行给真实回收量。

**软件本体永不清理。** 扩展、插件、随附二进制、`node_modules` 归为 `install` 类：
`duster status` 里算体积，`clean` 和 `prune` 里看不见。判据只有一条——删了要重新安装的，
这两个命令就不删，没有级别、没有开关、没有例外。真要整个搬走，只有一条路：
`duster uninstall <agent>`，一个单独命名、必须指名道姓的动词。

**「用过」指被调用过，不是被提到过。** 一份 skill 只有在**没有任何调用记录**时才算陈旧——
调用记录指会话里真实存在的那一笔：Claude Code 的 `Skill` 工具调用、参数里的
`skill://<名字>`、对该 skill 的 `SKILL.md` 的读取。名字出现在正文里**明确不算**证据：
skill 名都是 `pdf`、`docx`、`frontend` 这种普通词，而且每家 agent 都会在每场会话的
指令里把自己的整份 skill 清单列一遍——把这个也算上，等于所有 skill 永远都"在用"。
在 `duster scan` 采集到调用证据之前，一份 skill 都不会被判陈旧：「还没看过」与
「看过了没有」是两件不同的事，duster 不会把前者印成后者。

**备份按份数判，不按年龄判。** 资源可以在清单里声明 `keep_generations = 2`：最新两份留下，
其余是超编。同一个库的第 8 份副本，不管它是 16 天还是 16 个月都是冗余的，所以
`--older-than` 永远碰不到它——正因如此，超编代际需要显式的 `--keep-generations`，在计划里
单独成组，并在自己的理由里写明「是按份数挑中的」。不加旗标时，prune 会报出有几份超编、
一共多少字节，然后一个字节都不动。

### 关于回收站：没有

duster 不做回收站。一个清理工具跑完 `df` 没变化，是它能犯的最严重的错误；
而回收站自己还会变成下一个需要被清理的东西。取而代之的是四条更硬的保证：

- **删之前一定让你看清楚。** 每一条都带「是什么 / 为何判定可删 / 删了什么后果 /
  归档包里有没有」，不是甩一串路径。`--yes` 只跳过问句，**不跳过清单**。
- **不可再生的内容，删之前先打包。** 落到 `~/agent-duster-exports/<操作>-<日期>.tar.zst`，
  路径和尺寸印在输出里，`tar -xf` 直接解开。它归你所有，duster 不会替你清它。
  压缩率很高——全部 77 个 skill 的用户内容打包后只有 7.9 MB。
- **会话不是"删"是"压"。** 解压后逐字节校验与原文件一致，才会删原件。
  可验证的正确性比可撤销更强。
- **改配置文件不走这一套。** 从 `~/.claude.json` 里摘掉一条 MCP 不是删文件，是改一个
  还装着二十条别的东西的文件——这种操作前会做整文件快照。

`--json` 是全局的，任何命令都能改成打印一行 JSON。其余旗标属于有它的那条命令：
`--yes` 在 `clean` / `prune` / `session prune` / `mcp sync` 上，`--dry-run` 在前三条上
（`mcp sync` 没有这个旗标，因为不给 `--yes` 时它做的就是只出计划），`--confirm <agent>`
在 `uninstall` 上，`--full` 在 `session show` 与 `open` 上，`--agent <id>[,<id>]`
在一切需要缩范围的地方。`--json` 模式下破坏性动作
一律不会在缺 `--yes` 时执行：那里没有终端可问，duster 打完计划就以 4 退出。

## 原则

1. **只做管理，不做运行** — 不代理请求、不托管模型、不常驻后台。
2. **零遥测、零账号、零云端** — 默认完全离线。
3. **默认只读** — 破坏性操作必须显式开启；删除前一定逐条列清单，不可再生的先归档。
4. **不理解的东西不动** — 只操作适配器声明过所有权的路径。
5. **数据是你的** — 索引库随时可删，重扫即可重建。

## 支持的 Agent

Claude Code · Codex · oh-my-pi (omp) · Gemini CLI · Cursor · GitHub Copilot CLI · Kimi CLI · OpenCode · Qoder · 通用 MCP 客户端（VS Code / Windsurf / Cline，*规划中*）

新增一个 agent 通常只需要**一个声明式 TOML 清单——不写代码、不重新编译**。

## 许可证

[MIT](LICENSE)
