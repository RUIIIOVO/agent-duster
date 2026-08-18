//! `duster mcp` 外壳：把 [`duster_core::mcp`] 的四个用例渲染成人话或一行 JSON。
//!
//! 本模块零业务逻辑：解析参数 → 调 core → 按 `output.rs` 的契约排版 → 映射退出码。
//! 合并规则（同名 + 同内容哈希 = 一行）、冲突判定、闸门拒写、ping 的边界
//! 全部在 core 那一侧，这里只负责**把它说清楚**。
//!
//! # 为什么渲染函数返回 String 而不直接 println!
//!
//! `show` 有一条不能破的约束：env 与 headers 的值一律经
//! [`duster_core::doctor::mask`] 才准出现在屏幕上——一份 MCP 声明正是
//! API key 最常见的落脚处。这条约束要被测试守着，而「打印到 stdout」
//! 没法断言，所以渲染一律产出字符串，由命令外壳负责 println。
//!
//! # 掩码只管人类模式
//!
//! `--json` 输出的是 core 的结构体原样（[`McpList`] / [`MergedServer`] 能
//! 逐字段回环），env 值因此是原文。这是刻意的边界：`--json` 是用户
//! 显式索取的机器可读转录，掩码会让 `data` 变成一份假配置；
//! 而肩后偷看、终端回滚、截图这些真实泄露场景全发生在人类模式。
//!
//! # ping 为什么是 opt-in
//!
//! core 的 [`PingOptions::default`] 恒为 `enabled: false`——在别人的机器上
//! spawn 进程得让用户有权说不。本模块只在用户**敲了 `ping` 这个动词**时
//! 才把它置 true：那句命令本身就是那份同意，除此之外没有任何默认开启的路径。

use std::path::{Path, PathBuf};

use clap::Subcommand;
use console::style;

use duster_core::doctor::mask;
use duster_core::mcp::{
    self, Declaration, McpList, MergedServer, PingOptions, PingResult, RemoveReport, SyncOptions,
    SyncOutcome,
};
use duster_model::{McpServerSpec, McpTransport};

use crate::output::{
    EXIT_CONFIRM_DENIED, EXIT_OK, EXIT_PARTIAL, OutputMode, Prefix, Table, accent, display_width,
    emit_json, err_mark, human_bytes, human_ms, muted, ok_mark, truncate_width, warn_mark,
};
use crate::{fail, plural, render_warnings};

/// `mcp ping` 每个服务器的默认等待上限(毫秒),与命令行 `--timeout-ms` 同值。
pub(crate) const DEFAULT_PING_TIMEOUT_MS: u64 = 3_000;

