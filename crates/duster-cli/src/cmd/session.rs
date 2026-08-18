//! `duster session`:会话的浏览、导出与压缩存档。
//!
//! 外壳零业务逻辑,全部动作落在 [`duster_core::session`]。这里只负责三件事:
//! 把旗标翻成 `SessionFilter` / `SessionPruneOptions`、按 `output.rs` 的排版
//! 契约渲染、映射退出码。
//!
//! # 为什么值得单开一个模块
//!
//! `main.rs` 已经装下了 M1 全部动词的渲染器。会话这一组有四个子命令,
//! 塞进去就得让读者在两千行里找「表格是怎么排的」。旧命令留在原地不动,
//! 新组各自成文件。
//!
//! # 这一组的两条底线
//!
//! - **`prune` 是同一套机器**:计划、归档、确认、退出码全部复用 `duster prune`
//!   的渲染器([`crate::render_plan`] / [`crate::render_archive`] /
//!   [`crate::render_prune_outcomes`]),一行都不抄。两个入口的安全性一旦分叉,
//!   用户还以为它们一样。
//! - **绝不猜**:cwd 取不到就印 `-`,轮次没有时间戳就不印时间。会话是聊天记录,
//!   在它上面编一个看起来合理的值,代价是用户拿错误的信息做删除决定。

use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use console::style;
use duster_core::delete::{DeleteOptions, DeleteReport};
use duster_core::plan::parse_older_than;
use duster_core::session::{
    self, ExportFormat, SessionDetail, SessionFilter, SessionPruneOptions, SessionRow,
};
use duster_core::session_migrate::{self, MigrateOptions, MigrateReport};
use duster_fs::path::display_tilde;
use serde::Serialize;

use crate::output::{
    EXIT_OK, EXIT_PARTIAL, OutputMode, Table, accent, display_width, emit_json, human_bytes,
    muted, ok_mark, truncate_width, warn_mark,
};

/// 列表默认只印这么多行。0 = 全部。
pub(crate) const DEFAULT_LIST_LIMIT: usize = 20;
/// `show` 默认只印这么多轮。0 = 全部;被截断时一定会印一行明示。
pub(crate) const DEFAULT_SHOW_LIMIT: usize = 20;

#[derive(Subcommand, Clone)]
pub enum SessionCmd {
    /// List conversations across every agent, most recently used first
    List {
        /// Only these agents, comma separated, e.g. --agent codex,omp
        #[arg(long = "agent", value_delimiter = ',', value_name = "AGENT")]
        agents: Vec<String>,
        /// Only conversations whose project path contains this text
        #[arg(long, value_name = "SUBSTR")]
        project: Option<String>,
        /// Only conversations last used before this age: 30d, 60d, 90d
        #[arg(long, value_name = "AGE")]
        older_than: Option<String>,
        /// Only conversations larger than this many bytes
        #[arg(long, value_name = "N")]
        min_bytes: Option<u64>,
        /// How many to show; 0 means all
        #[arg(long, default_value_t = DEFAULT_LIST_LIMIT)]
        limit: usize,
    },
    /// Print one whole conversation (the id comes from `duster session list`)
    Show {
        /// Conversation id from the ID column of `duster session list`
        rid: i64,
        /// How many turns to print; 0 means all
        #[arg(long, default_value_t = DEFAULT_SHOW_LIMIT)]
        limit: usize,
        /// Expand tool output and print every line
        #[arg(long)]
        full: bool,
    },
    /// Write one conversation out as Markdown or JSON
    Export {
        /// Conversation id from the ID column of `duster session list`
        rid: i64,
        /// markdown for reading, json for scripts
        #[arg(long, value_enum, default_value_t = Format::Markdown)]
        format: Format,
        /// Write to this file instead of stdout
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Plant a text-only copy of one conversation into another agent (lossy)
    Migrate {
        /// Conversation id from the ID column of `duster session list`, e.g. s42 or 42
        #[arg(value_name = "ID")]
        id: String,
        /// Where to plant it: claude-code, codex or omp
        #[arg(long, value_name = "AGENT")]
        to: String,
        /// Show what would be written and stop
        #[arg(long)]
        dry_run: bool,
    },
    /// Compress conversations you stopped using a while ago (content is kept)
    Prune {
        /// Only these agents, comma separated, e.g. --agent codex,omp
        #[arg(long = "agent", value_delimiter = ',', value_name = "AGENT")]
        agents: Vec<String>,
        /// How old is old enough, in days: 30d, 60d, 90d (required)
        #[arg(long, value_name = "AGE")]
        older_than: Option<String>,
        /// Write a Markdown copy of each one before compressing it
        #[arg(long)]
        export_first: bool,
        /// Do it. Without this you only get the plan
        #[arg(long)]
        yes: bool,
        /// Print the plan and stop (this is also what happens without --yes)
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete one conversation, after packing a readable copy into
    /// ~/agent-duster-exports (a conversation is a chat log; once gone it is gone)
    Rm {
        /// Conversation id from the ID column of `duster session list`, e.g. s42 or 42
        #[arg(value_name = "ID")]
        id: String,
        /// Delete without packing a copy first (for scripts)
        #[arg(long)]
        no_archive: bool,
        /// Print what would be deleted and stop
        #[arg(long)]
        dry_run: bool,
    },
}

/// 导出格式的命令行词汇表。与 [`ExportFormat`] 一一对应——CLI 不认第三种说法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Headings and prose, for reading
    Markdown,
    /// One JSON object, for scripts
    Json,
}

impl From<Format> for ExportFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Markdown => ExportFormat::Markdown,
            Format::Json => ExportFormat::Json,
        }
    }
}