/// `duster mcp` 的动词表。
#[derive(Subcommand, Clone)]
pub enum McpCmd {
    /// List every MCP server declaration, one row per agent that declares it
    List,
    /// Show one server in full, with credentials masked
    Show {
        /// Server name, as `duster mcp list` prints it
        name: String,
    },
    /// Copy one server's declaration into other agents
    Sync {
        /// Server name, as `duster mcp list` prints it
        name: String,
        /// Agent to copy from. Only needed when more than one declares it
        #[arg(long, value_name = "AGENT")]
        from: Option<String>,
        /// Agents to copy into, e.g. --to codex,opencode
        #[arg(long, value_delimiter = ',', required = true, value_name = "AGENT")]
        to: Vec<String>,
        /// Do it. Without this you only get the plan
        #[arg(long)]
        yes: bool,
    },
    /// Remove one server's declaration from an agent's own config file
    Rm {
        /// Server name, as `duster mcp list` prints it
        name: String,
        /// Agent whose declaration to remove. Needed when more than one
        /// agent declares it; pass --all-agents to remove every declaration
        #[arg(long, value_name = "AGENT", conflicts_with = "all_agents")]
        agent: Option<String>,
        /// Remove the declaration from every agent that has it
        #[arg(long)]
        all_agents: bool,
        /// Print what would change — which key disappears from which file —
        /// and stop without touching anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Start each server once, say hello, and hang up
    Ping {
        /// Only this server. Leave it out to try all of them
        name: Option<String>,
        /// Give each server this many milliseconds to answer
        #[arg(long, default_value_t = DEFAULT_PING_TIMEOUT_MS, value_name = "MS")]
        timeout_ms: u64,
    },
}

pub fn run(mode: OutputMode, index: Option<&Path>, action: McpCmd) -> i32 {
    match action {
        McpCmd::List => cmd_list(mode, index),
        McpCmd::Show { name } => cmd_show(mode, index, &name),
        McpCmd::Sync {
            name,
            from,
            to,
            yes,
        } => cmd_sync(mode, index, name, from, to, yes),
        McpCmd::Rm {
            name,
            agent,
            all_agents,
            dry_run,
        } => cmd_rm(mode, index, name, agent, all_agents, dry_run),
        McpCmd::Ping { name, timeout_ms } => cmd_ping(mode, index, name.as_deref(), timeout_ms),
    }
}

/// TARGET 列的截断宽度。目标本身可以很长（`npx -y @scope/pkg@1.2.3 --flag`），
/// 而这一列只用来认人；要看全的走 `duster mcp show`。
const COMMAND_WIDTH: usize = 44;

/// PATH 列的截断宽度。铺平表的 PATH 列按内容宽度截断（交互菜单那版
/// 走 flex_col，见 [`browse_rows_at`]）；命令行表格没有终端余量可吃，
/// 路径截到 44 足够认人，要看全的走 `duster mcp show`。
const PATH_WIDTH: usize = 44;

/// INFO 列的截断宽度。ping 失败时这一栏装的是一整句报错，
/// 截断的是句尾，句首（"哪一步失败了"）永远看得见。
const INFO_WIDTH: usize = 56;

/// 这一屏只在 TTY 下出现,取不到宽度是异常而不是常态。
fn terminal_cols() -> usize {
    console::Term::stderr()
        .size_checked()
        .map_or(100, |(_, cols)| cols as usize)
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn cmd_list(mode: OutputMode, index: Option<&Path>) -> i32 {
    let list = match mcp::list(index) {
        Ok(l) => l,
        Err(e) => return fail(mode, "mcp-list", &e),
    };
    match mode {
        // data 是 McpList 原样（warnings 也在里面）；信封的 warnings 再抄一遍，
        // 只读信封的脚本不必往 data 里挖。
        OutputMode::Json => emit_json("mcp-list", &list, &list.warnings),
        OutputMode::Human => {
            println!();
            println!("{}", render_list(&list));
            render_warnings(&list.warnings);
        }
    }
    // 有 warning 意味着某组声明没能回读，视图是不全的——按 output.rs 的口径
    // 那是部分成功，不是成功。
    if list.warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    }
}

/// 空注册表的一句话。空表格（只有表头和横线）看起来像出了故障。
const EMPTY_LIST: &str =
    "No MCP server is indexed. Run `duster scan` first — if you just added one, run it again.";

/// `list` 的人类视图：铺平表 + 冲突块 + 一行小结。
///
/// 铺平口径与 `skill list` 同构：**一行一条声明**，组名只印在首行
/// （视觉上把一组连成一块），STATE 列给这组声明的合并结论。想知道的
/// 「几家声明一样吗」直接读 STATE 列——这正是 `mcp diff` 曾回答的问题，
/// 现在不需要第二条命令。
fn render_list(list: &McpList) -> String {
    if list.servers.is_empty() {
        return format!("  {}", muted().apply_to(EMPTY_LIST));
    }

    let mut blocks: Vec<String> = Vec::new();

    let mut t = Table::new(vec![
        "SERVER", "STATE", "AGENT", "TARGET", "CONFIG TOUCHED", "PATH",
    ]);
    t.color_col(0, accent());
    // CONFIG TOUCHED 与 PATH 都是旁证列（「这份配置上次被改何时」/ 出处），
    // 置灰让 SERVER / STATE / TARGET 自己跳出来。
    t.color_col(4, muted());
    t.color_col(5, muted());
    for s in &list.servers {
        for (i, d) in s.declared_in.iter().enumerate() {
            t.push_row(vec![
                // 组名只在首行写——与 `render_skill_groups` 同一口径。
                if i == 0 { s.name.clone() } else { String::new() },
                crate::dup_state_label(d.state).to_string(),
                d.agent_id.clone(),
                truncate_width(&target_cell(&s.spec), COMMAND_WIDTH),
                crate::relative_time(d.last_used_ms),
                truncate_width(&duster_fs::path::display_tilde(&d.path), PATH_WIDTH),
            ]);
        }
    }
    blocks.push(t.render());

    for (name, hashes) in &list.conflicts {
        blocks.push(conflict_block(list, name, hashes));
    }

    let declarations: usize = list.servers.iter().map(|s| s.declared_in.len()).sum();
    // 两个数必须一起报：表里装的是**全部** server，只印「N servers」丢掉了
    // 「一共几条声明」——那是铺平表的第一眼答案；drifted 组在行上有 STATE
    // 标着，这里再点一句名，读者不用回头去数。
    //
    // drifted 按**名字**数（冲突表的口径）：同一个名字拆成两组声明时是
    // 一个走散的名字，不是两个。
    let mut tail = format!("from {declarations} declarations");
    if !list.conflicts.is_empty() {
        tail.push_str(&format!(
            " · {} declared differently in different agents",
            plural(list.conflicts.len(), "name")
        ));
    }
    blocks.push(format!(
        "  {} {} {}",
        muted().apply_to("Total"),
        style(plural(list.servers.len(), "server")).bold(),
        muted().apply_to(tail)
    ));

    blocks.join("\n\n")
}

/// 给交互菜单的逐条浏览:表头 + 对齐行,列与 [`render_list`] 的铺平表一致
/// (SERVER / STATE / AGENT / TARGET / CONFIG TOUCHED / PATH),一行一条声明,
/// 组名只印首行。行文本是纯文本,宽度按 [`display_width`] 算好,宽字符不
/// 顶歪;TARGET 照旧按 [`COMMAND_WIDTH`] 截——认人靠 SERVER 列,目标只看
/// 个大概。PATH 列吃余量([`Table::flex_col`]),窄终端里截断给出,重定向成
/// 文件时整条留下。返回的行直接喂 `browse` 原语。
pub(crate) fn browse_rows(list: &McpList) -> (String, Vec<String>) {
    browse_rows_at(list, terminal_cols())
}

/// 宽度算法本体;`cols` 由测试直接给(60 / 80 / 200),不必 mock 终端。
/// 前缀预算是 [`Prefix::Checkbox`]:统一列表流的浏览表每行画 `❯ [x] `。
pub(crate) fn browse_rows_at(list: &McpList, cols: usize) -> (String, Vec<String>) {
    let mut t = Table::new(vec![
        "SERVER", "STATE", "AGENT", "TARGET", "CONFIG TOUCHED", "PATH",
    ]);
    // PATH 吃余量:声明路径天然就长(~/Library/Application Support/…),
    // 不设总宽上限的话,没有人算过整行,TARGET 截到 44 也只是把问题
    // 挪到别处。
    t.flex_col(5);
    for s in &list.servers {
        for d in &s.declared_in {
            t.push_row(vec![
                // 名字**每行都印**,不是只印首行。交互表会翻页,一组的几条
                // 声明可能被切到下一页;而每一行都是可勾选的删除目标,
                // 空名字的行既认不出属于谁、又能被选中删掉。首行留白只在
                // 「整组恒在同屏」的打印表里成立(见 `render_list`)。
                s.name.clone(),
                crate::dup_state_label(d.state).to_string(),
                d.agent_id.clone(),
                truncate_width(&target_cell(&s.spec), COMMAND_WIDTH),
                crate::relative_time(d.last_used_ms),
                // PATH 折 `~`:声明住在 home 下,`/Users/<你>/` 那 13 列对
                // 用户零信息量,而这一列是最宽的那一列(与 `memory list` 同口径)。
                duster_fs::path::display_tilde(&d.path),
            ]);
        }
    }
    t.rows_at(Prefix::Checkbox, cols)
}

/// 一个冲突名字的说明块。
///
/// 冲突表只给 `name -> 各自的 content hash`，所以「谁站在哪一边」要回
/// [`McpList::servers`] 里按哈希对回去。对不上的那一组（声明没能回读，
/// 详情在 warnings 里）如实说明，不假装它不存在——它的哈希来自索引，
/// 与文件读不读得动无关。
fn conflict_block(list: &McpList, name: &str, hashes: &[String]) -> String {
    let mut s = format!(
        "  {} {}\n",
        style("conflict").yellow().bold(),
        accent().apply_to(name)
    );
    s.push_str(&format!(
        "    {}\n",
        muted().apply_to(format!(
            "`{name}` does not mean the same thing in every agent: {} different declarations share this one name.",
            hashes.len()
        ))
    ));
    for h in hashes {
        let who = group_of(list, name, h)
            .map(|g| agents_with_dialect(&g.declared_in))
            .unwrap_or_else(|| "(this declaration could not be re-read)".to_string());
        s.push_str(&format!("    {}  {who}\n", muted().apply_to(short_hash(h))));
    }
    // 不再给 diff 命令:差异本身就在上面的铺平表里(STATE=drifted 的那几行),
    // 各家的命令 / url / 路径并排一眼可见,再指一条命令是画蛇添足。
    s
}

fn group_of<'a>(list: &'a McpList, name: &str, hash: &str) -> Option<&'a MergedServer> {
    list.servers
        .iter()
        .find(|s| s.name == name && s.content_hash == hash)
}

/// 哈希的展示形态：前 12 位够区分，整串 64 位会把行挤爆。
fn short_hash(hash: &str) -> String {
    let head: String = hash.chars().take(12).collect();
    if head.len() < hash.len() {
        format!("{head}…")
    } else {
        head
    }
}

/// AGENTS 列：`codex (mcp/codex-toml), claude-code (mcp/standard-json)`。
///
/// 方言必须跟着 agent 一起出现——同一份语义在三个文件里是三种写法，
/// 而 `sync` 的读者要据此判断「往那边写意味着什么格式」。
fn agents_with_dialect(decls: &[Declaration]) -> String {
    decls
        .iter()
        .map(|d| format!("{} ({})", d.agent_id, d.dialect))
        .collect::<Vec<_>>()
        .join(", ")
}

/// 只要 agent 名的场合（ping 的 AGENT 列）。
fn agent_ids(decls: &[Declaration]) -> String {
    decls
        .iter()
        .map(|d| d.agent_id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn transport_name(t: &McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio => "stdio",
        McpTransport::Http => "http",
        McpTransport::Sse => "sse",
    }
}

/// TARGET 单元格的命令 / URL 部分：stdio 给命令行，远程给端点。
/// 两者都没有就说明声明是残的。
fn command_cell(spec: &McpServerSpec) -> String {
    match spec.transport {
        McpTransport::Stdio => match spec.command.as_deref() {
            Some(c) if !c.is_empty() => {
                if spec.args.is_empty() {
                    c.to_string()
                } else {
                    format!("{c} {}", spec.args.join(" "))
                }
            }
            _ => "(no command)".to_string(),
        },
        _ => spec.url.clone().unwrap_or_else(|| "(no url)".to_string()),
    }
}

/// TARGET 列：`stdio npx -y foo` / `http https://…` / `sse https://…`。
///
/// 传输类型并进单元格——http 与 sse 都渲染成 URL，光看 URL 分不出两者，
/// 前缀 `stdio` / `http` / `sse` 让传输类型与目标一眼可见，省下一整列
/// （TRANSPORT 与 COMMAND / URL 两列并成一列，两张表都保持同构）。
fn target_cell(spec: &McpServerSpec) -> String {
    format!("{} {}", transport_name(&spec.transport), command_cell(spec))
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

fn cmd_show(mode: OutputMode, index: Option<&Path>, name: &str) -> i32 {
    let server = match mcp::show(index, name) {
        Ok(s) => s,
        Err(e) => return fail(mode, "mcp-show", &e),
    };
    match mode {
        OutputMode::Json => emit_json("mcp-show", &server, &[]),
        OutputMode::Human => {
            println!();
            println!("{}", render_show(&server));
        }
    }
    EXIT_OK
}

/// 字段名列的最小宽度。`env <KEY>` 这类标签可以任意长，所以真实列宽由
/// 本屏最长的那个标签决定（见 [`render_fields`]），这个常数只保证短标签
/// 也有一列呼吸感。
const LABEL_WIDTH: usize = 14;

/// `extra` 的处置：**不打印内容**。
///
/// 它装的是本方言里我们没建模的字段，里头完全可能有凭据（opencode 的
/// `headers` 变体、自定义的 token 键），而我们既不知道哪个键是秘密、
/// 也没有理由在详情页上赌一次。
const EXTRA_NOTE: &str = "present (dialect-specific fields duster does not model; not printed — they may hold credentials)";

/// `show` 的人类视图：字段清单。
///
/// 只讲「这一条声明长什么样」：transport / command / args / url / env /
/// headers 与掩码。DECLARED IN 小表已随铺平表一起撤掉——出处（agent /
/// 方言 / 路径）现在一行一行摆在 `duster mcp list` 里，这里再印一遍
/// 只会让两个入口各说一半的谎。
fn render_show(s: &MergedServer) -> String {
    // 先攒成 (标签, 值)，再按本屏最长标签对齐：`env STITCH_TOKEN` 与
    // `transport` 差一倍长度，写死列宽必然有一屏是歪的。
    let mut rows: Vec<(String, String)> = vec![(
        "transport".to_string(),
        transport_name(&s.spec.transport).to_string(),
    )];
    if let Some(c) = &s.spec.command {
        rows.push(("command".to_string(), c.clone()));
    }
    if !s.spec.args.is_empty() {
        rows.push(("args".to_string(), s.spec.args.join(" ")));
    }
    if let Some(u) = &s.spec.url {
        rows.push(("url".to_string(), u.clone()));
    }
    // headers 与 env 的值一律掩码。这是这一屏唯一的硬约束。
    for (k, v) in &s.spec.headers {
        rows.push((format!("header {k}"), mask(v)));
    }
    for (k, v) in &s.spec.env {
        rows.push((format!("env {k}"), mask(v)));
    }
    if s.spec.extra.is_some() {
        rows.push(("extra".to_string(), EXTRA_NOTE.to_string()));
    }
    rows.push(("content".to_string(), short_hash(&s.content_hash)));

    let mut fields = format!("  {}\n", accent().bold().apply_to(&s.name));
    fields.push_str(&render_fields(&rows));
    if !s.spec.env.is_empty() || !s.spec.headers.is_empty() {
        fields.push_str(&format!(
            "\n  {}",
            muted().apply_to(
                "Values of env and headers are masked — duster never prints them in full."
            )
        ));
    }
    fields
}

/// 字段清单：标签置灰、按最长标签左对齐，值原样。
///
/// 补白按**纯文本**宽度算再着色，与 [`Table`] 同一个次序——反过来会剪断
/// ANSI 序列，或者让宽度算进转义字节。
fn render_fields(rows: &[(String, String)]) -> String {
    let width = rows
        .iter()
        .map(|(k, _)| display_width(k))
        .max()
        .unwrap_or(0)
        .max(LABEL_WIDTH);
    let mut out = String::new();
    for (k, v) in rows {
        let pad = width.saturating_sub(display_width(k));
        out.push_str(&format!(
            "    {}{}  {v}\n",
            muted().apply_to(k),
            " ".repeat(pad)
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// sync
// ---------------------------------------------------------------------------

fn cmd_sync(
    mode: OutputMode,
    index: Option<&Path>,
    name: String,
    from: Option<String>,
    to: Vec<String>,
    yes: bool,
) -> i32 {
    // 与 clean / prune 同一套口径:预览是默认,执行是显式行为。
    let dry_run = !yes;
    let opts = SyncOptions {
        index_path: index.map(Path::to_path_buf),
        home: None,
        name,
        from,
        to,
        dry_run,
        yes,
    };

    // 两段式:plan_sync 只出计划(一个字节都不写),apply_sync 才动手。
    let plan = match mcp::plan_sync(&opts) {
        Ok(p) => p,
        Err(e) => return fail(mode, "mcp-sync", &e),
    };
    // 没 `--yes` 就停在计划:渲染与退出码(4)与两段式之前一字不差。
    if !yes {
        match mode {
            OutputMode::Json => emit_json("mcp-sync", &plan.targets, &[]),
            OutputMode::Human => {
                println!();
                println!("{}", render_sync(&plan.targets, true));
            }
        }
        return sync_exit(true, &plan.targets);
    }

    // 同意过了:整份计划照写(allow = None = 全部目标)。
    let outcomes = match mcp::apply_sync(&opts, &plan, None) {
        Ok(o) => o,
        Err(e) => return fail(mode, "mcp-sync", &e),
    };
    match mode {
        OutputMode::Json => emit_json("mcp-sync", &outcomes, &[]),
        OutputMode::Human => {
            println!();
            println!("{}", render_sync(&outcomes, false));
        }
    }
    sync_exit(false, &outcomes)
}

/// 分发结果 → 退出码。
///
/// 预览优先：dry-run 一个字节都没写，那是「什么都没做」（4），不是
/// 「做了但有失败」。执行过后任何一条目标**带着 error** 回来（闸门拒写、
/// 目标已有手写声明因此不覆盖）都算部分成功（3）——用户要求的是
/// 「把这条声明放进这几个 agent」，少一个就不是全做完。
///
/// `skipped`（用户没勾的目标）不算失败：那是用户的主动选择，不是分发
/// 失败。它的占位按契约不带 error，自然落不进上面的判定。
pub(crate) fn sync_exit(dry_run: bool, outcomes: &[SyncOutcome]) -> i32 {
    if dry_run {
        return EXIT_CONFIRM_DENIED;
    }
    if outcomes.iter().any(|o| o.error.is_some()) {
        EXIT_PARTIAL
    } else {
        EXIT_OK
    }
}

/// `sync` 的人类视图：一条目标一块。
///
/// 不做成表格是刻意的：拒写的原因是一整句话（还带着两个指纹），
/// 挤进单元格必然被截断，而被截掉的恰好是用户唯一需要读的那一句。
///
/// 菜单的目标勾选表之前也用它打全计划：同一份计划在命令行与菜单里
/// 必须印成同一个形状。
pub(crate) fn render_sync(outcomes: &[SyncOutcome], dry_run: bool) -> String {
    let mut blocks: Vec<String> = Vec::new();
    blocks.push(format!(
        "  {}",
        muted().apply_to(if dry_run {
            "Plan only — not one byte has been written."
        } else {
            // 「同意过了」不等于「每个目标都写了」——每一行自己说结果，
            // 表头不许替它们下结论。
            "Confirmed with --yes. Each target below says what actually happened."
        })
    ));

    for o in outcomes {
        let where_ = if o.path.is_empty() {
            "(no config file)".to_string()
        } else {
            o.path.clone()
        };
        let mut b = match o.action.as_str() {
            // 拒写是这条命令最重要的一句话：红色 + 标记 + 大写，
            // 后面跟着原因和两个指纹，逐字照搬 core 的措辞。
            "refused" => format!(
                "  {} {}  {}  {}",
                err_mark(),
                style("REFUSED").red().bold(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&where_)
            ),
            "skip" => format!(
                "  {} {}  {}  {}",
                warn_mark(),
                style("skip").yellow(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&where_)
            ),
            // 用户自己没勾的目标：中性标记（不是绿色成功、不是红色失败），
            // 文案必须说清是选择的结果，不是分发这一侧的事。
            "skipped" => format!(
                "  {} {}  {}  {}",
                muted().apply_to("-"),
                style("skipped").dim(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&where_)
            ),
            other if dry_run => format!(
                "  {} {}  {}  {}",
                muted().apply_to("+"),
                style(other).green(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&where_)
            ),
            other => format!(
                "  {} {}  {}  {}",
                ok_mark(),
                style(other).green(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&where_)
            ),
        };
        if let Some(e) = &o.error {
            b.push_str(&format!("\n      {e}"));
        }
        if o.action == "refused" {
            b.push_str(&format!(
                "\n      {}",
                muted().apply_to("nothing was written to this file")
            ));
        }
        // skipped 的占位不带 error，所以那句「原因」不会出现；补一句
        // 明确的「用户没选它」——否则读起来像 duster 忘了这个目标。
        if o.action == "skipped" && !dry_run {
            b.push_str(&format!(
                "\n      {}",
                muted().apply_to("not selected — nothing was written here")
            ));
        }
        if o.action != "refused" && dry_run {
            b.push_str(&format!(
                "\n      {}",
                muted().apply_to("nothing written yet")
            ));
        }
        // 快照路径是唯一的还原来源,必须原样出现在输出里。
        if let Some(s) = &o.snapshot {
            b.push_str(&format!(
                "\n      {} {s}",
                muted().apply_to("snapshot before the change:")
            ));
        }
        blocks.push(b);
    }

    if outcomes.is_empty() {
        blocks.push(format!(
            "  {}",
            muted().apply_to("No target left to write to.")
        ));
    }
    if dry_run {
        blocks.push(format!(
            "  {} {}",
            warn_mark(),
            muted().apply_to(
                "Nothing was written. Re-run the same command with --yes to apply this plan."
            )
        ));
    }
    blocks.join("\n\n")
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// `duster mcp rm`：从指定 agent 的**自己的主配置文件**里摘掉一条声明。
///
/// 这是全项目最危险的写入——改的是 `~/.claude.json`、`~/.codex/config.toml`、
/// `~/.gemini/settings.json` 这种别人家的主配置，同文件里全是用户其他设置。
/// 所以这里走 `duster_core::mcp::remove`，它的每一道工序（schema_guard 降只读、
/// 整文件快照、只删那一个键）在 core 那一侧，外壳只负责把话说清。快照是这次
/// 改写唯一的退路，落点 `~/.agent-duster/snapshots/<op-id>/` 由渲染印出来。
///
/// 目标即同意：`--agent a` / `--all-agents` 就是那份授权，与 `uninstall` 的
/// `--confirm` 同一个思路——删哪一家必须由用户说死，`--dry-run` 是预览。
fn cmd_rm(
    mode: OutputMode,
    index: Option<&Path>,
    name: String,
    agent: Option<String>,
    all_agents: bool,
    dry_run: bool,
) -> i32 {
    let report = match mcp::remove(&mcp::RemoveOptions {
        index_path: index.map(Path::to_path_buf),
        home: None,
        dry_run,
        name,
        agent,
        all_agents,
    }) {
        Ok(r) => r,
        Err(e) => return fail(mode, "mcp-rm", &e),
    };
    match mode {
        // `--json` 出四组共用的 DeleteReport（Contract 1）：机器可读的删除
        // 报告只有一种形状，脚本不必为每个名词学一套。mcp rm 真删不打包，
        // `archived` 恒为 None；快照落点看人话输出。
        OutputMode::Json => {
            let rep = duster_core::delete::DeleteReport {
                archived: None,
                removed: report
                    .outcomes
                    .iter()
                    .filter(|o| o.action == "removed")
                    .map(|o| PathBuf::from(&o.path))
                    .collect(),
                freed_bytes: report.freed_bytes,
                warnings: report.warnings.clone(),
            };
            emit_json("mcp-rm", &rep, &rep.warnings)
        }
        OutputMode::Human => {
            println!();
            println!("{}", render_rm(&report, dry_run));
            render_warnings(&report.warnings);
        }
    }
    rm_exit(&report, dry_run)
}

/// `rm` 的退出码。
///
/// 预览优先：dry-run 一个字节都没写，那是「什么都没做」（4）。执行过后任何
/// 一条目标被拒（guard 降只读、清单读不动、文件不存在）都算部分成功（3）——
/// 用户要求的是「这条声明没了」，还有一条挂着就不算全做完。
/// `absent`（文件里本来就没有这条声明）不算失败：那是事实陈述，不是 duster
/// 没干成。
fn rm_exit(report: &RemoveReport, dry_run: bool) -> i32 {
    if dry_run {
        return EXIT_CONFIRM_DENIED;
    }
    if report.outcomes.iter().any(|o| o.action == "refused") {
        EXIT_PARTIAL
    } else {
        EXIT_OK
    }
}

/// `rm` 的人类视图：一条目标一块。
///
/// 每块必须说清**哪个文件的哪个键**——摘键这种事，用户要能逐条核对
/// 落点，而不是相信一句「已删除」。拒写的原因是一整句话（还带着两个
/// 指纹），挤进表格必然被截断，所以照 `render_sync` 的块式排版。
pub(crate) fn render_rm(report: &RemoveReport, dry_run: bool) -> String {
    let mut blocks: Vec<String> = Vec::new();
    blocks.push(format!(
        "  {}",
        muted().apply_to(if dry_run {
            "Plan only — not one byte has been written."
        } else {
            "Removed. Each line below says what actually happened."
        })
    ));

    for o in &report.outcomes {
        let mut b = match o.action.as_str() {
            // 拒写是这条命令最重要的一句话：红色 + 标记 + 大写。
            "refused" => format!(
                "  {} {}  {}  {}",
                err_mark(),
                style("REFUSED").red().bold(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&o.path)
            ),
            "absent" => format!(
                "  {} {}  {}  {}",
                warn_mark(),
                style("absent").yellow(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&o.path)
            ),
            _ => format!(
                "  {} {}  {}  {}",
                ok_mark(),
                style("removed").green(),
                accent().apply_to(&o.agent_id),
                muted().apply_to(&o.path)
            ),
        };
        if !o.key.is_empty() {
            b.push_str(&format!(
                "\n      {} {}",
                muted().apply_to("key removed:"),
                o.key
            ));
        }
        if let Some(e) = &o.error {
            b.push_str(&format!("\n      {e}"));
        }
        match o.action.as_str() {
            "refused" => b.push_str(&format!(
                "\n      {}",
                muted().apply_to("nothing was written to this file")
            )),
            "absent" => b.push_str(&format!(
                "\n      {}",
                muted().apply_to("the file already lacks this declaration; its index entry is dropped")
            )),
            _ if dry_run => b.push_str(&format!(
                "\n      {}",
                muted().apply_to("nothing written yet")
            )),
            _ => {}
        }
        blocks.push(b);
    }

    // 收尾：快照落点与释放字节。快照是摘键后的唯一退路——改坏了从哪捞——
    // 必须原样出现；全 refused / absent（没写盘）时没有快照可指，就不编。
    let mut footer: Vec<String> = Vec::new();
    match (&report.snapshots, dry_run) {
        (Some(p), false) => footer.push(format!(
            "  {} {}",
            muted().apply_to("snapshot of the original files:"),
            p.display()
        )),
        (_, true) => footer.push(format!(
            "  {}",
            muted().apply_to(
                "A real run would snapshot the whole config file under \
                 ~/.agent-duster/snapshots/ first."
            )
        )),
        (None, false) => {}
    }
    footer.push(format!(
        "  {} {} {}",
        muted().apply_to("Freed"),
        style(human_bytes(report.freed_bytes)).bold(),
        muted().apply_to(if dry_run {
            "estimated"
        } else {
            "on disk"
        })
    ));
    blocks.push(footer.join("\n"));
    blocks.join("\n\n")
}

// ---------------------------------------------------------------------------
// ping
// ---------------------------------------------------------------------------

fn cmd_ping(mode: OutputMode, index: Option<&Path>, name: Option<&str>, timeout_ms: u64) -> i32 {
    // 指名一个就走 show：它的报错已经把「同名走散了」「根本没这个名字」
    // 分得很清楚，在这里重写一遍只会得到两套措辞。
    let servers = match name {
        Some(n) => match mcp::show(index, n) {
            Ok(s) => vec![s],
            Err(e) => return fail(mode, "mcp-ping", &e),
        },
        None => match mcp::list(index) {
            Ok(l) => l.servers,
            Err(e) => return fail(mode, "mcp-ping", &e),
        },
    };

    // 唯一把 ping 打开的地方：用户敲了 `ping` 这个动词，那句命令就是那份同意。
    let opts = PingOptions {
        enabled: true,
        timeout_ms,
    };
    // 一个 merged server 只 ping 一次。三个 agent 共用同一份规格就是同一条
    // 启动命令，ping 三遍等于把同一个进程起三次，答案还是同一个。
    let results: Vec<PingResult> = servers
        .iter()
        .map(|s| mcp::ping(&s.spec, &agent_ids(&s.declared_in), &opts))
        .collect();

    match mode {
        OutputMode::Json => emit_json("mcp-ping", &results, &[]),
        OutputMode::Human => {
            println!();
            println!("{}", render_ping(&servers, &results));
        }
    }

    // 只有「本该起得来却没起来」算部分失败。非 stdio 的跳过不是故障——
    // 它压根不在 ping 的射程里，为它落一个非零退出码会让脚本永远报警。
    let broken = servers
        .iter()
        .zip(&results)
        .any(|(s, r)| s.spec.transport == McpTransport::Stdio && !r.ok);
    if broken { EXIT_PARTIAL } else { EXIT_OK }
}

/// `ping` 的人类视图。INFO 一栏永远有话说：成功给 server 自报的名字与版本，
/// 失败或跳过给原因，绝不留空——空白单元格读起来像 duster 忘了做这件事。
fn render_ping(servers: &[MergedServer], results: &[PingResult]) -> String {
    if results.is_empty() {
        return format!("  {}", muted().apply_to(EMPTY_LIST));
    }
    let mut t = Table::new(vec!["SERVER", "AGENT", "OK", "MS", "INFO"]);
    t.color_col(0, accent());
    t.right_align(&[3]);
    t.color_col(4, muted());
    for (s, r) in servers.iter().zip(results) {
        let info = r
            .error
            .clone()
            .or_else(|| r.server_info.clone())
            .unwrap_or_else(|| "handshake ok, no serverInfo reported".to_string());
        t.push_row(vec![
            r.name.clone(),
            r.agent_id.clone(),
            if r.ok {
                "yes".to_string()
            } else if s.spec.transport == McpTransport::Stdio {
                "no".to_string()
            } else {
                // 非 stdio 不是"失败",是"不适用"。
                "n/a".to_string()
            },
            r.ms.map(human_ms).unwrap_or_else(|| "-".to_string()),
            truncate_width(&info, INFO_WIDTH),
        ]);
    }
    let ok = results.iter().filter(|r| r.ok).count();
    format!(
        "{}\n\n  {} {} {}",
        t.render(),
        muted().apply_to("Answered"),
        style(format!("{ok}/{}", results.len())).bold(),
        muted().apply_to("· one `initialize` each, then duster hung up")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use duster_core::skill_ops::DupState;

    /// 断言一律看剥掉 ANSI 之后的文本：着色由 console 按 tty 决定，
    /// 测试要守的是措辞与结构，不是转义字节。
    fn plain(s: &str) -> String {
        console::strip_ansi_codes(s).to_string()
    }

    fn stdio(name: &str, cmd: &str, args: &[&str], env: &[(&str, &str)]) -> McpServerSpec {
        McpServerSpec {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: Some(cmd.to_string()),
            args: args.iter().map(|s| (*s).to_string()).collect(),
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            url: None,
            headers: BTreeMap::new(),
            extra: None,
        }
    }

    fn decl(agent: &str, dialect: &str) -> Declaration {
        Declaration {
            agent_id: agent.to_string(),
            path: PathBuf::from(format!("/home/u/.{agent}/mcp.json")),
            dialect: dialect.to_string(),
            state: DupState::Identical,
            last_used_ms: None,
        }
    }

    fn merged(hash: &str, spec: McpServerSpec, decls: Vec<Declaration>) -> MergedServer {
        MergedServer {
            name: spec.name.clone(),
            content_hash: hash.to_string(),
            spec,
            declared_in: decls,
        }
    }

    /// 铺平表:一行一条声明,组名只印首行,STATE 列说清合并结论;冲突块
    /// 点名两侧,小结同时报出组数与声明数,drifted 组在尾行点一句名。
    #[test]
    fn list_铺平成声明行且状态与冲突块齐全() {
        let list = McpList {
            servers: vec![
                merged(
                    "aaaaaaaaaaaaaaaa1111",
                    stdio("stitch", "npx", &["-y", "@stitch/mcp"], &[]),
                    vec![
                        decl("claude-code", "mcp/standard-json"),
                        decl("codex", "mcp/codex-toml"),
                    ],
                ),
                merged(
                    "bbbbbbbbbbbbbbbb2222",
                    stdio("echo", "echo", &["hi"], &[]),
                    vec![Declaration {
                        state: DupState::Drifted,
                        ..decl("a1", "mcp/standard-json")
                    }],
                ),
                merged(
                    "cccccccccccccccc3333",
                    stdio("echo", "echo", &["bye"], &[]),
                    vec![Declaration {
                        state: DupState::Drifted,
                        ..decl("a2", "mcp/codex-toml")
                    }],
                ),
            ],
            conflicts: BTreeMap::from([(
                "echo".to_string(),
                vec![
                    "bbbbbbbbbbbbbbbb2222".to_string(),
                    "cccccccccccccccc3333".to_string(),
                ],
            )]),
            warnings: vec![],
        };

        let got = plain(&render_list(&list));

        // 铺平:每条声明一行。stitch 两组声明 → 两行;SERVER 列只在首行
        // 出现,第二行以空列开头(COMMAND 里的 `@stitch/mcp` 不算组名)。
        let stitch: Vec<&str> = got.lines().filter(|l| l.contains("stitch")).collect();
        assert_eq!(stitch.len(), 2, "两条声明应当占两行:\n{got}");
        // 表行带两格缩进,所以按 trim 后的行首认 SERVER 列。
        let stitch_names: Vec<&str> = got
            .lines()
            .filter(|l| l.trim_start().starts_with("stitch"))
            .collect();
        assert_eq!(stitch_names.len(), 1, "组名只印在首行:\n{got}");
        // 两处声明等价 → 每行 STATE 都是 identical。
        assert!(
            stitch.iter().all(|l| l.contains("identical")),
            "等价组每行都标 identical:\n{got}"
        );
        assert!(stitch[0].contains("claude-code"), "{got}");
        // 方言不单列一列(铺平表按任务口径:AGENT / TARGET / CONFIG TOUCHED /
        // PATH 足够认人,方言跟着 PATH 走),但冲突块里它随声明一起报出来。
        assert!(stitch[1].contains("codex"), "{got}");
        assert!(stitch[0].contains("npx -y @stitch/mcp"), "{got}");

        // 走散组:两行声明都标 drifted。同名不同内容的两组各占一块,组名在
        // 各自的首行各印一次(冲突块的 `conflict echo` 是块标题,不算)。
        assert_eq!(got.matches("drifted").count(), 2, "走散组每行都标 drifted:\n{got}");
        let echo_names: Vec<&str> = got
            .lines()
            .filter(|l| l.trim_start().starts_with("echo"))
            .collect();
        assert_eq!(echo_names.len(), 2, "同名两组的组名各印一次:\n{got}");

        // 冲突块:说清"同名不同义",两组各自的哈希与归属都在;不再给
        // diff 命令——差异就在上面的铺平表里。
        assert!(got.contains("conflict"), "{got}");
        assert!(
            got.contains("`echo` does not mean the same thing in every agent"),
            "{got}"
        );
        assert!(
            got.contains("bbbbbbbbbbbb…  a1 (mcp/standard-json)"),
            "{got}"
        );
        assert!(got.contains("cccccccccccc…  a2 (mcp/codex-toml)"), "{got}");
        assert!(!got.contains("mcp diff"), "diff 命令已随 mcp diff 删除:\n{got}");

        // 小结:3 组、4 处声明、1 个名字走散。
        assert!(got.contains("Total 3 servers"), "{got}");
        assert!(got.contains("from 4 declarations"), "{got}");
        assert!(
            got.contains("1 name declared differently in different agents"),
            "{got}"
        );

        // 空注册表给一句话,不给空表格。
        let empty = McpList {
            servers: vec![],
            conflicts: BTreeMap::new(),
            warnings: vec![],
        };
        assert!(plain(&render_list(&empty)).contains("No MCP server is indexed"));
    }

    /// 单家声明 → 只有一行,STATE 是 only copy。
    #[test]
    fn list_单家声明标_only_copy() {
        let list = McpList {
            servers: vec![merged(
                "dddddddddddddddd4444",
                stdio("solo", "echo", &["hi"], &[]),
                vec![Declaration {
                    state: DupState::Single,
                    ..decl("a1", "mcp/standard-json")
                }],
            )],
            conflicts: BTreeMap::new(),
            warnings: vec![],
        };
        let got = plain(&render_list(&list));
        assert!(got.contains("only copy"), "{got}");
        assert!(!got.contains("identical"), "{got}");
        assert_eq!(got.matches("solo").count(), 1, "组名只印一次:\n{got}");
        // 没有走散组时尾行不带 drifted 提醒。
        assert!(!got.contains("declared differently"), "{got}");
    }

    /// CONFIG TOUCHED 列：无时间戳 → `never`（不是 1970）；有值 → 相对时间。
    /// 语义是「这份配置上次被改是何时」，所以列名不叫 LAST USED——
    /// mcp 声明的 mtime 属于整份配置文件，不是这个 server 的调用记录。
    #[test]
    fn list_config_touched_无时间戳never_有值渲染相对时间() {
        let list = |last_used_ms: Option<i64>| McpList {
            servers: vec![merged(
                "eeeeeeeeeeeeeeee5555",
                stdio("solo", "echo", &["hi"], &[]),
                vec![Declaration {
                    last_used_ms,
                    ..decl("a1", "mcp/standard-json")
                }],
            )],
            conflicts: BTreeMap::new(),
            warnings: vec![],
        };

        let none = plain(&render_list(&list(None)));
        assert!(none.contains("CONFIG TOUCHED"), "列名必须是 CONFIG TOUCHED:\n{none}");
        assert!(none.contains("never"), "无时间戳渲染 never:\n{none}");

        // 固定过去时间戳（2023-11-14）：相对时间带 `ago` 后缀，且不是 never。
        let some = plain(&render_list(&list(Some(1_700_000_000_000))));
        assert!(some.contains("ago"), "有值渲染相对时间:\n{some}");
        assert!(!some.contains("never"), "{some}");
        assert!(!some.contains("LAST USED"), "mcp 不许用 LAST USED 列名:\n{some}");
    }

    /// 铺平后的浏览表每一行(含表头)加上控件前缀 `❯ [x] `(6 列)后,显示
    /// 宽度必须严格小于终端宽度——行宽碰到终端宽就会折成两个物理行,而控件
    /// 按逻辑行计数、`clear_last_lines` 按物理行擦,残影每按一次翻一倍。
    /// 数据把 TARGET / PATH 两列顶满(PATH 是 flex 列),60 列档恰好把它
    /// 逼到地板。CONFIG TOUCHED 也带真值(epoch → 5 位天数,与 14 宽的表头
    /// 持平,把这一列能吃下的余量吃干),None 与真值两条渲染路径都被压过。
    #[test]
    fn browse_rows_行宽不超终端() {
        let long = "~/Library/Application Support/Claude/claude_desktop_config.json";
        let decl_at = |agent: &str, path: &str| Declaration {
            path: PathBuf::from(path),
            last_used_ms: Some(0),
            ..decl(agent, "mcp/standard-json")
        };
        let list = McpList {
            servers: vec![
                merged(
                    "aaaaaaaaaaaaaaaa1111",
                    stdio(
                        "stitch",
                        "npx",
                        &["-y", "@stitch/mcp", "--with-a-very-long-flag"],
                        &[],
                    ),
                    vec![decl_at("claude-code", long), decl_at("codex", long)],
                ),
                merged(
                    "bbbbbbbbbbbbbbbb2222",
                    stdio("echo", "echo", &["hi"], &[]),
                    vec![decl_at("a1", long)],
                ),
            ],
            conflicts: BTreeMap::new(),
            warnings: vec![],
        };
        for cols in [60usize, 80, 200] {
            let (header, lines) = browse_rows_at(&list, cols);
            for line in std::iter::once(&header).chain(lines.iter()) {
                let w = Prefix::Checkbox.width() + display_width(line);
                assert!(
                    w < cols,
                    "{cols} 列终端:行宽 {w} 超限: {line}"
                );
            }
        }
    }

    /// `show` 的硬约束：env 与 headers 的原文一个字都不许上屏。
    #[test]
    fn show_不打印_env_与_header_原文() {
        const TOKEN: &str = "sk-live-9f3ac1b7d5e2481faaaa";
        const HEADER: &str = "Bearer zzz7788secretvalue001";
        let mut spec = stdio(
            "stitch",
            "npx",
            &["-y", "@stitch/mcp"],
            &[("STITCH_TOKEN", TOKEN), ("PLAIN", "false")],
        );
        spec.headers
            .insert("Authorization".to_string(), HEADER.to_string());
        spec.extra = Some(r#"{"enabled":true,"apiKey":"sk-should-never-print"}"#.to_string());
        let s = merged(
            "d4d4d4d4d4d4d4d4",
            spec,
            vec![decl("claude-code", "mcp/standard-json")],
        );

        let got = plain(&render_show(&s));

        assert!(!got.contains(TOKEN), "env 原文泄漏:\n{got}");
        assert!(!got.contains(HEADER), "header 原文泄漏:\n{got}");
        assert!(
            !got.contains("sk-should-never-print"),
            "extra 里的凭据泄漏:\n{got}"
        );
        // 泄露上限恰好是 mask 保留的首尾。
        assert!(got.contains(&mask(TOKEN)), "应当显示掩码值:\n{got}");
        assert!(got.contains(&mask(HEADER)), "{got}");
        // 键名要在:用户得知道这条声明带了哪些变量。
        assert!(got.contains("env STITCH_TOKEN"), "{got}");
        assert!(got.contains("env PLAIN"), "{got}");
        assert!(got.contains("header Authorization"), "{got}");
        // 非秘密字段照常展示。
        assert!(got.contains("npx"), "{got}");
        assert!(got.contains("-y @stitch/mcp"), "{got}");
        // 出处(agent / 方言 / 路径)已随铺平表撤下:详情只讲这一条声明
        // 长什么样,不在两个入口各说一半的谎。
        assert!(!got.contains("DECLARED IN"), "{got}");
        assert!(!got.contains("claude-code"), "{got}");
        assert!(!got.contains("mcp/standard-json"), "{got}");
    }

    // ---------------------- sync 的 dry-run ----------------------

    /// 一份最小 agent 清单：standard-json 方言，probe 根与资源路径都在假 home 内。
    fn manifest_of(id: &str) -> String {
        format!(
            r#"
[agent]
id = "{id}"
display_name = "{id}"

[probe]
any_of = ["~/.{id}"]

[[resource]]
kind = "mcp"
scope = "global"
path = "~/.{id}/mcp.json"
json_pointer = "/mcpServers"
mapper = "mcp/standard-json"
"#
        )
    }

    fn write_agent(home: &Path, id: &str, body: &str) -> PathBuf {
        fs::create_dir_all(home.join(".agent-duster/adapters")).unwrap();
        fs::write(
            home.join(".agent-duster/adapters")
                .join(format!("{id}.toml")),
            manifest_of(id),
        )
        .unwrap();
        let dir = home.join(format!(".{id}"));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("mcp.json");
        fs::write(&file, body).unwrap();
        file
    }

    /// 没给 `--yes` 就是预览：退出码 4，目标文件一个字节都不许变。
    #[test]
    fn sync_未给_yes_时不写一个字节且退出_4() {
        let home = tempfile::tempdir().unwrap();
        let src = write_agent(
            home.path(),
            "a1",
            r#"{"mcpServers":{"echo":{"command":"echo","args":["hi"]}}}"#,
        );
        let dst_body = "{\n  \"mcpServers\": {}\n}\n";
        let dst = write_agent(home.path(), "a2", dst_body);
        let index = home.path().join(".agent-duster/index.db");

        duster_core::scan::scan(&duster_core::scan::ScanOptions {
            home: Some(home.path().to_path_buf()),
            index_path: Some(index.clone()),
            full: true,
        })
        .unwrap();

        let plan = mcp::plan_sync(&SyncOptions {
            index_path: Some(index),
            home: Some(home.path().to_path_buf()),
            name: "echo".to_string(),
            from: Some("a1".to_string()),
            to: vec!["a2".to_string()],
            dry_run: true,
            yes: false,
        })
        .unwrap();

        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].action, "create", "{plan:?}");
        assert_eq!(plan.targets[0].snapshot, None, "预览不许留快照");
        assert_eq!(sync_exit(true, &plan.targets), EXIT_CONFIRM_DENIED);

        // 真正的证据:两份文件逐字节未变。
        assert_eq!(fs::read_to_string(&dst).unwrap(), dst_body);
        assert!(fs::read_to_string(&src).unwrap().contains("\"hi\""));

        let shown = plain(&render_sync(&plan.targets, true));
        assert!(shown.contains("not one byte has been written"), "{shown}");
        assert!(shown.contains("create"), "{shown}");
        assert!(shown.contains("a2"), "{shown}");
        assert!(shown.contains("--yes"), "{shown}");

        // 执行过后带 error 的目标算部分成功,全成算成功。
        let refused = SyncOutcome {
            agent_id: "a2".to_string(),
            path: "/x/config.toml".to_string(),
            action: "refused".to_string(),
            error: Some("schema drifted (fingerprint aaa -> bbb)".to_string()),
            snapshot: None,
        };
        assert_eq!(sync_exit(false, &plan.targets), EXIT_OK);
        assert_eq!(
            sync_exit(false, std::slice::from_ref(&refused)),
            EXIT_PARTIAL
        );
        // 拒写的原因与两个指纹必须原样出现,而且带着"没写"的明说。
        let shown = plain(&render_sync(&[refused], false));
        assert!(shown.contains("REFUSED"), "{shown}");
        assert!(shown.contains("fingerprint aaa -> bbb"), "{shown}");
        assert!(
            shown.contains("nothing was written to this file"),
            "{shown}"
        );
    }

    /// 用户在菜单里主动缩小范围（只勾一部分目标）不是失败：`skipped`
    /// 占位不带 error，退出码必须是 0；只有真的带着 error 回来
    /// （闸门拒写、不覆盖手写声明）才算部分成功（3）。
    #[test]
    fn sync_exit_未入选的目标不算失败() {
        let mk = |action: &str, error: Option<&str>| SyncOutcome {
            agent_id: "a2".to_string(),
            path: "/x/config.json".to_string(),
            action: action.to_string(),
            error: error.map(|s| s.to_string()),
            snapshot: None,
        };

        // 全成 + 用户没勾的占位：一个失败都没有。
        let out = vec![mk("create", None), mk("skipped", None)];
        assert_eq!(sync_exit(false, &out), EXIT_OK);

        // 真的失败了才升级成部分成功。
        let out = vec![
            mk("create", None),
            mk("skipped", None),
            mk("refused", Some("schema drifted (fingerprint aaa -> bbb)")),
        ];
        assert_eq!(sync_exit(false, &out), EXIT_PARTIAL);

        // skipped 的渲染是中性标记，不是绿色成功 ✓，文案说清是用户没选。
        let shown = plain(&render_sync(&[mk("skipped", None)], false));
        assert!(shown.contains("skipped"), "{shown}");
        assert!(shown.contains("not selected"), "{shown}");
        assert!(!shown.contains("✔"), "skipped 不该带成功标记: {shown}");
    }

    // ---------------------- rm ----------------------

    fn rm_outcome(action: &str, key: &str, error: Option<&str>) -> duster_core::mcp::RemoveOutcome {
        duster_core::mcp::RemoveOutcome {
            agent_id: "codex".to_string(),
            path: "/home/u/.codex/config.toml".to_string(),
            key: key.to_string(),
            action: action.to_string(),
            error: error.map(|s| s.to_string()),
            freed: 128,
        }
    }

    fn rm_report(outcomes: Vec<duster_core::mcp::RemoveOutcome>) -> RemoveReport {
        RemoveReport {
            freed_bytes: outcomes.iter().map(|o| o.freed).sum(),
            outcomes,
            snapshots: Some(PathBuf::from(
                "/home/u/.agent-duster/snapshots/20260814-090000-12345",
            )),
            warnings: vec![],
        }
    }

    /// 退出码三档：dry-run 什么都没做（4）；执行后有任何一条被拒算部分成功（3）；
    /// absent 不是失败——那是「本来就没有」的事实陈述。
    #[test]
    fn rm_exit_三档退出码() {
        assert_eq!(
            rm_exit(&rm_report(vec![rm_outcome("removed", "/mcpServers/stitch", None)]), true),
            EXIT_CONFIRM_DENIED,
            "dry-run 一个字节没写,按「什么都没做」落 4"
        );

        let ok = rm_report(vec![
            rm_outcome("removed", "/mcpServers/stitch", None),
            rm_outcome("absent", "", None),
        ]);
        assert_eq!(rm_exit(&ok, false), EXIT_OK, "absent 不是失败");

        let partial = rm_report(vec![
            rm_outcome("removed", "/mcpServers/stitch", None),
            rm_outcome(
                "refused",
                "",
                Some("structure changed (fingerprint aaa -> bbb)"),
            ),
        ]);
        assert_eq!(rm_exit(&partial, false), EXIT_PARTIAL);
    }

    /// 人类视图必须说清「哪个文件的哪个键会没」：每块带 agent、路径、键，
    /// dry-run 加「一个字节没写」的明说，收尾给快照落点与释放字节。mcp rm
    /// 真删不打包——文案里不许再出现导出目录。
    #[test]
    fn render_rm_报出文件_键_快照与释放字节() {
        let dry = rm_report(vec![rm_outcome("removed", "mcp_servers.stitch", None)]);
        let shown = plain(&render_rm(&dry, true));
        assert!(shown.contains("not one byte has been written"), "{shown}");
        assert!(shown.contains("removed"), "{shown}");
        assert!(shown.contains("codex"), "{shown}");
        assert!(shown.contains(".codex/config.toml"), "{shown}");
        assert!(shown.contains("mcp_servers.stitch"), "必须说出哪个键会没: {shown}");
        assert!(shown.contains("nothing written yet"), "{shown}");
        assert!(
            shown.contains("A real run would snapshot the whole config file"),
            "dry-run 要预告快照: {shown}"
        );
        assert!(shown.contains("128 B"), "释放字节要出现: {shown}");
        assert!(!shown.contains("agent-duster-exports"), "{shown}");

        // 拒写：REFUSED 红块 + 指纹 + 「没写」的明说。
        let refused = rm_report(vec![rm_outcome(
            "refused",
            "",
            Some("structure changed (fingerprint aaa -> bbb)"),
        )]);
        let shown = plain(&render_rm(&refused, false));
        assert!(shown.contains("REFUSED"), "{shown}");
        assert!(shown.contains("fingerprint aaa -> bbb"), "{shown}");
        assert!(shown.contains("nothing was written to this file"), "{shown}");

        // 真删过的报告：快照落点必须原样出现——改坏了从哪捞，这是唯一退路。
        let ran = rm_report(vec![rm_outcome("removed", "/mcpServers/stitch", None)]);
        let shown = plain(&render_rm(&ran, false));
        assert!(
            shown.contains("snapshot of the original files:"),
            "快照落点必须出现: {shown}"
        );
        assert!(
            shown.contains("/home/u/.agent-duster/snapshots/"),
            "快照落点要是可捞的路径: {shown}"
        );
        assert!(!shown.contains("agent-duster-exports"), "{shown}");

        // 没写盘（全 refused / absent）→ 报告里没有快照可指，渲染不编假话。
        let no_write = RemoveReport {
            snapshots: None,
            ..rm_report(vec![rm_outcome("removed", "/mcpServers/stitch", None)])
        };
        let shown = plain(&render_rm(&no_write, false));
        assert!(
            !shown.contains("snapshot of the original files"),
            "没写盘不许提快照: {shown}"
        );
    }
}