pub fn run(mode: OutputMode, index: Option<&Path>, action: SessionCmd) -> i32 {
    match action {
        SessionCmd::List {
            agents,
            project,
            older_than,
            min_bytes,
            limit,
        } => run_list(
            mode,
            index,
            ListArgs {
                agents,
                project,
                older_than,
                min_bytes,
                limit,
            },
        ),
        SessionCmd::Show { rid, limit, full } => run_show(mode, index, rid, limit, full),
        SessionCmd::Export { rid, format, out } => {
            run_export(mode, index, rid, format, out.as_deref())
        }
        SessionCmd::Migrate { id, to, dry_run } => {
            run_migrate(mode, index, None, &id, &to, dry_run)
        }
        SessionCmd::Prune {
            agents,
            older_than,
            export_first,
            yes,
            dry_run,
        } => run_prune(
            mode,
            index,
            None,
            PruneArgs {
                agents,
                older_than,
                export_first,
                execute: crate::consent_of(yes, dry_run) == crate::Consent::Granted,
            },
        ),
        SessionCmd::Rm { id, no_archive, dry_run } => {
            run_rm(mode, index, None, &id, no_archive, dry_run)
        }
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

struct ListArgs {
    agents: Vec<String>,
    project: Option<String>,
    older_than: Option<String>,
    min_bytes: Option<u64>,
    limit: usize,
}

fn run_list(mode: OutputMode, index: Option<&Path>, args: ListArgs) -> i32 {
    let older_than_days = match &args.older_than {
        Some(s) => match parse_older_than(s) {
            Ok(d) => Some(d),
            Err(e) => {
                return crate::usage(mode, "session-list", &format!("--older-than {s}: {e:#}"));
            }
        },
        None => None,
    };
    let filter = SessionFilter {
        agents: args.agents,
        project: args.project,
        older_than_days,
        min_bytes: args.min_bytes,
        limit: args.limit,
        now_ms: None,
    };
    let list = match session::list(index, &filter) {
        Ok(l) => l,
        Err(e) => return crate::fail(mode, "session-list", &e),
    };
    match mode {
        OutputMode::Json => emit_json("session-list", &list.rows, &list.warnings),
        OutputMode::Human => {
            println!("{}", render_list(&list.rows));
            crate::render_warnings(&list.warnings);
        }
    }
    // 有 warning 意味着某场会话的源库打不开，这张表是**不全**的——
    // 按 output.rs 的口径那是部分成功，不是成功（与 memory list 同一条规矩）。
    if list.warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    }
}

/// 列表表格 + 一行合计。整块返回而不是边算边印:这样它能被测试逐字核对。
fn render_list(rows: &[SessionRow]) -> String {
    if rows.is_empty() {
        return format!(
            "\n  {}",
            muted().apply_to(
                "No conversations matched. If that looks wrong, run `duster scan` first."
            )
        );
    }
    let mut t = Table::new(vec!["ID", "AGENT", "PROJECT", "TURNS", "SIZE", "LAST USED"]);
    t.color_col(0, accent());
    // ID / TURNS / SIZE 是数字列。SIZE 带 `zst` 后缀时也右对齐——
    // 单位和标记一起贴着右边缘,数字仍然在同一条竖线上。
    // ID 带 `s` 前缀(会话 id,与 search 的 `t<tid>` 区分开),列宽照旧
    // 交给 Table 按 display_width 算,右对齐口径不用手调。
    t.right_align(&[0, 3, 4]);
    t.color_col(5, muted());
    for r in rows {
        t.push_row(vec![
            format!("s{}", r.rid),
            r.agent_id.clone(),
            project_cell(r.cwd.as_deref()),
            r.turns.to_string(),
            size_cell(r.bytes, r.compressed),
            crate::relative_time(r.last_turn_ms),
        ]);
    }
    let total: u64 = rows.iter().map(|r| r.bytes).sum();
    format!(
        "\n{}\n\n  {} {} {}",
        t.render(),
        muted().apply_to("Total"),
        style(crate::plural(rows.len(), "conversation")).bold(),
        muted().apply_to(format!(
            "· {} on disk · open one with `duster session show <id>`",
            human_bytes(total)
        ))
    )
}

/// 项目列:cwd 的最后两段。整条路径能把表撑到换行,而最后两段
/// (`Code/agent-duster`)已经足够认出是哪个项目。
///
/// cwd 是 None 时印占位横杠。**不从会话文件名反推**——Claude 的目录名
/// `-Users-me-proj` 是不可逆编码,反推出来的路径有一半是错的,而用户会拿
/// 这一列决定删哪场会话。
pub(crate) fn project_cell(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd else {
        return "-".to_string();
    };
    let parts: Vec<&str> = cwd.split('/').filter(|s| !s.is_empty()).collect();
    let short = match parts.len() {
        0 => cwd.to_string(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    };
    truncate_width(&short, 28)
}

/// 体积列。已压缩的带一个 `zst` 标记:prune 已经动过它了,而它照样读得出来——
/// 不标出来,用户会以为这场会话还没被处理过,又跑一次 prune 找不到收益。
pub(crate) fn size_cell(bytes: u64, compressed: bool) -> String {
    if compressed {
        format!("{} zst", human_bytes(bytes))
    } else {
        human_bytes(bytes)
    }
}

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

fn run_show(mode: OutputMode, index: Option<&Path>, rid: i64, limit: usize, full: bool) -> i32 {
    let mut detail = match session::show(index, rid) {
        Ok(d) => d,
        Err(e) => return crate::fail(mode, "session-show", &e),
    };
    let total = detail.turns.len();
    let notice = truncation_notice(total, limit);
    if limit > 0 {
        detail.turns.truncate(limit);
    }
    match mode {
        OutputMode::Json => {
            // 信封里装的是**印出去的那些轮次**,与人类模式看到的一致;
            // 截断这件事本身进 warnings,不让脚本误以为这就是全部。
            // 折叠与行数封顶是**显示层**的事,绝不进信封:脚本消费的是
            // 完整正文,`--json` 的输出要被 `jq` 原样吃下去,一个被压成
            // `## tool · bash · 2 KB` 的轮次对脚本就是数据丢失。`--full`
            // 只改人怎么读,不改信封里有什么。
            let warnings: Vec<String> = notice.iter().cloned().collect();
            emit_json("session-show", &detail, &warnings);
        }
        OutputMode::Human => {
            render_show(&detail, full);
            if let Some(n) = notice {
                println!();
                println!("  {}", muted().apply_to(n));
            }
        }
    }
    EXIT_OK
}

/// 截断告示。悄悄砍掉别人的对话是不可接受的:砍了就必须说砍了多少、
/// 以及怎么看全部。`limit == 0` 或没砍到东西时返回 None。
fn truncation_notice(total: usize, limit: usize) -> Option<String> {
    if limit == 0 || total <= limit {
        return None;
    }
    Some(format!(
        "… {} more turns, use --limit 0 for all",
        total - limit
    ))
}

/// 单轮正文超过这么多行就截断,除非 `--full`。40 行已经是一整屏,
/// 再长用户扫不动,而 `--full` 把全文还给要看的人。
const TURN_BODY_MAX_LINES: usize = 40;

/// 印一整场会话(`open` 拿到会话 id 时也走这里)。
///
/// 头部(`## {role}` 行)在这里印,因为只有这里拿得到时间戳;正文交给
/// [`print_turn_body`]。折叠的工具轮例外:它自己那一行就是全部,头部
/// 和正文一起省了,这里必须先跳过头部,否则 `## tool` 会跟着
/// `## tool · bash · 2 KB` 印成两行。
pub(crate) fn render_show(detail: &SessionDetail, full: bool) {
    print!("{}", show_body(detail, full));
}

/// 一整场会话的完整渲染。返回而不是边算边印:折叠工具轮之间的空行规矩
/// 只有逐字比对才守得住(与 [`render_list`] 同一个理由)。
fn show_body(detail: &SessionDetail, full: bool) -> String {
    let row = &detail.row;
    let dot = muted().apply_to("·").to_string();
    let rule = display_width(&row.path).clamp(16, 72);
    let mut out = format!(
        "\n  {} {dot} {} {dot} {} {dot} {}\n  {}\n  {}\n",
        accent().bold().apply_to(&row.agent_id),
        project_cell(row.cwd.as_deref()),
        crate::plural(row.turns as usize, "turn"),
        crate::relative_time(row.last_turn_ms),
        muted().apply_to(&row.path),
        muted().apply_to("─".repeat(rule)),
    );

    // 折叠成一行的工具轮连着出现时不再彼此空行:一串 `read` / `grep` 是
    // 一次动作的连续过程,挤成一块比隔行铺开好读,也省下半屏。
    let mut prev_collapsed = false;
    for turn in &detail.turns {
        let collapsed = turn.role == "tool" && !full;
        if !(collapsed && prev_collapsed) {
            out.push('\n');
        }
        // 轮次时间用相对说法,与 LAST USED 列、`duster status` 的 LAST SCAN
        // 同一套口径(CLI 全程不做日历算术,绝对时间戳在 `--json` 与
        // `session export --format markdown` 里)。没有时间戳就不印时间——
        // 编一个出来比不印更糟。
        if !collapsed {
            match turn.ts_ms {
                Some(_) => out.push_str(&format!(
                    "## {} · {}\n",
                    style(&turn.role).magenta(),
                    crate::relative_time(turn.ts_ms)
                )),
                None => out.push_str(&format!("## {}\n", style(&turn.role).magenta())),
            }
        }
        out.push_str(&render_turn_body(
            &turn.role,
            turn.tool.as_deref(),
            &turn.text,
            turn.raw_fallback,
            full,
        ));
        prev_collapsed = collapsed;
    }
    out
}

/// 印一轮正文:工具轮折叠成一行、超长正文截断并给出下一步。`full` = 用户要全文。
///
/// 头部(`## {role}` 行)由调用方先印——签名里没有时间戳,而 `session show`
/// 的头部要带相对时间。折叠的工具轮例外:这里直接印完整的一行
/// `## tool · {name} · {size}`,调用方必须先跳过自己的头部(判
/// `role == "tool" && !full`),否则同一轮会印出两行。
///
/// `session export` 不经过这里:导出是档案,一轮不缺。
pub(crate) fn print_turn_body(
    role: &str,
    tool: Option<&str>,
    text: &str,
    raw_fallback: bool,
    full: bool,
) {
    print!("{}", render_turn_body(role, tool, text, raw_fallback, full));
}

/// 一轮正文的完整渲染。返回而不是边算边印:这样它能被测试逐字核对
/// (与 [`render_list`] 同一个理由)。折叠、截断、raw 告示全在这里。
fn render_turn_body(
    role: &str,
    tool: Option<&str>,
    text: &str,
    raw_fallback: bool,
    full: bool,
) -> String {
    if role == "tool" && !full {
        // 工具输出占了会话体积的大头,而读者来这里是读对话,不是给
        // `read()` 的返回值做回放。折叠成一行,尺寸是唯一值得留下的信息;
        // 要读内容,`--full` 给全文。
        let name = tool.unwrap_or("output");
        return format!(
            "## {} · {} · {}\n",
            style(role).magenta(),
            style(name).bold(),
            muted().apply_to(human_bytes(text.len() as u64))
        );
    }

    let mut out = String::new();
    if raw_fallback {
        // 兜底行是原始 JSONL,直接印会把读者骗进一串转义里。明说一句
        // 这是没解析出来,而不是假装它是散文。
        out.push_str(&format!(
            "  {}\n",
            muted().apply_to("(could not parse this turn — showing the raw line)")
        ));
    }

    let lines: Vec<&str> = text.lines().collect();
    if !full && lines.len() > TURN_BODY_MAX_LINES {
        let withheld = lines.len() - TURN_BODY_MAX_LINES;
        for l in &lines[..TURN_BODY_MAX_LINES] {
            out.push_str(l);
            out.push('\n');
        }
        // 砍了就必须说砍了多少、以及怎么看全部——和轮次截断同一条规矩。
        out.push_str(&format!(
            "  {}\n",
            muted().apply_to(format!("… {withheld} more lines · --full for all"))
        ));
    } else {
        out.push_str(text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// `--json` 信封里的导出回执。
///
/// 导出的正文进 `text` 字段而不是另起一条 stdout 流:`--json` 模式下
/// stdout 只能有一行信封,再多印一段正文,`| jq` 当场就崩。
/// 写了文件时 `text` 为 null——内容在盘上,信封里再抄一份只是把它说两遍。
#[derive(Serialize)]
struct ExportReceipt<'a> {
    rid: i64,
    format: &'a str,
    path: Option<String>,
    bytes: u64,
    text: Option<&'a str>,
}

fn run_export(
    mode: OutputMode,
    index: Option<&Path>,
    rid: i64,
    format: Format,
    out: Option<&Path>,
) -> i32 {
    let detail = match session::show(index, rid) {
        Ok(d) => d,
        Err(e) => return crate::fail(mode, "session-export", &e),
    };
    let fmt: ExportFormat = format.into();
    let name = match format {
        Format::Markdown => "markdown",
        Format::Json => "json",
    };

    let Some(out) = out else {
        let text = match session::export(&detail, fmt) {
            Ok(t) => t,
            Err(e) => return crate::fail(mode, "session-export", &e),
        };
        match mode {
            OutputMode::Json => emit_json(
                "session-export",
                &ExportReceipt {
                    rid,
                    format: name,
                    path: None,
                    bytes: text.len() as u64,
                    text: Some(&text),
                },
                &[],
            ),
            OutputMode::Human => {
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
        }
        return EXIT_OK;
    };

    // 落盘走 core:原子写是 IO 层的活,外壳一个字节都不碰盘。
    let bytes = match session::export_to_file(&detail, fmt, out) {
        Ok(n) => n,
        Err(e) => return crate::fail(mode, "session-export", &e),
    };
    let path = out.display().to_string();
    match mode {
        OutputMode::Json => emit_json(
            "session-export",
            &ExportReceipt {
                rid,
                format: name,
                path: Some(path),
                bytes,
                text: None,
            },
            &[],
        ),
        // 回执走 stderr:stdout 是结果,而这一路的结果已经在文件里了。
        // 用户可以 `duster session export 3 --out x.md > /dev/null` 而不丢回执。
        OutputMode::Human => eprintln!(
            "  {} {} {} {}",
            ok_mark(),
            muted().apply_to("wrote"),
            accent().apply_to(&path),
            muted().apply_to(human_bytes(bytes))
        ),
    }
    EXIT_OK
}

// ---------------------------------------------------------------------------
// migrate
// ---------------------------------------------------------------------------

/// `s42` / `42` -> 42。前缀只认小写 `s`(与 `session list` 印出来的一字不差),
/// 数字只认纯 ASCII 十进制——与 `main.rs` 的 `parse_open_id` 同一套判据,
/// 但这里没有轮次空间:migrate 只收会话 id,`t42` 也该死在这里。
fn parse_rid(s: &str) -> Result<i64, String> {
    let raw = s.trim();
    let rest = match raw.as_bytes().first() {
        Some(b's') => &raw[1..],
        _ => raw,
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "invalid conversation id {s:?}: expected s<number> or a bare <number> \
             from the ID column of `duster session list`, e.g. s42 or 42"
        ));
    }
    rest.parse().map_err(|_| {
        format!("invalid conversation id {s:?}: number is out of range")
    })
}

/// `duster session migrate`:有损文本移植,全部业务在
/// [`duster_core::session_migrate`]。这里只翻旗标、渲染回执、映射退出码。
///
/// `home` 只给测试注入:真实运行恒为 None(= 真实用户主目录),否则移植
/// 会落到两个地方。
fn run_migrate(
    mode: OutputMode,
    index: Option<&Path>,
    home: Option<&Path>,
    id: &str,
    to: &str,
    dry_run: bool,
) -> i32 {
    let rid = match parse_rid(id) {
        Ok(r) => r,
        Err(msg) => return crate::fail(mode, "session-migrate", &anyhow::anyhow!(msg)),
    };
    let report = match session_migrate::migrate(
        index,
        &MigrateOptions {
            rid,
            to: to.to_string(),
            dry_run,
            home: home.map(Path::to_path_buf),
            now_ms: None,
        },
    ) {
        Ok(r) => r,
        Err(e) => return crate::fail(mode, "session-migrate", &e),
    };
    match mode {
        OutputMode::Json => emit_json("session-migrate", &report, &[]),
        OutputMode::Human => print!("{}", render_migrate(&report)),
    }
    EXIT_OK
}

/// 迁移回执的人读排版。整块返回而不是边算边印:这样它能被测试逐字核对
/// (与 [`render_list`] 同一个理由)。
///
/// 两行:第一行是结果(种了几轮、种到哪),第二行明说这是**有损**副本——
/// 「工具调用没带过去」必须印出来,用户拿这份副本去目标 agent 续聊时,
/// 不该被上下文缺口吓一跳。丢弃数为 0 时不印括号:没有丢东西就不暗示丢了。
fn render_migrate(report: &MigrateReport) -> String {
    let target = display_tilde(Path::new(&report.target));
    let dropped = match report.tool_turns_dropped {
        0 => String::new(),
        n => format!(" ({} dropped)", crate::plural(n, "tool turn")),
    };
    if report.dry_run {
        format!(
            "  {} {} into {}{}\n  {}\n",
            muted().apply_to("would plant"),
            style(crate::plural(report.turns_planted, "text turn")).bold(),
            accent().apply_to(&target),
            muted().apply_to(&dropped),
            muted().apply_to("a text-only copy — tool calls are not carried over; drop --dry-run to write it")
        )
    } else {
        format!(
            "  {} {} {} into {}{}\n  {}\n",
            ok_mark(),
            muted().apply_to("planted"),
            style(crate::plural(report.turns_planted, "text turn")).bold(),
            accent().apply_to(&target),
            muted().apply_to(&dropped),
            muted().apply_to("a text-only copy — tool calls are not carried over; run `duster scan` to index it")
        )
    }
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

struct PruneArgs {
    agents: Vec<String>,
    older_than: Option<String>,
    export_first: bool,
    execute: bool,
}

/// `duster session prune`。计划、归档、逐项对账、退出码全部复用 `duster prune`
/// 的渲染器——两个入口印出来的必须是同一张单子。
///
/// `home` 只给测试注入:真实运行恒为 None(= 真实用户主目录),否则归档
/// 会落到两个地方。
fn run_prune(mode: OutputMode, index: Option<&Path>, home: Option<&Path>, args: PruneArgs) -> i32 {
    // 与 `duster prune` 同一道门:没有 `--older-than` 就没有「陈旧」的定义,
    // 外壳绝不替用户挑一个阈值。
    let days = match &args.older_than {
        Some(s) => match parse_older_than(s) {
            Ok(d) => d,
            Err(e) => {
                return crate::usage(mode, "session-prune", &format!("--older-than {s}: {e:#}"));
            }
        },
        None => {
            return crate::usage(
                mode,
                "session-prune",
                "session prune needs --older-than, e.g. --older-than 30d (30d / 60d / 90d)",
            );
        }
    };
    let opts = SessionPruneOptions {
        index_path: index.map(Path::to_path_buf),
        home: home.map(Path::to_path_buf),
        agents: args.agents,
        older_than_days: days,
        export_first: args.export_first,
        // 注入了假 home 就连导出目录一起注入:测试不许写进真实
        // `~/agent-duster-exports`。真实运行给 None,由 core 用默认目录。
        export_dir: home.map(|h| h.join("agent-duster-exports")),
        dry_run: !args.execute,
        yes: args.execute,
        json: mode == OutputMode::Json,
        now_ms: None,
    };
    let report = match session::prune(&opts) {
        Ok(r) => r,
        Err(e) => return crate::fail(mode, "session-prune", &e),
    };

    let failures = report.outcomes.iter().filter(|o| o.error.is_some()).count();
    let mut warnings = crate::merge_warnings(&report.plan.warnings, &report.warnings);
    let mut next = format!("duster session prune --older-than {days}d");
    next.push_str(" --yes");
    let hint = (!report.executed).then(|| crate::not_done_hint(false, &next));
    match mode {
        OutputMode::Json => {
            warnings.extend(hint);
            emit_json("session-prune", &report, &warnings);
        }
        OutputMode::Human => {
            crate::render_plan(&report.plan, false);
            crate::render_archive(report.archive_path.as_deref(), report.archive_bytes);
            crate::render_prune_outcomes(&report.outcomes);
            if report.executed {
                println!();
                println!(
                    "  {} {} {}",
                    ok_mark(),
                    style(human_bytes(report.freed_bytes)).green().bold(),
                    muted()
                        .apply_to("freed — the compressed conversations still read back as before")
                );
            }
            crate::finish_human(&warnings, hint.as_deref());
        }
    }
    crate::exec_exit_code(report.executed, failures)
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// `duster session rm`：归档后删除一场会话。全部业务在
/// [`duster_core::session::remove`]——先归档后删、库型会话走适配器删库行、
/// 删完清索引，都在那边。这里只翻旗标、渲染回执、映射退出码。
///
/// 归档默认开（会话是聊天记录，删了就没了）；`--no-archive` 是脚本用的
/// 显式关闭。`--dry-run` 只报将删什么，一个字节都不动。
///
/// `home` 只给测试注入：真实运行恒为 None（= 真实用户主目录），否则归档
/// 会落到两个地方。
fn run_rm(
    mode: OutputMode,
    index: Option<&Path>,
    home: Option<&Path>,
    id: &str,
    no_archive: bool,
    dry_run: bool,
) -> i32 {
    let rid = match parse_rid(id) {
        Ok(r) => r,
        Err(msg) => return crate::fail(mode, "session-rm", &anyhow::anyhow!(msg)),
    };
    let report = match duster_core::session::remove(
        &DeleteOptions {
            index_path: index.map(Path::to_path_buf),
            home: home.map(Path::to_path_buf),
            archive: !no_archive,
            dry_run,
        },
        &[rid],
    ) {
        Ok(r) => r,
        Err(e) => return crate::fail(mode, "session-rm", &e),
    };
    match mode {
        OutputMode::Json => emit_json("session-rm", &report, &report.warnings),
        OutputMode::Human => print!("{}", render_rm(&report, dry_run)),
    }
    crate::render_warnings(&report.warnings);
    if report.warnings.is_empty() {
        EXIT_OK
    } else {
        // 有条没删成（源库锁住、读不出内容）→ 部分成功。
        EXIT_PARTIAL
    }
}

/// 删除回执的人读排版。整块返回而不是边算边印：这样它能被测试逐字核对。
///
/// 干跑与真跑分行文：预览说 `would delete` 并把归档去处也预告出来；
/// 真跑说 `deleted` + 释放字节，归档包路径必须亮出来——那是用户唯一的退路。
fn render_rm(report: &DeleteReport, dry_run: bool) -> String {
    let mut out = format!(
        "\n  {} {} {}\n",
        if dry_run { warn_mark() } else { ok_mark() },
        style(if dry_run {
            format!(
                "would delete {} · {}",
                crate::plural(report.removed.len(), "conversation"),
                human_bytes(report.freed_bytes)
            )
        } else {
            format!(
                "deleted {} · {} freed",
                crate::plural(report.removed.len(), "conversation"),
                human_bytes(report.freed_bytes)
            )
        })
        .bold(),
        muted().apply_to(
            report
                .removed
                .iter()
                .map(|p| display_tilde(p))
                .collect::<Vec<_>>()
                .join(", ")
        )
    );
    match (&report.archived, dry_run) {
        (Some(archived), _) => out.push_str(&format!(
            "  {} {}\n",
            muted().apply_to("archived to"),
            accent().apply_to(display_tilde(archived))
        )),
        (None, true) => out.push_str(&format!(
            "  {}\n",
            muted().apply_to(
                "would archive a readable copy into ~/agent-duster-exports/ unless \
                 --no-archive is passed"
            )
        )),
        (None, false) => out.push_str(&format!(
            "  {}\n",
            muted().apply_to("no archive was made (--no-archive)")
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::EXIT_CONFIRM_DENIED;
    // 只有整块渲染的那条测试要自己造轮次;正文路径的测试直接喂字符串。
    use duster_core::session::TurnView;
    use tempfile::TempDir;

    /// 一个 codex 型会话:首行 `session_meta` 带 cwd,两条 `response_item`
    /// 消息。形状照抄 `fixtures/sessions/codex-basic.jsonl`——内置的
    /// `native/codex-session` 解析器只认这一种,写成别的形状会扫出
    /// 一个零轮次的空会话,而测试还以为自己在测正文。
    ///
    /// 时间钉在 2025-01-01:陈旧判定走真实时钟,一个固定的旧日期让
    /// `--older-than 30d` 这条断言不会随日历慢慢烂掉。
    const SESSION: &str = concat!(
        r#"{"timestamp":"2025-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"aaaa","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/Users/me/Code/agent-duster","cli_version":"1"}}"#,
        "\n",
        r#"{"timestamp":"2025-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello duster"}]}}"#,
        "\n",
        r#"{"timestamp":"2025-01-01T00:00:05.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"acknowledged"}]}}"#,
        "\n",
    );

    /// 假 home + 真扫描出来的索引。真实 `$HOME` 一个字节都不碰。
    /// 返回 (临时目录, home, 索引路径, 会话源文件)。
    fn fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let codex = home.join(".codex");
        std::fs::create_dir_all(codex.join("sessions/2025/01/01")).unwrap();
        std::fs::write(codex.join("config.toml"), b"# empty\n").unwrap();
        let src = codex.join("sessions/2025/01/01/rollout-2025-01-01T00-00-00-aaaa.jsonl");
        std::fs::write(&src, SESSION).unwrap();

        let index = home.join(".agent-duster/index.db");
        duster_core::scan::scan(&duster_core::scan::ScanOptions {
            home: Some(home.clone()),
            index_path: Some(index.clone()),
            full: false,
        })
        .expect("scan 夹具");
        (tmp, home, index, src)
    }

    fn only_rid(index: &Path) -> i64 {
        let rows = session::list(Some(index), &SessionFilter::default())
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1, "夹具里只该有一场会话");
        rows[0].rid
    }

    fn row(rid: i64, cwd: Option<&str>, bytes: u64, compressed: bool) -> SessionRow {
        SessionRow {
            rid,
            agent_id: "codex".into(),
            key: "k.jsonl".into(),
            path: "/tmp/k.jsonl".into(),
            cwd: cwd.map(str::to_string),
            bytes,
            turns: 4,
            last_turn_ms: None,
            compressed,
        }
    }

    /// 项目列只取最后两段,取不到就是横杠——绝不从文件名反推一个像样的路径。
    #[test]
    fn project_列取最后两段_未知即横杠() {
        assert_eq!(
            project_cell(Some("/Users/me/Code/agent-duster")),
            "Code/agent-duster"
        );
        assert_eq!(project_cell(Some("/w")), "w");
        assert_eq!(project_cell(Some("/")), "/");
        assert_eq!(project_cell(None), "-");
    }

    /// 已压缩的会话必须在体积格里带标记:prune 动过它了,而它照样读得出来。
    #[test]
    fn size_格标出已压缩() {
        assert_eq!(size_cell(2048, false), "2 KB");
        assert_eq!(size_cell(2048, true), "2 KB zst");
    }

    /// 表头、行内容与页脚合计:这张表是用户挑 rid 的唯一依据。
    #[test]
    fn list_表有六列且页脚报合计() {
        let out = render_list(&[
            row(7, Some("/Users/me/Code/agent-duster"), 1024, false),
            row(8, None, 1024, true),
        ]);
        assert!(out.contains("ID") && out.contains("PROJECT") && out.contains("LAST USED"));
        assert!(out.contains("Code/agent-duster"), "{out}");
        assert!(out.contains("1 KB zst"), "压缩标记必须出现在表里:{out}");
        assert!(out.contains("2 conversations"), "{out}");
        assert!(out.contains("2 KB"), "页脚要报合计体积:{out}");
        // 空结果不印表头,只给一句下一步。
        assert!(render_list(&[]).contains("duster scan"));
    }

    /// ID 列带 `s` 前缀,且前缀没把数字列的对齐带歪:ID 是右对齐列,
    /// `s7` 与 `s500` 宽度不同,AGENT 列(紧跟其后)的起点必须一致——
    /// 这只有列宽按 display_width 取最大值、右缘对齐才成立。CJK 行
    /// 混进来也不能歪(项目列宽度同样走 display_width)。
    #[test]
    fn list_表_id_带_s_前缀且列对齐不歪() {
        fn wide_row(rid: i64, project: &str) -> SessionRow {
            SessionRow {
                rid,
                agent_id: "codex".into(),
                key: "k.jsonl".into(),
                path: "/tmp/k.jsonl".into(),
                cwd: Some(project.to_string()),
                bytes: 1024,
                turns: 4,
                last_turn_ms: None,
                compressed: false,
            }
        }
        let out = render_list(&[
            wide_row(7, "/Users/me/Code/agent-duster"),
            wide_row(500, "/Users/me/Code/项目"),
        ]);
        // ID 格带 s 前缀。
        assert!(out.contains("s7"), "{out}");
        assert!(out.contains("s500"), "{out}");
        // 两条数据行里 AGENT(codex)与项目列(Code/)的起点分别一致:
        // ID 右对齐 + CJK 列宽按显示宽度算,列竖线没歪。
        let data: Vec<&str> = out.lines().filter(|l| l.contains("codex")).collect();
        assert_eq!(data.len(), 2, "该有两条数据行:{out}");
        let codex_at: Vec<usize> = data.iter().map(|l| l.find("codex").unwrap()).collect();
        assert_eq!(codex_at[0], codex_at[1], "ID 列右对齐被带歪:{out}");
        let code_at: Vec<usize> = data.iter().map(|l| l.find("Code/").unwrap()).collect();
        assert_eq!(code_at[0], code_at[1], "CJK 项目列把列起点带歪:{out}");
    }

    /// 截断必须明示,且要说清怎么看全部。
    #[test]
    fn show_截断时告知还剩多少轮() {
        assert_eq!(
            truncation_notice(50, 20).unwrap(),
            "… 30 more turns, use --limit 0 for all"
        );
        assert_eq!(truncation_notice(20, 20), None, "刚好装下不该报截断");
        assert_eq!(truncation_notice(999, 0), None, "--limit 0 就是要全部");
    }

    /// 导出往返:写出去的文件读回来必须是同一份内容,且 JSON 可解析。
    #[test]
    fn export_写文件后读回来可解析() {
        let (_tmp, home, index, _src) = fixture();
        let rid = only_rid(&index);
        let out = home.join("dump/session.json");

        assert_eq!(
            run_export(
                OutputMode::Human,
                Some(&index),
                rid,
                Format::Json,
                Some(&out)
            ),
            EXIT_OK
        );
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(back["row"]["agent_id"], "codex");
        assert_eq!(back["turns"].as_array().unwrap().len(), 2);
        assert!(
            back["turns"][0]["text"]
                .as_str()
                .unwrap()
                .contains("hello duster"),
            "导出里必须有真正的对话正文:{back}"
        );

        let md = home.join("dump/session.md");
        assert_eq!(
            run_export(
                OutputMode::Human,
                Some(&index),
                rid,
                Format::Markdown,
                Some(&md)
            ),
            EXIT_OK
        );
        assert!(std::fs::read_to_string(&md).unwrap().contains("## user"));
    }

    /// 工具轮折叠成一行:带工具名与字节尺寸,正文一个字都不印。
    /// `--full` 展开成完整正文。
    #[test]
    fn 工具轮折叠成一行_full_展开() {
        let body = "目录清单\n".repeat(60);
        let folded = render_turn_body("tool", Some("bash"), &body, false, false);
        assert_eq!(folded.lines().count(), 1, "折叠后必须恰好一行:{folded}");
        assert!(folded.contains("## tool"), "{folded}");
        assert!(folded.contains("bash"), "{folded}");
        assert!(folded.contains("780 B"), "一行里要带字节尺寸:{folded}");
        assert!(
            !folded.contains("目录清单"),
            "正文一个字都不许进折叠行:{folded}"
        );

        let expanded = render_turn_body("tool", Some("bash"), &body, false, true);
        assert_eq!(expanded, body, "--full 给完整正文,一个字节都不砍");
    }

    /// 工具轮没有工具名时用占位词,不能印出一个裸的 `## tool · · 2 KB`。
    #[test]
    fn 工具轮无工具名时用占位词() {
        let folded = render_turn_body("tool", None, "x", false, false);
        assert_eq!(
            console::strip_ansi_codes(&folded),
            "## tool · output · 1 B\n"
        );
    }

    /// 连着的折叠工具轮之间不留空行,而换到别的角色时必须空一行隔开。
    ///
    /// 这条规矩是「一屏能读几轮」的全部差别:14 轮会话隔行铺开要 37 行,
    /// 挤成块只要 23 行。它只在整块渲染上看得出来,所以在这里逐行比对。
    #[test]
    fn 连续折叠的工具轮之间不空行() {
        let turn = |role: &str, tool: Option<&str>| TurnView {
            seq: 0,
            role: role.to_string(),
            ts_ms: None,
            text: "x".to_string(),
            tool: tool.map(str::to_string),
            raw_fallback: false,
        };
        let detail = SessionDetail {
            row: SessionRow {
                rid: 1,
                agent_id: "omp".into(),
                key: "s.jsonl".into(),
                path: "/tmp/s.jsonl".into(),
                cwd: None,
                bytes: 1,
                turns: 4,
                last_turn_ms: None,
                compressed: false,
            },
            turns: vec![
                turn("user", None),
                turn("tool", Some("read")),
                turn("tool", Some("grep")),
                turn("assistant", None),
            ],
        };
        let plain = console::strip_ansi_codes(&show_body(&detail, false)).to_string();
        let tail: Vec<&str> = plain
            .lines()
            .skip_while(|l| !l.starts_with("## user"))
            .collect();
        assert_eq!(
            tail,
            vec![
                "## user",
                "x",
                "",
                "## tool · read · 1 B",
                "## tool · grep · 1 B",
                "",
                "## assistant",
                "x",
            ],
            "折叠块要连成一片,前后各空一行:{plain}"
        );
    }

    /// 100 行正文截到 40 行,告示必须说清砍了多少、以及怎么看全部。
    #[test]
    fn 超长正文截断到_40_行并报被扣行数() {
        let body: String = (0..100)
            .map(|i| format!("line {i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = render_turn_body("user", None, &body, false, false);
        let plain = console::strip_ansi_codes(&out);
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines.len(), 41, "40 行正文 + 1 行告示");
        assert!(lines[0].starts_with("line 000"), "从开头截,不许从中间");
        assert!(lines[39].starts_with("line 039"));
        assert!(
            plain.contains("… 60 more lines · --full for all"),
            "告示要报被扣的 60 行并指路 --full:{plain}"
        );
        assert!(!plain.contains("line 040"), "第 41 行不该出现");

        // --full 免截断,100 行全在。
        let full = render_turn_body("user", None, &body, false, true);
        assert_eq!(full.lines().count(), 100, "--full 一行不砍");
    }

    /// raw 兜底必须明说,不许把原始 JSONL 当散文悄悄印过去。
    #[test]
    fn raw_兜底印告示行() {
        let out = render_turn_body("assistant", None, "{\"raw\":true}", true, false);
        let plain = console::strip_ansi_codes(&out);
        assert!(
            plain.contains("could not parse this turn"),
            "必须说明这一轮没解析出来:{plain}"
        );
        assert!(
            plain.contains("{\"raw\":true}"),
            "正文还是给出来,不能假装没内容"
        );
    }

    /// 星标不是删掉界面就完事:clap 的定义里残留一个 `star` / `--keep-starred`,
    /// 用户敲出来还是会跑(或者撞一个说不清的错)。直接拿命令树证明
    /// 这两个词从命令行词汇表里彻底消失。
    #[test]
    fn 命令行词汇表里没有_star_也没有_keep_starred() {
        use clap::CommandFactory;
        let cli = crate::Cli::command();
        let session = cli
            .find_subcommand("session")
            .expect("session 子命令必须还在");
        let names: Vec<&str> = session.get_subcommands().map(|c| c.get_name()).collect();
        assert!(!names.contains(&"star"), "session 下仍有 star:{names:?}");
        assert!(
            !names.contains(&"unstar"),
            "session 下仍有 unstar:{names:?}"
        );

        let prune = session
            .find_subcommand("prune")
            .expect("session prune 必须还在");
        let flags: Vec<&str> = prune.get_arguments().filter_map(|a| a.get_long()).collect();
        assert!(
            !flags.contains(&"keep-starred"),
            "session prune 仍有 --keep-starred:{flags:?}"
        );
    }

    /// 没有 `--yes` 就是预览:退出码 4,源文件一个字节都不许动。
    #[test]
    fn prune_预览不动盘且退出码_4() {
        let (_tmp, home, index, src) = fixture();
        let before = std::fs::read(&src).unwrap();
        // 先证明这场会话真的在 30 天档的射程内:否则「什么都没删」也可能只是
        // 因为压根没东西可删,这条断言就成了摆设。
        assert_eq!(
            session::list(
                Some(&index),
                &SessionFilter {
                    older_than_days: Some(30),
                    ..Default::default()
                }
            )
            .unwrap()
            .rows
            .len(),
            1
        );

        let code = run_prune(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            PruneArgs {
                agents: Vec::new(),
                older_than: Some("30d".into()),
                export_first: true,
                execute: false,
            },
        );
        assert_eq!(code, EXIT_CONFIRM_DENIED);
        assert_eq!(std::fs::read(&src).unwrap(), before, "预览不许改源文件");
        assert!(
            !home.join("agent-duster-exports").exists(),
            "预览不许写导出目录"
        );

        // `--older-than` 缺失或写错都是用法错误,不许替用户挑一个阈值。
        for older in [None, Some("30".to_string())] {
            let code = run_prune(
                OutputMode::Human,
                Some(&index),
                Some(&home),
                PruneArgs {
                    agents: Vec::new(),
                    older_than: older,
                    export_first: false,
                    execute: true,
                },
            );
            assert_eq!(code, crate::output::EXIT_USAGE);
        }
        assert_eq!(std::fs::read(&src).unwrap(), before);
    }

    /// id 只认 `s<number>` 与裸 `<number>`:`t42` 是轮次 id,粘错空间必须
    /// 死在解析层,不能落到「查无此会话」那种误导性的错。
    #[test]
    fn migrate_id_解析只认会话空间() {
        assert_eq!(parse_rid("42"), Ok(42));
        assert_eq!(parse_rid("s42"), Ok(42));
        assert_eq!(parse_rid(" s7 "), Ok(7));
        for bad in ["t42", "S42", "s", "", "s-1", "-3", "4 2", "s4.2"] {
            assert!(parse_rid(bad).is_err(), "{bad:?} 该被拒绝");
        }
    }

    /// 回执措辞:种了几轮、种到哪、丢了几轮、有损明示。0 丢弃不印括号。
    #[test]
    fn migrate_回执措辞() {
        let mut report = duster_core::session_migrate::MigrateReport {
            rid: 7,
            from: "codex".into(),
            to: "omp".into(),
            target: "/tmp/x.jsonl".into(),
            turns_planted: 4,
            tool_turns_dropped: 1,
            dry_run: false,
        };
        let out = render_migrate(&report);
        assert!(out.contains("planted 4 text turns into"), "{out}");
        assert!(out.contains("/tmp/x.jsonl"), "{out}");
        assert!(out.contains("(1 tool turn dropped)"), "{out}");
        assert!(out.contains("tool calls are not carried over"), "{out}");
        assert!(out.contains("duster scan"), "{out}");

        report.dry_run = true;
        report.tool_turns_dropped = 0;
        let out = render_migrate(&report);
        assert!(out.contains("would plant 4 text turns into"), "{out}");
        assert!(!out.contains("dropped"), "0 丢弃不许暗示丢了东西:{out}");
        assert!(out.contains("drop --dry-run"), "{out}");
    }

    /// 整条命令走通:codex 夹具 → 归档后源文件没了、列表不再列出;dry-run
    /// 不动盘也不写导出目录。核心逻辑的逐字节断言在 duster-core 的
    /// session::remove 测试里,这里只钉外壳的接线与退出码。
    #[test]
    fn rm_接线_归档删除_dry_run() {
        let (_tmp, home, index, src) = fixture();
        let rid = only_rid(&index);

        // dry-run:退出 0,源文件与导出目录都不动。
        let code = run_rm(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            &format!("s{rid}"),
            false,
            true,
        );
        assert_eq!(code, EXIT_OK);
        assert!(src.is_file(), "预览不许动源文件");
        assert!(
            !home.join("agent-duster-exports").exists(),
            "预览不许写导出目录"
        );

        // 真跑:源文件没了,归档包出现,列表不再列出。
        let code = run_rm(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            &format!("s{rid}"),
            false,
            false,
        );
        assert_eq!(code, EXIT_OK);
        assert!(!src.exists(), "源文件应已删除");
        let exports = home.join("agent-duster-exports");
        let archives: Vec<_> = std::fs::read_dir(&exports)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "zst"))
            .collect();
        assert_eq!(archives.len(), 1, "归档包应出现: {archives:?}");
        assert!(archives[0].is_file(), "{}", archives[0].display());
        let rows = session::list(Some(&index), &SessionFilter::default())
            .unwrap()
            .rows;
        assert!(rows.is_empty(), "删完列表不能再列出: {rows:?}");
    }

    /// 回执措辞:真跑报 deleted + freed + 归档去处;干跑报 would delete 并
    /// 预告归档。整块渲染逐字核对。
    #[test]
    fn rm_回执措辞() {
        let mut report = duster_core::delete::DeleteReport {
            archived: Some("/tmp/exports/session-rm-20260101-000000.tar.zst".into()),
            removed: vec!["/Users/me/.codex/sessions/x.jsonl".into()],
            freed_bytes: 2048,
            warnings: vec![],
        };
        let out = render_rm(&report, false);
        assert!(out.contains("deleted 1 conversation"), "{out}");
        assert!(out.contains("2 KB freed"), "{out}");
        assert!(out.contains("archived to"), "{out}");
        assert!(out.contains("session-rm-20260101-000000.tar.zst"), "{out}");

        report.archived = None;
        let out = render_rm(&report, true);
        assert!(out.contains("would delete 1 conversation"), "{out}");
        assert!(out.contains("would archive"), "{out}");
        assert!(!out.contains("freed"), "干跑不许说 freed:{out}");
    }

    /// 命令行词汇表里必须有 `rm`,且它只收会话 id 一种位置参数。
    #[test]
    fn 命令行词汇表里有_session_rm() {
        use clap::CommandFactory;
        let cli = crate::Cli::command();
        let session = cli.find_subcommand("session").unwrap();
        let names: Vec<&str> = session.get_subcommands().map(|c| c.get_name()).collect();
        assert!(names.contains(&"rm"), "{names:?}");
    }

    /// 整条命令走通:codex 夹具 → omp 目标落盘;dry-run 不碰盘;
    /// 名单外目标报错退出。核心逻辑的逐字节断言在 duster-core 的
    /// session_migrate 测试里,这里只钉外壳的接线与退出码。
    #[test]
    fn migrate_接线_落盘_dry_run_与名单外() {
        let (_tmp, home, index, _src) = fixture();
        let rid = only_rid(&index);

        // dry-run:报告成功但目标树一个目录都不长出来。
        let code = run_migrate(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            &format!("s{rid}"),
            "omp",
            true,
        );
        assert_eq!(code, EXIT_OK);
        assert!(!home.join(".omp").exists(), "dry-run 不许碰盘");

        // 真跑:落进 cwd 编码的 omp 会话目录,恰好一个文件。
        let code = run_migrate(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            &format!("s{rid}"),
            "omp",
            false,
        );
        assert_eq!(code, EXIT_OK);
        let dir = home.join(".omp/agent/sessions/-Users-me-Code-agent-duster");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "目标目录该恰好一份移植文件");

        // 名单外目标:非零退出,不落盘。
        let code = run_migrate(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            &format!("{rid}"),
            "gemini-cli",
            false,
        );
        assert_ne!(code, EXIT_OK);
        assert!(!home.join(".gemini").exists());

        // id 写错空间(t 前缀):死在解析层,同样非零退出。
        let code = run_migrate(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            "t1",
            "omp",
            false,
        );
        assert_ne!(code, EXIT_OK);
    }
}
