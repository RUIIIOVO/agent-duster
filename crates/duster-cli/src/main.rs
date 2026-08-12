//! duster CLI 入口。命令结构为「名词 + 动词」,外壳零业务逻辑:
//! 解析参数 → 调 duster-core 用例 → 按 output.rs 契约渲染并映射退出码。
//!
//! 人类模式的排版规则集中在这里:两格缩进、表格数字列右对齐、摘要行带
//! ✔ / ! 前缀。颜色一律经 console 的 `Style`,管道与 `NO_COLOR` 自动退化。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::builder::styling::AnsiColor;
use clap::{Parser, Subcommand};
use console::{Style, style};

use duster_core::clean::{Buckets, CleanOptions, CleanOutcome, clean, clean_filtered};
use duster_core::doctor::{ALL_CHECKS, DoctorOptions, DoctorReport, Finding, Severity, doctor};
use duster_core::plan::{Action, Plan, PlanFilter, PlanItem, Verb, parse_older_than};
use duster_core::prune::{PruneOptions, PruneOutcome, prune, prune_filtered};
use duster_core::scan::{ScanOptions, ScanReport, scan};
use duster_core::search::{SearchFilter, SearchHit, TurnDetail, open_turn, search};
use duster_core::session::SessionDetail;
use duster_core::skill_ops::{DupState, LinkMode, LinkReport, SkillGroup, copies, link};
use duster_core::status::{StatusReport, status};
use duster_core::uninstall::{
    PackageOutcome, PreflightCheck, SharedAction, SharedEditOutcome, UninstallOptions, uninstall,
};
use duster_model::CleanLevel;

mod browse;
mod cmd;
mod interactive;
mod output;
mod prompt;
use output::{
    EXIT_CONFIRM_DENIED, EXIT_ERROR, EXIT_LOCKED, EXIT_OK, EXIT_PARTIAL, EXIT_USAGE, OutputMode,
    Table, accent, display_width, emit_json, emit_json_error, human_bytes, human_ms, is_json_mode,
    muted, ok_mark, truncate_width, warn_mark,
};

/// `search` 默认返回这么多条结果,与交互菜单用同一个常量,不再照抄。
pub(crate) const DEFAULT_SEARCH_LIMIT: usize = 20;

/// 帮助与报错的配色。clap 自带 anstyle,不引新依赖;非 tty 时 clap 自动关色。
const HELP_STYLES: clap::builder::Styles = clap::builder::Styles::styled()
    .header(AnsiColor::Cyan.on_default().bold())
    .usage(AnsiColor::Cyan.on_default().bold())
    .literal(AnsiColor::Green.on_default().bold())
    .placeholder(AnsiColor::Yellow.on_default())
    .error(AnsiColor::Red.on_default().bold())
    .valid(AnsiColor::Green.on_default())
    .invalid(AnsiColor::Yellow.on_default());

#[derive(Parser)]
#[command(
    name = "duster",
    version,
    about = "See and clean up what your AI coding agents leave on disk",
    after_help = examples(),
    styles = HELP_STYLES,
)]
struct Cli {
    /// Print one line of JSON instead of tables (for scripts)
    #[arg(long, global = true)]
    json: bool,
    /// Index database path (for tests; defaults to ~/.agent-duster/index.db)
    #[arg(long, global = true, hide = true, value_name = "PATH")]
    index: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// 帮助末尾的示例区。着色走 console(帮助写 stdout,tty 判定与之一致)。
fn examples() -> String {
    let head = Style::new().cyan().bold();
    let cmd = Style::new().green().bold();
    let dim = muted();
    let rows = [
        ("duster", "Open the menu and pick a command"),
        ("duster scan", "Find your agents and index what they store"),
        ("duster status", "See which agent uses how much disk"),
        (
            "duster search \"docker\"",
            "Find a past conversation by its words",
        ),
        ("duster open 42", "Read the whole turn that search found"),
        (
            "duster session list",
            "Browse your conversations by project and age",
        ),
        ("duster clean", "Reclaim caches, logs and SQLite slack"),
        (
            "duster prune --older-than 30d",
            "Archive and remove what you stopped using",
        ),
        (
            "duster uninstall qoder --confirm qoder",
            "Remove everything one agent keeps on disk",
        ),
        (
            "duster skill copies",
            "See which skills exist in more than one place",
        ),
        (
            "duster doctor --secrets",
            "Find API keys sitting in plain text",
        ),
        (
            "duster mcp list",
            "See every MCP server, merged across agents",
        ),
        (
            "duster memory list",
            "See what your agents remember about you",
        ),
        ("duster diff a b", "Compare any two files or folders"),
    ];
    let width = rows.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    let mut out = format!("{}\n", head.apply_to("Examples:"));
    for (c, what) in rows {
        let pad = " ".repeat(width - c.len());
        out.push_str(&format!(
            "  {}{pad}  {}\n",
            cmd.apply_to(c),
            dim.apply_to(what)
        ));
    }
    out
}

#[derive(Subcommand)]
enum Command {
    /// Find your agents and index what they store (only re-reads changed files)
    Scan {
        /// Re-read every conversation, even unchanged ones. Rarely needed —
        /// duster notices rule changes by itself; use this if search looks wrong
        #[arg(long)]
        full: bool,
    },
    /// See which agent uses how much disk, and what it keeps
    Status,
    /// Search the text of your past agent conversations
    Search {
        /// What to look for (3 characters or more; Chinese works too)
        query: String,
        /// Only these agents, comma separated, e.g. --agent codex,omp
        #[arg(long = "agent", value_delimiter = ',', value_name = "AGENT")]
        agents: Vec<String>,
        /// How many results to show
        #[arg(long, default_value_t = DEFAULT_SEARCH_LIMIT)]
        limit: usize,
    },
    /// Print one whole conversation turn, or a whole conversation
    Open {
        /// Turn id shown as `#42` in search results; a conversation id from
        /// `duster session list` works too
        tid: i64,
        /// Lift the turn-collapse and line cap: show tool bodies and every
        /// line (the same --full as `duster session show`)
        #[arg(long)]
        full: bool,
    },
    /// Reclaim caches, logs and SQLite slack (safe to run often)
    Clean {
        /// Only these agents, comma separated, e.g. --agent codex,omp
        #[arg(long = "agent", value_delimiter = ',', value_name = "AGENT")]
        agents: Vec<String>,
        /// Do it. Without this you only get the plan
        #[arg(long)]
        yes: bool,
        /// Print the plan and stop (this is also what happens without --yes)
        #[arg(long)]
        dry_run: bool,
    },
    /// Archive and remove what you stopped using a while ago
    Prune {
        /// Only these agents, comma separated, e.g. --agent codex,omp
        #[arg(long = "agent", value_delimiter = ',', value_name = "AGENT")]
        agents: Vec<String>,
        /// How old is old enough, in days: 30d, 60d, 90d (required)
        #[arg(long, value_name = "AGE")]
        older_than: Option<String>,
        /// Pack a copy into ~/agent-duster-exports before deleting
        #[arg(long)]
        archive: bool,
        /// Delete without packing a copy first
        #[arg(long, conflicts_with = "archive")]
        no_archive: bool,
        /// Do it. Without this you only get the plan
        #[arg(long)]
        yes: bool,
        /// Print the plan and stop (this is also what happens without --yes)
        #[arg(long)]
        dry_run: bool,
        /// Also prune surplus generations of resources that declare
        /// keep_generations (e.g. database backups that keep the newest N
        /// copies) — redundant by count, independent of --older-than
        #[arg(long)]
        keep_generations: bool,
    },
    /// Remove everything one agent keeps on disk
    Uninstall {
        /// Which agent to remove, e.g. duster uninstall qoder
        agent: String,
        /// Only its own folders and files. Without it, duster also removes
        /// this agent's entries from config files other agents own, and shows
        /// how the software itself was installed
        #[arg(long)]
        data_only: bool,
        /// Type the agent name here to go ahead: --confirm qoder
        #[arg(long, value_name = "AGENT")]
        confirm: Option<String>,
        /// Pack conversations and memory into ~/agent-duster-exports first.
        /// On by default — to leave them on disk instead, use --keep
        #[arg(long, default_value_t = true)]
        export_first: bool,
        /// Leave these where they are, e.g. --keep sessions,memory
        #[arg(long, value_delimiter = ',', value_name = "KIND")]
        keep: Vec<String>,
        /// Pack a copy into ~/agent-duster-exports before deleting
        #[arg(long)]
        archive: bool,
        /// Delete without packing a copy first
        #[arg(long, conflicts_with = "archive")]
        no_archive: bool,
        /// Print the plan and stop
        #[arg(long)]
        dry_run: bool,
        /// Let duster run the package-manager uninstall command it printed.
        /// Off by default: duster prints the command and stops
        #[arg(long)]
        run_package_manager: bool,
    },
    /// Compare the same skill across agents, or share one copy between them
    Skill {
        #[command(subcommand)]
        action: SkillCmd,
    },
    /// Show what your agents remember, and read one of those memories
    Memory {
        #[command(subcommand)]
        action: cmd::memory::MemoryCmd,
    },
    /// See every MCP server you have, merged across the agents that declare it
    Mcp {
        #[command(subcommand)]
        action: cmd::mcp::McpCmd,
    },
    /// Browse, export and archive your past agent conversations
    Session {
        #[command(subcommand)]
        action: cmd::session::SessionCmd,
    },
    /// Compare two files or two folders, line by line
    Diff(cmd::diff::DiffArgs),
    /// Check what your agents left behind: secrets, broken links, bad configs
    Doctor {
        /// Look for API keys and tokens stored in plain text
        #[arg(long)]
        secrets: bool,
        /// Start each MCP server once to see whether it still works
        #[arg(long)]
        ping: bool,
        /// Only these agents, comma separated, e.g. --agent codex,omp
        #[arg(long = "agent", value_delimiter = ',', value_name = "AGENT")]
        agents: Vec<String>,
        /// Run only this check. Repeat for more than one
        #[arg(
            long = "check",
            value_name = "NAME",
            value_parser = clap::builder::PossibleValuesParser::new(ALL_CHECKS),
        )]
        checks: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SkillCmd {
    /// List skills that exist in more than one place, and which copies drifted
    Copies,
    /// Give another agent the same skill, sharing one copy on disk
    Link {
        /// Skill name as `duster skill copies` prints it
        name: String,
        /// Agent that has it now
        #[arg(long)]
        from: String,
        /// Agent that should get it
        #[arg(long)]
        to: String,
        /// Print what would happen and stop
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let mode = is_json_mode(cli.json);
    let index = cli.index.as_deref();
    let code = match &cli.command {
        Some(Command::Scan { full }) => cmd_scan(mode, index, *full),
        Some(Command::Status) => cmd_status(mode, index),
        Some(Command::Search {
            query,
            agents,
            limit,
        }) => cmd_search(mode, index, query, agents.clone(), *limit),
        Some(Command::Open { tid, full }) => cmd_open(mode, index, *tid, *full),
        Some(Command::Clean {
            agents,
            yes,
            dry_run,
        }) => cmd_clean(mode, index, agents.clone(), consent_of(*yes, *dry_run)),
        Some(Command::Prune {
            agents,
            older_than,
            archive,
            no_archive,
            yes,
            dry_run,
            keep_generations,
        }) => cmd_prune(
            mode,
            index,
            agents.clone(),
            older_than.clone(),
            archive_choice(*archive, *no_archive),
            consent_of(*yes, *dry_run),
            *keep_generations,
        ),
        Some(Command::Uninstall {
            agent,
            data_only,
            confirm,
            export_first,
            keep,
            archive,
            no_archive,
            dry_run,
            run_package_manager,
        }) => cmd_uninstall(
            mode,
            index,
            UninstallArgs {
                agent: agent.clone(),
                data_only: *data_only,
                confirm: confirm.clone(),
                export_first: *export_first,
                keep: keep.clone(),
                archive: archive_choice(*archive, *no_archive),
                run_package_manager: *run_package_manager,
            },
            // uninstall 没有 `--yes`:授权凭据是逐字输入的 agent id。
            // 相等性由 cmd_uninstall 校验,不相等就降级成预览。
            if *dry_run {
                Consent::Preview
            } else {
                Consent::Granted
            },
        ),
        Some(Command::Skill { action }) => match action {
            SkillCmd::Copies => cmd_skill_copies(mode, index),
            SkillCmd::Link {
                name,
                from,
                to,
                dry_run,
            } => cmd_skill_link(mode, index, name, from, to, *dry_run),
        },
        Some(Command::Memory { action }) => cmd::memory::run(mode, index, action),
        Some(Command::Mcp { action }) => cmd::mcp::run(mode, index, action.clone()),
        Some(Command::Session { action }) => cmd::session::run(mode, index, action.clone()),
        Some(Command::Diff(args)) => cmd::diff::run(mode, args),
        Some(Command::Doctor {
            secrets,
            ping,
            agents,
            checks,
        }) => cmd_doctor(
            mode,
            index,
            DoctorArgs {
                secrets: *secrets,
                ping: *ping,
                agents: agents.clone(),
                checks: checks.clone(),
            },
        ),
        // 裸 `duster`:交互式启动菜单(TTY),否则打印帮助。
        None => interactive::run(mode, index),
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// 错误与退出码
// ---------------------------------------------------------------------------

/// "拒绝执行"的稳定文案。core 在这些情况下**一个字节都没动**:
/// prune 的 `--json` 无 `--yes`、归档预估超阈值而未表态、uninstall 的确认串
/// 不符或前置检查未过。语义是"需要确认被拒绝"(退出码 4),不是执行失败(1)——
/// 脚本据此区分"什么都没做"与"做坏了"。
///
/// 与锁文案同理:分层铁律禁止 CLI import 下层 crate,downcast 不可达,
/// 只能匹配 `Display` 的稳定子串,视作跨层契约。
const REFUSAL_PHRASES: [&str; 4] = [
    "refusing to execute",
    "auto-archive limit",
    "confirmation required",
    "preflight failed",
];

/// 从错误链推断退出码。
///
/// 锁冲突有两个来源,类型分别是 `duster_index::db::LockBusy`(duster 自己的
/// 索引单实例锁)与 `duster_fs::lockprobe::LockedError`(目标文件被 agent 占用),
/// 但分层铁律禁止 CLI import 这两个 crate,downcast 不可达;退而匹配它们
/// `Display` 里的稳定文案:
///
/// - `index database is locked by another duster instance: ...`
/// - `<path> is locked by another process: <reason>. Quit the agent and try again`
///
/// 两条都含 `is locked by`,所以只匹配这一截。该子串由错误类型固定输出,
/// 视作跨层契约(两侧各有断言守着),改文案等于改契约。
///
/// 锁先判:被锁本身也是一种拒绝,但它有自己的退出码与自己的解法(退出 agent
/// 再来),归进 4 就把这条唯一的行动建议冲掉了。都匹配不上落一般错误。
fn exit_code_for(err: &anyhow::Error) -> i32 {
    let hit = |needle: &str| err.chain().any(|e| e.to_string().contains(needle));
    if hit("is locked by") {
        EXIT_LOCKED
    } else if REFUSAL_PHRASES.iter().any(|p| hit(p)) {
        EXIT_CONFIRM_DENIED
    } else {
        EXIT_ERROR
    }
}

/// 退出码对应的机器可读短码(进 JSON 信封的 error.code)。
fn error_code_name(code: i32) -> &'static str {
    match code {
        EXIT_LOCKED => "locked",
        EXIT_PARTIAL => "partial",
        EXIT_USAGE => "usage",
        EXIT_CONFIRM_DENIED => "confirm-denied",
        _ => "error",
    }
}

/// 统一错误出口:人话链落 stderr;--json 时信封走 emit_json_error。
pub(crate) fn fail(mode: OutputMode, command: &str, err: &anyhow::Error) -> i32 {
    let code = exit_code_for(err);
    match mode {
        // emit_json_error 会同时把人话写 stderr,这里不再重复。
        OutputMode::Json => emit_json_error(command, error_code_name(code), &format!("{err:#}")),
        OutputMode::Human => {
            eprintln!();
            eprintln!(
                "  {} {}",
                output::err_mark(),
                style(format!("{err:#}")).red()
            );
        }
    }
    code
}

/// 用法错误出口:命令根本没法执行(`prune` 缺 `--older-than`、值不合法),
/// 与"执行失败"分开——和 clap 自己的用法错误同落退出码 2,脚本不必区分
/// "我参数写错了"是谁发现的。
fn usage(mode: OutputMode, command: &str, message: &str) -> i32 {
    match mode {
        OutputMode::Json => {
            emit_json_error(command, error_code_name(EXIT_USAGE), message);
        }
        OutputMode::Human => {
            eprintln!();
            eprintln!("  {} {}", output::err_mark(), style(message).red());
        }
    }
    EXIT_USAGE
}

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

fn cmd_scan(mode: OutputMode, index: Option<&Path>, full: bool) -> i32 {
    if mode == OutputMode::Human {
        let what = if full {
            "reading every file"
        } else {
            "only reading what changed (--full re-reads everything)"
        };
        eprintln!();
        eprintln!(
            "  {} {}",
            accent().bold().apply_to("Scanning…"),
            muted().apply_to(what)
        );
    }
    let report = match scan(&ScanOptions {
        home: None,
        index_path: index.map(Path::to_path_buf),
        full,
    }) {
        Ok(r) => r,
        Err(e) => return fail(mode, "scan", &e),
    };

    // agent 级 warnings 摊平进信封/stderr;有任何一条即视为部分成功(退出码 3)。
    let warnings: Vec<String> = report
        .agents
        .iter()
        .flat_map(|a| {
            a.warnings
                .iter()
                .map(move |w| format!("{}: {w}", a.agent_id))
        })
        .collect();

    match mode {
        OutputMode::Json => emit_json("scan", &report, &warnings),
        OutputMode::Human => render_scan_human(&report, &warnings),
    }
    if warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    }
}

fn render_scan_human(report: &ScanReport, warnings: &[String]) {
    let mut headers: Vec<String> = vec!["AGENT".into()];
    headers.extend(metric_headers());
    headers.push("WARN".into());
    let ncols = headers.len();

    let mut table = Table::new(headers);
    table.color_col(0, accent());
    table.right_align(&(1..ncols).collect::<Vec<_>>());
    let mut installed = 0usize;
    let mut clean: BTreeMap<String, u64> = BTreeMap::new();
    let mut install_bytes = 0u64;
    for a in &report.agents {
        if !a.installed {
            continue; // 未安装的 agent 无数据,人类模式不占版面(JSON 里仍完整)。
        }
        installed += 1;
        for (level, bytes) in &a.clean_bytes {
            *clean.entry(level.clone()).or_insert(0) += bytes;
        }
        install_bytes += a.kind_bytes.get("install").copied().unwrap_or(0);
        let mut row = vec![a.agent_id.clone()];
        row.extend(
            COUNT_KINDS
                .iter()
                .map(|k| count_cell(a.kind_counts.get(*k))),
        );
        row.extend(
            BYTE_KINDS
                .iter()
                .map(|(k, _)| bytes_cell(a.kind_bytes.get(*k))),
        );
        row.push(human_bytes(a.bytes));
        row.push(count_cell(Some(&a.warnings.len())));
        table.push_row(row);
    }

    println!();
    if table.is_empty() {
        println!(
            "  {}",
            muted().apply_to("No agents found in your home directory.")
        );
    } else {
        println!("{}", table.render());
    }

    // unclassified 分两拨,含义完全不同,混进一张表就都读不懂:
    //   · 无 agent 归属 = 整个 agent 都没清单,duster 只能报体积;
    //   · 有 agent 归属 = 清单漏声明了这一块,它的字节数是那个 agent 真实占用的
    //     一部分(`~/.codex/computer-use`),说成"没有适配器"是错的。
    let (undeclared, no_adapter): (Vec<_>, Vec<_>) =
        report.unclassified.iter().partition(|u| u.agent.is_some());
    if !no_adapter.is_empty() {
        let mut t = Table::new(vec!["FOLDER", "SIZE"]);
        t.color_col(0, muted());
        t.right_align(&[1]);
        for u in &no_adapter {
            t.push_row(vec![u.path.clone(), human_bytes(u.bytes)]);
        }
        println!();
        println!("  {}", Style::new().bold().apply_to("Not supported yet"));
        println!(
            "  {}",
            muted().apply_to("No adapter claims these folders, they just look like agent data — size only, nothing is managed.")
        );
        println!("{}", t.render());
    }
    if !undeclared.is_empty() {
        let mut t = Table::new(vec!["AGENT", "FOLDER", "SIZE"]);
        t.color_col(0, accent());
        t.color_col(1, muted());
        t.right_align(&[2]);
        // 按体积降序、只列前 UNDECLARED_ROWS 条:本机实测 125 条里 52 条不足 1 KB,
        // 逐条铺开只会把真正要看的那几百兆(`~/.codex/.tmp` 138 MB)挤出屏幕。
        // 尾巴压成一行汇总,`--json` 里仍是完整清单。
        let mut rows: Vec<_> = undeclared.iter().collect();
        rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        let shown = rows.len().min(UNDECLARED_ROWS);
        for u in &rows[..shown] {
            t.push_row(vec![
                u.agent.clone().unwrap_or_default(),
                u.path.clone(),
                human_bytes(u.bytes),
            ]);
        }
        println!();
        println!(
            "  {}",
            Style::new()
                .bold()
                .apply_to("Inside an agent, but not declared")
        );
        println!(
            "  {}",
            muted().apply_to("Real usage by that agent, but no manifest rule covers it — not in the SIZE column, and clean / prune leave it alone.")
        );
        println!("{}", t.render());
        if rows.len() > shown {
            let rest: u64 = rows[shown..].iter().map(|u| u.bytes).sum();
            println!(
                "  {}",
                muted().apply_to(format!(
                    "  … {} more, {} total (see --json for the full list)",
                    rows.len() - shown,
                    human_bytes(rest)
                ))
            );
        }
    }

    println!();
    let mark = if warnings.is_empty() {
        ok_mark().to_string()
    } else {
        warn_mark().to_string()
    };
    // 重解析数只在非零时露面:日常增量恒为 0,常驻一列纯占版面。
    let reparsed: usize = report.agents.iter().map(|a| a.sessions_indexed).sum();
    let mut parts = vec![
        style(format!("{installed} agents")).bold().to_string(),
        format!("{} on disk", style(human_bytes(report.total_bytes)).bold()),
    ];
    if reparsed > 0 {
        parts.push(format!("re-read {reparsed} conversations"));
    }
    parts.push(muted().apply_to(human_ms(report.duration_ms)).to_string());
    println!(
        "  {mark} {}",
        parts.join(&muted().apply_to(" · ").to_string())
    );
    render_footprint(&clean, install_bytes);

    if report.rules_changed {
        println!(
            "  {} {}",
            muted().apply_to("↳"),
            muted().apply_to(
                "Parser rules changed since the last scan, so every conversation was re-read."
            )
        );
    }
    if !warnings.is_empty() {
        eprintln!();
        for w in warnings {
            eprintln!("  {} {w}", warn_mark());
        }
        eprintln!(
            "  {}",
            muted().apply_to("Those items were skipped; everything else was indexed.")
        );
    }
}

/// 计数列的资源类:行数本身就有意义(有几个 MCP server、几份 skill)。
const COUNT_KINDS: [&str; 4] = ["mcp", "skill", "memory", "session"];

/// 「认领目录内未声明」表最多铺开几行。超出的压成一行汇总。
/// 本机实测这份清单有 125 条、其中 52 条不足 1 KB,全列出来等于没列。
const UNDECLARED_ROWS: usize = 15;

/// 体积列的资源类:条数不可行动,「占了 1.1 GB」才是决策依据。
/// 两列刻意分开——它们的可操作性完全相反:
/// `artifact` 是 clean + prune 的全部作用域,`install` 是两者都永不触碰的部分。
///
/// 列里给的是**占用量**;能拿回多少看摘要行,两者对 l0 并不相等。
const BYTE_KINDS: [(&str, &str); 2] = [("artifact", "CLEANABLE"), ("install", "INSTALLED")];

/// 每个 agent 的度量列表头,顺序即列顺序:先四类计数,再三个体积。
///
/// 三个体积列必须挨着,顺序 CLEANABLE → INSTALLED → SIZE 也是刻意的:
/// 前两个是这次决策要看的数(能清多少 / 碰不得多少),SIZE 是它们的上界,
/// 放在末尾当总计。夹在 AGENT 和计数列之间时,读者得跨半张表才能对上。
fn metric_headers() -> Vec<String> {
    let mut h: Vec<String> = COUNT_KINDS.iter().map(|k| k.to_uppercase()).collect();
    h.extend(BYTE_KINDS.iter().map(|(_, label)| (*label).to_string()));
    h.push("SIZE".into());
    h
}

/// 计数单元格:0 与缺失都渲染成占位横杠(Table 会把它置灰)。
fn count_cell(n: Option<&usize>) -> String {
    match n {
        Some(&n) if n > 0 => n.to_string(),
        _ => "-".to_string(),
    }
}

/// 体积单元格:0 与缺失同样退化成占位横杠。
fn bytes_cell(n: Option<&u64>) -> String {
    match n {
        Some(&n) if n > 0 => human_bytes(n),
        _ => "-".to_string(),
    }
}

/// 磁盘足迹小结:可回收一行、软件本体一行,scan 与 status 共用。
///
/// 只给两行是刻意的——把二十来个 artifact 目录逐行铺开没人看得完,
/// 但「总共能清多少、其中多少是无损的」必须一眼可见,所以总量与
/// 分级明细压在同一行里。
///
/// `clean` 里装的是**可回收量**(l0 只算 SQLite 空洞),不是占用量。
fn render_footprint(clean: &BTreeMap<String, u64>, install_bytes: u64) {
    let total: u64 = clean.values().sum();
    if total > 0 {
        // 档位与标签都来自 CleanLevel,CLI 不另抄一份字符串表。
        // 库里冒出未知级别(旧库、手改)时明细会为空,此时只报总量——
        // 宁可少说一句,不能吐出个空破折号。
        let detail: Vec<String> = CleanLevel::ORDER
            .iter()
            .filter_map(|level| {
                let b = clean.get(level.as_str()).copied().unwrap_or(0);
                (b > 0).then(|| format!("{} {}", human_bytes(b), level.label()))
            })
            .collect();
        let tail = if detail.is_empty() {
            "reclaimable".to_string()
        } else {
            format!("reclaimable — {}", detail.join(" · "))
        };
        println!(
            "  {} {} {}",
            muted().apply_to("↳"),
            style(human_bytes(total)).yellow().bold(),
            muted().apply_to(tail)
        );
    }
    if install_bytes > 0 {
        println!(
            "  {} {} {}",
            muted().apply_to("↳"),
            style(human_bytes(install_bytes)).bold(),
            muted().apply_to("installed software — clean and prune never touch it")
        );
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn cmd_status(mode: OutputMode, index: Option<&Path>) -> i32 {
    let report = match status(index) {
        Ok(r) => r,
        Err(e) => return fail(mode, "status", &e),
    };
    match mode {
        OutputMode::Json => emit_json("status", &report, &[]),
        OutputMode::Human => render_status_human(&report),
    }
    EXIT_OK
}

fn render_status_human(report: &StatusReport) {
    let mut headers: Vec<String> = vec!["AGENT".into()];
    headers.extend(metric_headers());
    headers.push("LAST SCAN".into());
    let last_col = headers.len() - 1;

    let mut table = Table::new(headers);
    table.color_col(0, accent());
    table.color_col(last_col, muted());
    table.right_align(&(1..last_col).collect::<Vec<_>>());
    let mut clean: BTreeMap<String, u64> = BTreeMap::new();
    let mut install_bytes = 0u64;
    for a in &report.agents {
        for (level, bytes) in &a.clean_bytes {
            *clean.entry(level.clone()).or_insert(0) += bytes;
        }
        install_bytes += a.kind_bytes.get("install").copied().unwrap_or(0);
        let mut row = vec![a.agent_id.clone()];
        row.extend(
            COUNT_KINDS
                .iter()
                .map(|k| count_cell_u64(a.kind_counts.get(*k))),
        );
        row.extend(
            BYTE_KINDS
                .iter()
                .map(|(k, _)| bytes_cell(a.kind_bytes.get(*k))),
        );
        row.push(human_bytes(a.bytes));
        row.push(relative_time(a.last_scan_ms));
        table.push_row(row);
    }

    println!();
    if table.is_empty() {
        println!("  {}", muted().apply_to("Nothing indexed yet."));
        println!(
            "  {} {}",
            muted().apply_to("Run"),
            Style::new().green().bold().apply_to("duster scan")
        );
        return;
    }
    println!("{}", table.render());
    println!();
    println!(
        "  {} {} {}",
        muted().apply_to("Total"),
        style(human_bytes(report.total_bytes)).bold(),
        muted().apply_to(format!("across {} agents", report.agents.len()))
    );
    render_footprint(&clean, install_bytes);
}

/// [`count_cell`] 的 u64 版本(status 的计数是 u64)。
fn count_cell_u64(n: Option<&u64>) -> String {
    match n {
        Some(&n) if n > 0 => n.to_string(),
        _ => "-".to_string(),
    }
}

/// Unix 毫秒 → 相对时间人话,如「3 minutes ago」;None → 「never」。
fn relative_time(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "never".to_string();
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let delta_s = (now_ms - ms).max(0) / 1000;
    // 走 plural():以前是硬拼 `{n} days ago`,于是每张表里都能读到
    // 「1 days ago」「1 hours ago」——这一列在 status / session list /
    // 轮次标题里到处都是,一个错字被复印了十几遍。
    match delta_s {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} ago", plural((delta_s / 60) as usize, "minute")),
        3600..86400 => format!("{} ago", plural((delta_s / 3600) as usize, "hour")),
        _ => format!("{} ago", plural((delta_s / 86400) as usize, "day")),
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

fn cmd_search(
    mode: OutputMode,
    index: Option<&Path>,
    query: &str,
    agents: Vec<String>,
    limit: usize,
) -> i32 {
    let filter = SearchFilter { agents, limit };
    let hits = match search(index, query, &filter) {
        Ok(h) => h,
        Err(e) => return fail(mode, "search", &e),
    };
    match mode {
        OutputMode::Json => {
            // SearchHit 定义在无 serde 依赖的 duster-index 里,不带 Serialize;
            // 在外壳手工映射成 JSON 数组,字段名与结构体保持一致。
            let data: Vec<serde_json::Value> = hits
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "tid": h.tid,
                        "rid": h.rid,
                        "agent_id": h.agent_id,
                        "resource_path": h.resource_path,
                        "seq": h.seq,
                        "role": h.role,
                        "byte_off": h.byte_off,
                        "byte_len": h.byte_len,
                        "snippet": h.snippet,
                        "highlights": h.highlights,
                    })
                })
                .collect();
            emit_json("search", &data, &[]);
        }
        OutputMode::Human => render_search_human(&hits, query),
    }
    EXIT_OK
}

fn render_search_human(hits: &[SearchHit], query: &str) {
    if hits.is_empty() {
        eprintln!();
        eprintln!("  {} No matches for {}", warn_mark(), style(query).bold());
        eprintln!(
            "  {}",
            muted().apply_to(
                "Search needs 3 characters or more. If the index is stale, run `duster scan`."
            )
        );
        return;
    }
    let color = use_color();
    let dot = muted().apply_to("·").to_string();
    println!();
    println!(
        "  {}",
        muted().apply_to(format!(
            "{} {} for \"{query}\"",
            hits.len(),
            if hits.len() == 1 { "match" } else { "matches" }
        ))
    );
    for h in hits {
        let file = Path::new(&h.resource_path).file_name().map_or_else(
            || h.resource_path.clone(),
            |f| f.to_string_lossy().into_owned(),
        );
        println!(
            "  {}  {} {dot} {} {dot} {} {dot} {}",
            style(format!("#{}", h.tid)).yellow().bold(),
            accent().apply_to(&h.agent_id),
            // 会话文件名可以长到七十列(codex 的 rollout-<时间>-<uuid>),
            // 截断保住一行一条;定位靠 tid,文件名只是上下文。
            muted().apply_to(truncate_width(&file, 32)),
            muted().apply_to(format!("turn {}", h.seq)),
            style(&h.role).magenta()
        );
        println!("      {}", highlight(&h.snippet, &h.highlights, color));
    }
    println!();
    println!(
        "  {}",
        muted().apply_to("Read one in full: duster open <id>")
    );
}

/// 是否给命中着色。console 已内建 tty + `NO_COLOR` / `CLICOLOR_FORCE` 判定,
/// 这里只取它的结论,免得手搓的规则和其余输出对不上。
fn use_color() -> bool {
    console::colors_enabled()
}

/// 把 highlights 字节区间包上 ANSI 粗体红(`\x1b[1;31m..\x1b[0m`)。
/// 区间由检索层保证落在 char 边界且按序不重叠;不着色时原样返回。
fn highlight(snippet: &str, spans: &[(usize, usize)], color: bool) -> String {
    if !color || spans.is_empty() {
        return snippet.to_string();
    }
    let mut out = String::with_capacity(snippet.len() + spans.len() * 12);
    let mut cursor = 0usize;
    for &(start, end) in spans {
        if start < cursor || end > snippet.len() || start > end {
            continue; // 防御:非法区间直接跳过,不 panic。
        }
        out.push_str(&snippet[cursor..start]);
        out.push_str("\x1b[1;31m");
        out.push_str(&snippet[start..end]);
        out.push_str("\x1b[0m");
        cursor = end;
    }
    out.push_str(&snippet[cursor..]);
    out
}

// ---------------------------------------------------------------------------
// open
// ---------------------------------------------------------------------------

/// open 的落点:一个数字要么是 `duster search` 印的 turn id,要么是
/// `duster session list` 印的会话 id——两档数字长得一模一样,不试到
/// 第二个空间根本分不出来。
#[derive(Debug)]
enum OpenTarget {
    Turn(TurnDetail),
    Session(SessionDetail),
}

/// 把 id 解析成轮次或整场会话。先按 turn 查,查无再按会话查;
/// 两个都落空才报错,而且报错必须同时点名两个命令,让用户知道
/// 自己用的是哪一档数字、该去哪一档找。
fn resolve_open(index: Option<&Path>, id: i64) -> anyhow::Result<OpenTarget> {
    match open_turn(index, id) {
        Ok(d) => Ok(OpenTarget::Turn(d)),
        // 只有「查无此项」才值得试会话 id:turn 存在但源文件读不动是
        // 另一回事,拿同一个 id 去 show 只会把错误再包一层。
        // 判据只能匹配 Display 的稳定文案(open_turn 没有错误枚举,
        // 与 REFUSAL_PHRASES 同一套跨层字符串契约,两侧各有测试守着)。
        Err(e) if is_missing(&e) => match duster_core::session::show(index, id) {
            Ok(d) => Ok(OpenTarget::Session(d)),
            Err(se) if is_missing(&se) => anyhow::bail!(
                "id {id} matches neither a turn id nor a conversation id. \
                 `duster search` prints turn ids (#42); `duster session list` \
                 prints conversation ids. Use one of those to find a valid id."
            ),
            Err(se) => Err(se),
        },
        Err(e) => Err(e),
    }
}

/// 「查无此项」判据:open_turn 与 session::show 各自的缺省文案都含这句。
fn is_missing(err: &anyhow::Error) -> bool {
    err.to_string().contains("does not exist")
}

fn cmd_open(mode: OutputMode, index: Option<&Path>, id: i64, full: bool) -> i32 {
    let target = match resolve_open(index, id) {
        Ok(t) => t,
        Err(e) => return fail(mode, "open", &e),
    };
    match target {
        OpenTarget::Turn(detail) => match mode {
            OutputMode::Json => emit_json("open", &detail, &[]),
            OutputMode::Human => {
                // 元信息一行 + 路径一行:open 的正文来自哪场会话,这是它与
                // session show 不同的唯一信息(print_turn_body 不印它)。
                let dot = muted().apply_to("·").to_string();
                println!();
                println!(
                    "  {} {dot} {}",
                    accent().bold().apply_to(&detail.agent_id),
                    format_args!("turn {}", detail.seq)
                );
                println!("  {}", muted().apply_to(&detail.resource_path));
                let rule = display_width(&detail.resource_path).clamp(16, 72);
                println!("  {}", muted().apply_to("─".repeat(rule)));
                println!();
                // 头是调用方的活(print_turn_body 没有时间戳参数,只印正文);
                // 工具轮折叠成一行时它连头一起印,这里就不该再打一个 ##。
                if detail.role != "tool" || full {
                    println!("## {}", style(&detail.role).magenta());
                }
                cmd::session::print_turn_body(
                    &detail.role,
                    detail.tool.as_deref(),
                    &detail.text,
                    detail.raw_fallback,
                    full,
                );
            }
        },
        OpenTarget::Session(detail) => {
            // 数字空间撞车是必然会发生的事:不点破,用户下次还会拿
            // session list 的 id 来 open,再吃一次同样的错。
            let notice = format!(
                "id {id} is a conversation id — showing the conversation \
                 (turn ids come from `duster search`)"
            );
            match mode {
                OutputMode::Json => emit_json("open", &detail, &[notice]),
                OutputMode::Human => {
                    println!("  {}", muted().apply_to(notice));
                    cmd::session::render_show(&detail, full);
                }
            }
        }
    }
    EXIT_OK
}

// ---------------------------------------------------------------------------
// 清理三动词的公共部件
// ---------------------------------------------------------------------------

/// 同意来源。三个动词一律默认 [`Consent::Preview`]:预览是默认,执行是显式行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consent {
    /// 只出计划,不问也不动(默认;`--dry-run` 同此)。
    Preview,
    /// 已授权:`--yes`(clean / prune)或逐字输入的 agent id(uninstall)。
    Granted,
    /// 清单打完当场问,条目多时按类别分组问。只有交互菜单走这条。
    Ask,
}

/// 旗标 → 同意来源。`--dry-run` 压过 `--yes`:两个都给时用户要的是预览。
fn consent_of(yes: bool, dry_run: bool) -> Consent {
    if yes && !dry_run {
        Consent::Granted
    } else {
        Consent::Preview
    }
}

/// `--json` 下不存在"当场问"这回事——机器可读的输出里没有地方放"用户读过清单"
/// 这件事,所以 Ask 一律退化成只出计划,收尾把原因写进信封 warnings,退出码 4。
fn settle(mode: OutputMode, consent: Consent) -> Consent {
    if mode == OutputMode::Json && consent == Consent::Ask {
        Consent::Preview
    } else {
        consent
    }
}

/// `--archive` / `--no-archive` 收敛成三态:两个都没给 = 未表态(None)。
/// 未表态且归档预估超阈值时由 core 拒绝执行——外壳不替用户猜。
fn archive_choice(archive: bool, no_archive: bool) -> Option<bool> {
    match (archive, no_archive) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

/// 合并计划与报告的 warnings。两处可能带同一条(core 会把计划 warning 抄进报告),
/// 原样输出等于让用户读两遍;按内容去重且保序。
fn merge_warnings(plan: &[String], report: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(plan.len() + report.len());
    for w in plan.iter().chain(report) {
        if !out.iter().any(|seen| seen == w) {
            out.push(w.clone());
        }
    }
    out
}

/// 执行结果 → 退出码:什么都没动 = 4(脚本据此区分"没做"与"做完了");
/// 动了但有子项失败 = 3;全成 = 0。
fn exec_exit_code(executed: bool, failures: usize) -> i32 {
    if !executed {
        EXIT_CONFIRM_DENIED
    } else if failures > 0 {
        EXIT_PARTIAL
    } else {
        EXIT_OK
    }
}

/// 未执行时的收尾结论。必须把"怎么才算动"原样给出来——`--dry-run` 报告的
/// 用法是重定向成文件、读完再回来敲一条命令,那条命令不能靠用户回忆。
fn not_done_hint(declined: bool, next: &str) -> String {
    if declined {
        format!("nothing was done — you said no. Run `{next}` when you want it to happen")
    } else {
        format!("nothing was done — this was a preview; run `{next}` to go ahead")
    }
}

/// warnings 落 stderr(stdout 只放结果),与 scan 的收尾一致。
pub(crate) fn render_warnings(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    eprintln!();
    for w in warnings {
        eprintln!("  {} {w}", warn_mark());
    }
}

/// 人类模式的收尾:结论落 stdout(它是报告的最后一行,重定向成文件后仍在),
/// warnings 落 stderr。`--json` 的两者都进信封,由调用方 emit,不走这里。
fn finish_human(warnings: &[String], hint: Option<&str>) {
    if let Some(h) = hint {
        println!();
        println!("  {} {}", warn_mark(), muted().apply_to(h));
    }
    render_warnings(warnings);
}

// ---------------------------------------------------------------------------
// 计划清单渲染:确认的唯一防线
// ---------------------------------------------------------------------------
//
// 没有回收站、没有 undo,全部安全性压在这一次打印上。所以每条都必须可评估:
// 是什么 / 为何可清(陈旧判定带最后使用时间与依据来源)/ 清后影响 /
// 归档包里有没有。**禁止只甩路径列表。**
//
// 为什么不做成表格:四段说明每段都可能是一整句话,挤进单元格就得截断,
// 而被截掉的恰好是「清后影响」。所以一条 = 一行定位 + 三行说明。

/// `install` 行的标记。这一列存在的意义就是让用户看见那几个 GB 为什么不动。
const NOT_CLEANABLE: &str = "not cleanable";

/// 计数 + 名词的英文复数。"1 items" 这种小破绽会让人连带怀疑其余数字。
pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// 动词名,进标题与 JSON 无关(信封的 command 由调用方给)。
fn verb_name(verb: Verb) -> &'static str {
    match verb {
        Verb::Clean => "clean",
        Verb::Prune => "prune",
        Verb::Uninstall => "uninstall",
    }
}

/// 动作的人话短语。用户判断风险靠这一栏:"empty file" 与 "delete folder"
/// 是两种完全不同的后果,统一叫 "remove" 等于把区别藏起来。
fn action_label(action: Action) -> &'static str {
    match action {
        Action::Vacuum => "compact in place",
        Action::RemoveSidecar => "delete leftover",
        Action::RemoveFile => "delete file",
        Action::RemoveDir => "delete folder",
        Action::TruncateFile => "empty file",
        Action::CompressFile => "compress",
        Action::Keep => "keep",
    }
}

/// 执行结果里的 action 字符串(core 给的是 [`Action`] 的 serde 名,如 `remove_dir`)
/// → 计划里用的同一句人话。一次操作在同一屏里不该有两个名字。
/// 认不出的原样透出:core 加了新动作时宁可显得生硬,也不要猜错。
fn action_phrase(raw: &str) -> String {
    match raw {
        "vacuum" => action_label(Action::Vacuum),
        "remove_sidecar" => action_label(Action::RemoveSidecar),
        "remove_file" => action_label(Action::RemoveFile),
        "remove_dir" => action_label(Action::RemoveDir),
        "truncate_file" => action_label(Action::TruncateFile),
        "compress_file" => action_label(Action::CompressFile),
        "keep" => action_label(Action::Keep),
        other => other,
    }
    .to_string()
}

/// 归档标记:每条都要说清"这份东西在归档包里有没有"——没有回收站之后,
/// 归档包是唯一的还原来源。`install` 行改标 not cleanable。
fn archive_mark(item: &PlanItem) -> String {
    if !item.cleanable {
        return style(NOT_CLEANABLE).yellow().to_string();
    }
    match (item.archived, item.action) {
        (true, _) => muted().apply_to("in the archive").to_string(),
        // 代际保留项是「最新 N 份之一」,不是唯一的"最后一份"。
        (false, Action::Keep) if item.generation => {
            muted().apply_to("kept as a fallback").to_string()
        }
        (false, Action::Keep) => muted().apply_to("kept as the last copy").to_string(),
        (false, _) => style("no archive copy").yellow().to_string(),
    }
}

/// 类别标签:`kind` + `clean_level`。分组渲染与逐组确认都按它切。
///
/// 超编代际单独一组:它和按龄清理的 l2 项在同一个计划里出现,读者必须
/// 一眼看出「这 8 项为什么在这」和「那 2 项为什么留下」是两种理由,
/// 混在同一组里,16 天前的文件就会被读成「因为 90d 才删」。
fn category_label(item: &PlanItem) -> String {
    if item.generation && item.action != Action::Keep {
        return match item.clean_level {
            Some(level) => format!("{} {} surplus generations", item.kind, level.as_str()),
            None => format!("{} surplus generations", item.kind),
        };
    }
    match item.clean_level {
        Some(level) => format!("{} {}", item.kind, level.as_str()),
        None => item.kind.clone(),
    }
}

/// 按类别分组。顺序固定:可清理的在前(按类别名排),软件本体永远最后一组。
/// 固定顺序是为了 `--dry-run` 报告重定向成文件后能直接 diff 两次运行。
fn group_plan<'a>(items: impl Iterator<Item = &'a PlanItem>) -> Vec<(String, Vec<&'a PlanItem>)> {
    let mut groups: BTreeMap<(bool, String), Vec<&PlanItem>> = BTreeMap::new();
    for item in items {
        groups
            .entry((!item.cleanable, category_label(item)))
            .or_default()
            .push(item);
    }
    groups
        .into_iter()
        .map(|((_, label), v)| (label, v))
        .collect()
}

/// 一条计划项:定位行 + 三段说明。
///
/// 路径**不截断**:报告要能重定向成文件、路径要能原样复制,截掉的正是
/// 用户要核对的那一截。宽度让终端自己折,不做分页也不做换行。
fn render_plan_item(item: &PlanItem) {
    let dot = muted().apply_to(" · ").to_string();
    let mut head = vec![
        accent()
            .apply_to(item.path.display().to_string())
            .to_string(),
    ];
    // `install` 行的 bytes 按契约恒为 0(它不进任何合计)。照直印成 "0 B" 会被
    // 读成"这个目录是空的",而它其实有几百 MB —— 体积在 what 那句话里,
    // 合计在「软件本体」那一行,这里干脆不印数字。
    if item.cleanable || item.bytes > 0 {
        head.push(human_bytes(item.bytes));
    }
    head.push(action_label(item.action).to_string());
    head.push(archive_mark(item));
    println!("    {}", head.join(dot.as_str()));
    for (label, text) in [
        ("what", &item.what),
        ("why", &item.why),
        ("impact", &item.impact),
    ] {
        // 空字符串照实报成 (not stated):这三段是计划层的硬要求,
        // 缺了要看得见,不该由外壳替它圆过去。
        let text = if text.is_empty() {
            "(not stated)"
        } else {
            text.as_str()
        };
        println!("      {} {text}", muted().apply_to(format!("{label:<6}")));
    }
}

/// 计划清单:三个动词共用。
///
/// `show_install_total` = false 时省掉「软件本体合计」那一行——clean 的收尾
/// 三桶报告会连同去处一起报,同一屏里说两遍会被当成两笔账。
fn render_plan(plan: &Plan, show_install_total: bool) {
    println!();
    println!(
        "  {} {}",
        accent()
            .bold()
            .apply_to(format!("{} plan", verb_name(plan.verb))),
        muted().apply_to(plural(plan.items.len(), "item"))
    );
    if plan.items.is_empty() {
        println!(
            "  {}",
            muted().apply_to("Nothing to do. If that looks wrong, run `duster scan` first.")
        );
        return;
    }

    for (label, items) in group_plan(plan.items.iter()) {
        let bytes: u64 = items.iter().map(|i| i.bytes).sum();
        let cleanable = items.first().is_some_and(|i| i.cleanable);
        let mut head = format!("{label} · {}", plural(items.len(), "item"));
        // 同上:not cleanable 的组体积恒为 0,印出来只会误导。
        if cleanable || bytes > 0 {
            head.push_str(&format!(" · {}", human_bytes(bytes)));
        }
        println!();
        if cleanable {
            println!("  {}", Style::new().bold().apply_to(head));
        } else {
            println!(
                "  {} {}",
                Style::new().bold().apply_to(head),
                style(NOT_CLEANABLE).yellow()
            );
        }
        for item in items {
            render_plan_item(item);
        }
    }

    println!();
    println!(
        "  {} {} {}",
        muted().apply_to("Total"),
        style(human_bytes(plan.reclaim_bytes)).yellow().bold(),
        muted().apply_to(format!(
            "reclaimable · {} to change",
            plural(plan.actionable().count(), "item")
        ))
    );
    // 软件本体既不在计划的合计里,也永远不会被动——但必须看得见。
    if show_install_total && plan.install_bytes > 0 {
        println!(
            "  {} {} {}",
            muted().apply_to("↳"),
            style(human_bytes(plan.install_bytes)).bold(),
            muted().apply_to("installed software — not cleanable, not counted above")
        );
    }
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

fn cmd_clean(mode: OutputMode, index: Option<&Path>, agents: Vec<String>, consent: Consent) -> i32 {
    let consent = settle(mode, consent);
    let build = |execute: bool| CleanOptions {
        index_path: index.map(Path::to_path_buf),
        home: None,
        agents: agents.clone(),
        dry_run: !execute,
        // `yes` 只表示"问句免了",动不动由 dry_run 说——两者同源,不会背离。
        yes: execute,
    };
    let mut report = match clean(&build(consent == Consent::Granted)) {
        Ok(r) => r,
        Err(e) => return fail(mode, "clean", &e),
    };

    // 交互模式(Ask)的清单由勾选列表自己印,不再打全量 what / why / impact
    // 转储;命令行路径(`--yes` / `--dry-run`)照旧要能重定向成文件逐条核对。
    // `--yes` 跳过的是问句,从来不是清单。
    if mode == OutputMode::Human && consent != Consent::Ask {
        render_plan(&report.plan, false);
    }
    let mut declined = false;
    // 计划本身就空:用户没被问过任何问题,收尾不许说「你说了不」,
    // 退出码也该与命令行 `--yes` 撞上空计划时一致(0)。
    let mut nothing = false;
    if consent == Consent::Ask {
        match interactive::approve_clean_plan(&report.plan) {
            // 用户勾过的才动:白名单,而不是「没被跳过的都动」——出计划与
            // 执行之间隔着读清单的时间,这期间新冒出来的项用户从没见过。
            interactive::PlanApproval::Run(allow) => {
                match clean_filtered(&build(true), &PlanFilter::allow_only(allow)) {
                    Ok(r) => report = r,
                    Err(e) => return fail(mode, "clean", &e),
                }
            }
            interactive::PlanApproval::No => declined = true,
            interactive::PlanApproval::Nothing => nothing = true,
            interactive::PlanApproval::Aborted => return EXIT_ERROR,
        }
    }

    let failures = report.outcomes.iter().filter(|o| o.error.is_some()).count();
    let mut warnings = merge_warnings(&report.plan.warnings, &report.warnings);
    // 空计划那一屏已经自己印过 "Nothing to do…",再补一句只是噪音。
    let hint = (!report.executed && !nothing).then(|| not_done_hint(declined, "duster clean --yes"));
    match mode {
        OutputMode::Json => {
            warnings.extend(hint);
            emit_json("clean", &report, &warnings);
        }
        OutputMode::Human => {
            render_clean_outcomes(&report.outcomes);
            render_buckets(
                &report.buckets,
                report.executed,
                report.plan.reclaim_bytes,
                &agents,
            );
            finish_human(&warnings, hint.as_deref());
        }
    }
    // 无事可做 = 成功地什么都不用做,与命令行 `--yes` 撞上空计划同码。
    if nothing {
        return EXIT_OK;
    }
    exec_exit_code(report.executed, failures)
}

/// clean 的逐项对账单。这里可以截断路径:计划已经原样列过一遍,
/// 这张表回答的是"刚才发生了什么",列宽比完整路径重要。
fn render_clean_outcomes(outcomes: &[CleanOutcome]) {
    if outcomes.is_empty() {
        return;
    }
    let mut t = Table::new(vec!["PATH", "ACTION", "BEFORE", "FREED", "NOTE"]);
    t.color_col(0, accent());
    t.right_align(&[2, 3]);
    t.color_col(4, muted());
    for o in outcomes {
        t.push_row(vec![
            truncate_width(&o.path, 48),
            action_phrase(&o.action),
            human_bytes(o.before),
            human_bytes(o.freed),
            // 失败原因这里只留一截,全文进 warnings(core 已经放进去了)。
            o.error
                .as_deref()
                .map_or_else(|| "-".to_string(), |e| truncate_width(e, 36)),
        ]);
    }
    println!();
    println!("{}", t.render());
}

/// clean 的收尾三桶报告。三个数字必须同时出现,且各自带去处——只报「已回收 X」
/// 会让用户断定 duster 的上限就是这点,而另外两桶(陈旧资源、软件本体)常常
/// 大一个数量级,出口是另外两个命令。
///
/// dry-run 时第一桶报的是**预估**并明说没动过:把预估说成"已回收"正是这个
/// 里程碑要消灭的那句谎话。
fn render_buckets(buckets: &Buckets, executed: bool, would_reclaim: u64, agents: &[String]) {
    // 卸载提示只在一个 agent 时给具体命令：`duster uninstall` 收单个位置参数，
    // 多选或全选时给占位符，不然这行提示会暗示一条跑不起来的命令。
    let target = match agents {
        [a] => a.as_str(),
        _ => "<agent>",
    };
    println!();
    if executed {
        println!(
            "  {} {} {}",
            ok_mark(),
            style(human_bytes(buckets.reclaimed)).green().bold(),
            muted().apply_to("reclaimed — actually freed on disk")
        );
    } else {
        println!(
            "  {} {} {}",
            warn_mark(),
            style(human_bytes(would_reclaim)).yellow().bold(),
            muted().apply_to("would be reclaimed — nothing has been touched yet")
        );
    }
    println!(
        "  {} {} {}",
        muted().apply_to("↳"),
        style(human_bytes(buckets.stale)).bold(),
        muted().apply_to("stale — duster prune --older-than 30d")
    );
    println!(
        "  {} {} {}",
        muted().apply_to("↳"),
        style(human_bytes(buckets.install)).bold(),
        muted().apply_to(format!("installed software — duster uninstall {target}"))
    );
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

fn cmd_prune(
    mode: OutputMode,
    index: Option<&Path>,
    agents: Vec<String>,
    older_than: Option<String>,
    archive: Option<bool>,
    consent: Consent,
    keep_generations: bool,
) -> i32 {
    // prune 默认关闭:没有 `--older-than` 就没有"陈旧"的定义,外壳绝不替用户挑
    // 一个阈值——挑错了删的是不可再生的东西。
    let days = match older_than {
        Some(ref s) => match parse_older_than(s) {
            Ok(d) => d,
            Err(e) => return usage(mode, "prune", &format!("--older-than {s}: {e:#}")),
        },
        None => {
            return usage(
                mode,
                "prune",
                "prune needs --older-than, e.g. --older-than 30d (30d / 60d / 90d)",
            );
        }
    };
    let consent = settle(mode, consent);
    let build = |execute: bool| PruneOptions {
        index_path: index.map(Path::to_path_buf),
        home: None,
        agents: agents.clone(),
        older_than_days: days,
        keep_generations,
        archive,
        export_dir: None,
        dry_run: !execute,
        yes: execute,
        // core 的同一道门槛:`--json` 无 `--yes` 一律拒绝执行。外壳这边
        // consent 已经保证了没 `--yes` 就只出计划,两处不冲突。
        json: mode == OutputMode::Json,
        now_ms: None,
    };
    let mut report = match prune(&build(consent == Consent::Granted)) {
        Ok(r) => r,
        Err(e) => return fail(mode, "prune", &e),
    };

    // 交互模式(Ask)的清单由勾选列表自己印,不再打全量 what/why 转储;
    // 命令行路径(--yes / --dry-run)照旧要能重定向成文件逐条核对。
    if mode == OutputMode::Human && consent != Consent::Ask {
        render_plan(&report.plan, true);
    }
    let mut declined = false;
    // 空计划:用户没被问过任何问题,收尾不许说「你说了不」。理由同 `cmd_clean`。
    let mut nothing = false;
    if consent == Consent::Ask {
        match interactive::approve_prune_plan(&report.plan) {
            // 用户勾过的才动:白名单,而不是「没被跳过的都动」。
            interactive::PlanApproval::Run(allow) => {
                match prune_filtered(&build(true), &PlanFilter::allow_only(allow)) {
                    Ok(r) => report = r,
                    Err(e) => return fail(mode, "prune", &e),
                }
            }
            interactive::PlanApproval::No => declined = true,
            interactive::PlanApproval::Nothing => nothing = true,
            interactive::PlanApproval::Aborted => return EXIT_ERROR,
        }
    }

    let failures = report.outcomes.iter().filter(|o| o.error.is_some()).count();
    let mut warnings = merge_warnings(&report.plan.warnings, &report.warnings);
    let next = format!("duster prune --older-than {days}d --yes");
    let hint = (!report.executed && !nothing).then(|| not_done_hint(declined, &next));
    match mode {
        OutputMode::Json => {
            warnings.extend(hint);
            emit_json("prune", &report, &warnings);
        }
        OutputMode::Human => {
            render_archive(report.archive_path.as_deref(), report.archive_bytes);
            render_prune_outcomes(&report.outcomes);
            if report.executed {
                // 压缩存档那句话只在真压缩过时说:没压缩还提"读回来还是原样",
                // 读者会去找那个不存在的压缩包。
                let compressed = report.outcomes.iter().any(|o| o.replaced_by.is_some());
                println!();
                println!(
                    "  {} {} {}",
                    ok_mark(),
                    style(human_bytes(report.freed_bytes)).green().bold(),
                    muted().apply_to(if compressed {
                        "freed — the compressed conversations still read back as before"
                    } else {
                        "freed"
                    })
                );
            }
            finish_human(&warnings, hint.as_deref());
        }
    }
    if nothing {
        return EXIT_OK;
    }
    exec_exit_code(report.executed, failures)
}

/// prune 的逐项对账单。多一列 NOW:压缩存档换了文件名,不报出来用户会以为
/// 会话被删了。
fn render_prune_outcomes(outcomes: &[PruneOutcome]) {
    if outcomes.is_empty() {
        return;
    }
    let mut t = Table::new(vec!["PATH", "ACTION", "BEFORE", "FREED", "NOW", "NOTE"]);
    t.color_col(0, accent());
    t.right_align(&[2, 3]);
    t.color_col(5, muted());
    for o in outcomes {
        let now = o.replaced_by.as_deref().map_or_else(
            || "-".to_string(),
            |p| {
                Path::new(p)
                    .file_name()
                    .map_or_else(|| p.to_string(), |f| f.to_string_lossy().into_owned())
            },
        );
        t.push_row(vec![
            truncate_width(&o.path, 44),
            action_phrase(&o.action),
            human_bytes(o.before),
            human_bytes(o.freed),
            now,
            o.error
                .as_deref()
                .map_or_else(|| "-".to_string(), |e| truncate_width(e, 32)),
        ]);
    }
    println!();
    println!("{}", t.render());
}

/// 归档回执:路径与实际尺寸必须出现在输出里。用户要能立刻 `tar -tf` 核对,
/// 也要自己决定什么时候删掉这个包——duster 不做 GC,包归用户所有。
fn render_archive(path: Option<&str>, bytes: Option<u64>) {
    let Some(path) = path else {
        return;
    };
    println!();
    println!(
        "  {} {} {}",
        ok_mark(),
        muted().apply_to("archived to"),
        accent().apply_to(path)
    );
    if let Some(b) = bytes {
        println!(
            "    {} {}",
            muted().apply_to("size  "),
            style(human_bytes(b)).bold()
        );
    }
    println!(
        "    {}",
        muted().apply_to("restore with `tar -xf` — duster never deletes this for you")
    );
}

// ---------------------------------------------------------------------------
// uninstall
// ---------------------------------------------------------------------------

/// `duster uninstall` 的参数袋。凑成一个结构体不是为了好看:参数已经超过
/// clippy 的容忍线,而 clap 与交互菜单必须走同一条入口。
struct UninstallArgs {
    agent: String,
    data_only: bool,
    confirm: Option<String>,
    export_first: bool,
    keep: Vec<String>,
    archive: Option<bool>,
    run_package_manager: bool,
}

fn cmd_uninstall(
    mode: OutputMode,
    index: Option<&Path>,
    args: UninstallArgs,
    consent: Consent,
) -> i32 {
    let consent = settle(mode, consent);
    // 逐字确认串不相等即降级为预览。这里没有 `--yes` 可用:确认串是唯一的
    // 授权凭据(它删的是用户全部历史会话与记忆),不相等就只能看报告。
    let consent = match consent {
        Consent::Granted if args.confirm.as_deref() != Some(args.agent.as_str()) => {
            Consent::Preview
        }
        other => other,
    };
    let build = |execute: bool, confirm: Option<String>| UninstallOptions {
        index_path: index.map(Path::to_path_buf),
        home: None,
        agent: args.agent.clone(),
        data_only: args.data_only,
        confirm,
        export_first: args.export_first,
        keep: args.keep.clone(),
        archive: args.archive,
        export_dir: None,
        dry_run: !execute,
        run_package_manager: args.run_package_manager,
    };
    // 预览路径把确认串补齐成 agent id:`dry_run = true` 已经保证一个字节都不动,
    // 而 core 的确认校验在生成计划之前——不补齐的话「先读报告、再回来逐字确认」
    // 这条主路径根本走不到报告。真正的授权只有一处:执行路径把用户亲手输入的
    // 那个串原样交给 core,由它再校验一次。
    let preview = || build(false, Some(args.agent.clone()));
    let mut report = match uninstall(&if consent == Consent::Granted {
        build(true, args.confirm.clone())
    } else {
        preview()
    }) {
        Ok(r) => r,
        Err(e) => return fail(mode, "uninstall", &e),
    };

    if mode == OutputMode::Human {
        render_plan(&report.plan, true);
        render_checks(&report.checks);
        // 菜单路径的清单必须在逐字确认**之前**摆全,包括"要动别人家哪一个键"
        // 和"软件当初是怎么装的"。旗标路径的这一份报告已经是最终结果了,
        // 留给收尾统一渲染,免得同样的两栏印两遍。
        if consent == Consent::Ask {
            render_shared(&report.shared);
            render_packages(&report.packages);
        }
    }
    let mut declined = false;
    if consent == Consent::Ask {
        match interactive::confirm_uninstall(&args.agent) {
            // 走到这里说明用户已经逐字输入过 agent id,原样转交 core 再校验一次。
            interactive::Approval::Yes => match uninstall(&build(true, Some(args.agent.clone()))) {
                Ok(r) => report = r,
                Err(e) => return fail(mode, "uninstall", &e),
            },
            interactive::Approval::No => declined = true,
            interactive::Approval::Aborted => return EXIT_ERROR,
        }
    }

    // 残留即"卸载不干净",算部分成功而不是成功——这是这个动词的验收点。
    // 被拒的共享改键同理:文件一个字节没动,那个键还指着已经被删掉的程序。
    let failures = report.leftovers.len()
        + report
            .shared
            .iter()
            .filter(|s| s.action == SharedAction::Refused)
            .count();
    let mut warnings = merge_warnings(&report.plan.warnings, &report.warnings);
    let next = format!("duster uninstall {a} --confirm {a}", a = args.agent);
    let hint = (!report.executed).then(|| not_done_hint(declined, &next));
    match mode {
        OutputMode::Json => {
            warnings.extend(hint);
            emit_json("uninstall", &report, &warnings);
        }
        OutputMode::Human => {
            render_archive(report.archive_path.as_deref(), report.archive_bytes);
            // 明细在前、总结在后:那一行 ✔ 是这次卸载的最后一句话,
            // 它前面必须已经交代完"别人家的文件动了哪一处、包管理器怎么办"。
            render_shared(&report.shared);
            render_packages(&report.packages);
            render_leftovers(&report.leftovers);
            if report.executed {
                println!();
                println!(
                    "  {} {} {}",
                    ok_mark(),
                    style(human_bytes(report.removed_bytes)).green().bold(),
                    muted().apply_to(format!("removed · {} is gone", args.agent))
                );
            }
            finish_human(&warnings, hint.as_deref());
        }
    }
    exec_exit_code(report.executed, failures)
}

/// 前置检查:三项缺一不可。未通过时 detail 要说清卡在哪、怎么解,
/// 所以它按行铺开而不是塞进表格单元格。
fn render_checks(checks: &[PreflightCheck]) {
    if checks.is_empty() {
        return;
    }
    println!();
    println!("  {}", Style::new().bold().apply_to("Before removing"));
    let width = checks
        .iter()
        .map(|c| display_width(&c.name))
        .max()
        .unwrap_or(0);
    for c in checks {
        let mark = if c.passed {
            ok_mark().to_string()
        } else {
            output::err_mark().to_string()
        };
        let pad = " ".repeat(width - display_width(&c.name));
        println!(
            "    {mark} {}{pad}  {}",
            accent().apply_to(&c.name),
            muted().apply_to(&c.detail)
        );
    }
}

/// 共享文件改键:每一条都带上清单里那句理由。duster 要动的是**别人家的**
/// 文件,凭什么动必须在动之前写在用户眼前,而不是事后在 changelog 里解释。
fn render_shared(edits: &[SharedEditOutcome]) {
    if edits.is_empty() {
        return;
    }
    println!();
    println!(
        "  {}",
        Style::new()
            .bold()
            .apply_to("Its keys in files other agents own")
    );
    for e in edits {
        let (mark, verb) = match e.action {
            SharedAction::Removed => (ok_mark().to_string(), "removed"),
            SharedAction::Planned => (muted().apply_to("·").to_string(), "will be removed"),
            SharedAction::Absent => (muted().apply_to("·").to_string(), "already gone"),
            SharedAction::Missing => (muted().apply_to("·").to_string(), "no such file"),
            SharedAction::Refused => (warn_mark().to_string(), "refused"),
        };
        println!(
            "    {mark} {}  {}",
            accent().apply_to(&e.key),
            muted().apply_to(format!("{verb} · {}", truncate_width(&e.path, 44)))
        );
        println!("      {}", muted().apply_to(&e.reason));
        if let Some(d) = &e.detail {
            println!("      {}", muted().apply_to(d));
        }
    }
}

/// 包管理器一栏。**它的全部内容就是一行可以粘回终端的命令。**
/// duster 永不代跑——`--run-package-manager` 是用户自己按下的那个例外,
/// 而且命令照样先原样印出来再执行。
fn render_packages(pkgs: &[PackageOutcome]) {
    if pkgs.is_empty() {
        return;
    }
    println!();
    println!(
        "  {}",
        Style::new()
            .bold()
            .apply_to("How the software was installed")
    );
    for p in pkgs {
        println!(
            "    {}  {}",
            accent().apply_to(&p.manager),
            muted().apply_to(p.detected.as_str())
        );
        println!("      {}", style(&p.command).bold());
        if let Some(d) = &p.detail {
            println!("      {}", muted().apply_to(d));
        }
        for line in p.output.iter().flat_map(|o| o.lines()) {
            println!("      {}", muted().apply_to(line));
        }
    }
}

/// 删完仍在的路径。应为空;非空就是卸载不干净,必须报出来而不是当成功收场。
fn render_leftovers(leftovers: &[String]) {
    if leftovers.is_empty() {
        return;
    }
    println!();
    println!(
        "  {} {}",
        warn_mark(),
        style("still on disk after removing:").yellow()
    );
    for p in leftovers {
        println!("    {p}");
    }
}

// ---------------------------------------------------------------------------
// skill
// ---------------------------------------------------------------------------

fn cmd_skill_copies(mode: OutputMode, index: Option<&Path>) -> i32 {
    let groups = match copies(index) {
        Ok(g) => g,
        Err(e) => return fail(mode, "skill-copies", &e),
    };
    match mode {
        OutputMode::Json => emit_json("skill-copies", &groups, &[]),
        OutputMode::Human => println!("{}", render_skill_groups(&groups)),
    }
    EXIT_OK
}

/// DupState 的人话。
fn dup_state_label(state: DupState) -> &'static str {
    match state {
        DupState::Identical => "identical",
        DupState::Drifted => "drifted",
    }
}

/// skill 副本表 + 组内差异摘要 + 一行合计。整块返回而不是边算边印:
/// 这样「INSTALLED 列该不该出现」能被测试逐字核对。
fn render_skill_groups(groups: &[SkillGroup]) -> String {
    if groups.is_empty() {
        return format!(
            "\n  {}",
            muted().apply_to("Every skill lives in exactly one place — nothing to share.")
        );
    }

    // INSTALLED 列只在报告里确实有非零安装体积时才出现:全部为 0 的列
    // 只是竖着一排 `0 B`,既占掉 PATH 列急需的宽度,又邀请读者去读
    // 一行不存在的信息。它什么时候才可能非零?清单给 skill 声明了
    // `install_paths`(如 node_modules)且现场确实测得体积的时候——
    // 这台机器的清单没声明,列就整体消失。
    let show_installed = groups
        .iter()
        .any(|g| g.copies.iter().any(|c| c.install_bytes > 0));

    let mut headers = vec!["SKILL", "STATE", "AGENT", "CONTENT"];
    if show_installed {
        headers.push("INSTALLED");
    }
    headers.push("PATH");
    // PATH 恒为最后一列:flex_col 吃的是终端余量,只有尾巴才有余量可吃。
    let path_col = headers.len() - 1;

    let mut t = Table::new(headers);
    t.color_col(0, accent());
    // CONTENT 与 INSTALLED 是数字列;PATH 不右对齐(flex_col 之后是变宽列)。
    t.right_align(&[3]);
    if show_installed {
        t.right_align(&[4]);
    }
    t.color_col(path_col, muted());
    t.flex_col(path_col);

    for g in groups {
        for (i, c) in g.copies.iter().enumerate() {
            // 同一组只在首行写名字与状态,后续行留空——视觉上把一组连成一块。
            let (name, state) = if i == 0 {
                (g.name.clone(), dup_state_label(g.state).to_string())
            } else {
                (String::new(), String::new())
            };
            let mut row = vec![name, state, c.agent_id.clone(), human_bytes(c.bytes)];
            if show_installed {
                row.push(human_bytes(c.install_bytes));
            }
            // 路径不截断:截断交给 flex_col,重定向成文件时它整条留下,
            // 路径要能原样复制去核对(截掉的那一截正是要核对的那一截)。
            row.push(c.path.display().to_string());
            t.push_row(row);
        }
    }
    let mut out = format!("\n{}", t.render());

    // DRIFTED 的差异摘要在表下展开:"改了哪儿"是 DRIFTED 唯一有用的信息,
    // 塞进单元格必然被截断。
    for g in groups.iter().filter(|g| g.state == DupState::Drifted) {
        let Some(diff) = &g.diff else { continue };
        out.push_str(&format!(
            "\n\n  {} {}",
            style("drifted").yellow().bold(),
            accent().apply_to(&g.name)
        ));
        for line in diff.lines() {
            out.push_str(&format!("\n    {line}"));
        }
    }

    // 能省多少:只有 IDENTICAL 组能省,省的是「副本数 - 1」份内容体积
    // (install 子路径不算——它本来就不参与哈希,也不该被链接)。
    let shareable: u64 = groups
        .iter()
        .filter(|g| g.state == DupState::Identical)
        .map(|g| g.copies.iter().skip(1).map(|c| c.bytes).sum::<u64>())
        .sum();
    let drifted = groups
        .iter()
        .filter(|g| g.state == DupState::Drifted)
        .count();
    // 同一份 skill 也可能在一个 agent 里躺两处(目录不同、退化命名),
    // 所以说法是「不止一个地方」而不是「不止一个 agent」。
    out.push_str(&format!(
        "\n\n  {} {} {}",
        muted().apply_to("Total"),
        style(plural(groups.len(), "skill")).bold(),
        muted().apply_to(format!(
            "in more than one place · {drifted} drifted · {} could be shared with `duster skill link`",
            human_bytes(shareable)
        ))
    ));
    out
}

fn cmd_skill_link(
    mode: OutputMode,
    index: Option<&Path>,
    name: &str,
    from: &str,
    to: &str,
    dry_run: bool,
) -> i32 {
    let report = match link(index, None, name, from, to, dry_run) {
        Ok(r) => r,
        Err(e) => return fail(mode, "skill-link", &e),
    };
    match mode {
        OutputMode::Json => emit_json("skill-link", &report, &report.warnings),
        OutputMode::Human => {
            render_link(&report, dry_run);
            render_warnings(&report.warnings);
        }
    }
    if dry_run {
        // 预览什么都没动,和三个清理动词同一档。
        EXIT_CONFIRM_DENIED
    } else if report.verified {
        EXIT_OK
    } else {
        // 链接建好了但目标 agent 加载不了 —— 半成品必须报出来。
        EXIT_PARTIAL
    }
}

/// LinkMode 的人话。降级不是失败,但必须说清代价。
fn link_mode_label(mode: LinkMode) -> &'static str {
    match mode {
        LinkMode::Hardlink => "hard link (one copy on disk)",
        LinkMode::Symlink => "symlink (one copy, follows the store)",
        LinkMode::Copy => "copy (two copies; they can drift apart)",
    }
}

fn render_link(report: &LinkReport, dry_run: bool) {
    println!();
    println!(
        "  {} {} {} {}",
        if dry_run {
            warn_mark().to_string()
        } else {
            ok_mark().to_string()
        },
        accent().bold().apply_to(&report.name),
        muted().apply_to("→"),
        accent().apply_to(report.to.display().to_string())
    );
    for (label, value) in [
        ("from ", report.from.display().to_string()),
        ("store", report.cas_path.display().to_string()),
        ("how  ", link_mode_label(report.mode).to_string()),
    ] {
        println!("    {} {value}", muted().apply_to(label));
    }
    if let Some(reason) = &report.downgrade_reason {
        println!("    {} {reason}", muted().apply_to("why  "));
    }
    println!(
        "    {} {}",
        muted().apply_to("loads"),
        // dry-run 里 verified 天然是 false(什么都还没建),说成 "not verified"
        // 会被读成失败。这里分开说,免得预览看起来像出了错。
        match (report.verified, dry_run) {
            (true, _) => style("yes — SKILL.md parses in the target agent").to_string(),
            (false, true) => muted()
                .apply_to("not checked — nothing was created yet")
                .to_string(),
            (false, false) => style("not verified").yellow().to_string(),
        }
    );
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// `duster doctor` 的参数袋。四个旗标一起决定"跑哪几项",拆开传会让两个
/// 调用点(main 的分发与交互菜单)各抄一遍同样的四元组。
pub(crate) struct DoctorArgs {
    pub secrets: bool,
    pub ping: bool,
    /// 只查这几个 agent；空 Vec = 全部。
    pub agents: Vec<String>,
    /// 空 = 全跑。取值由 clap 按 [`ALL_CHECKS`] 校验,到手的一定合法。
    pub checks: Vec<String>,
}

fn cmd_doctor(mode: OutputMode, index: Option<&Path>, args: DoctorArgs) -> i32 {
    let report = match doctor(&DoctorOptions {
        index_path: index.map(Path::to_path_buf),
        home: None,
        agents: args.agents.clone(),
        secrets: args.secrets,
        ping: args.ping,
        checks: args.checks.clone(),
    }) {
        Ok(r) => r,
        Err(e) => return fail(mode, "doctor", &e),
    };
    match mode {
        OutputMode::Json => emit_json("doctor", &report, &report.warnings),
        OutputMode::Human => {
            render_doctor(&report, &args);
            render_warnings(&report.warnings);
        }
    }
    doctor_exit_code(&report)
}

/// 体检的退出码。
///
/// 有发现不等于命令失败——那些发现正是用户要的结果,报一个非零码会让
/// `duster doctor && deploy` 这类用法在"扫出两条明文凭据"时也算通过不了。
/// 但 [`Severity::Error`] 意味着真的有东西坏了(配置解析不了、库损坏、
/// 某项检查自己摔了),那一档必须让脚本停下来,落"部分成功"这一格。
fn doctor_exit_code(report: &DoctorReport) -> i32 {
    if report
        .findings
        .iter()
        .any(|f| f.severity == Severity::Error)
    {
        EXIT_PARTIAL
    } else {
        EXIT_OK
    }
}

/// 体检报告:按检查分组,组内一条一段。
///
/// 分组顺序恒为 [`ALL_CHECKS`](即 doctor 的执行顺序),不按发现数排——
/// 同一台机器两次运行的输出要能直接 diff。
///
/// **跑过却没发现的检查也占一行**(一句 `ok`)。这份报告的价值有一半在
/// "这一项我查过了",而一片空白既可能是"查过没事"也可能是"根本没查";
/// 让读者去猜,是这类工具最容易骗人的地方。
fn render_doctor(report: &DoctorReport, args: &DoctorArgs) {
    println!();
    for check in ALL_CHECKS {
        if !report.checks_run.iter().any(|c| c == check) {
            continue;
        }
        println!("  {}", accent().bold().apply_to(check));
        let mut found = 0usize;
        for f in report.findings.iter().filter(|f| f.check == check) {
            render_finding(f);
            found += 1;
        }
        if found == 0 {
            println!("    {}", muted().apply_to("ok — nothing found"));
        }
        println!();
    }
    render_doctor_footer(report, args);
}

/// 一条发现:`<严重度> <出处>`,说明缩进一层,有修法再来一行。
///
/// 出处**不截断**:它是文件路径、`<库>#<表>.<列>:<行>` 或 server 名,
/// 用户要照着它去改东西,截掉的正是要复制的那一截。
fn render_finding(f: &Finding) {
    println!("    {} {}", severity_tag(f.severity), f.subject);
    println!("      {}", muted().apply_to(&f.detail));
    if let Some(fix) = &f.fix {
        println!("      {} {fix}", muted().apply_to("fix:"));
    }
}

/// 严重度标签。定宽 5 列,好让右边的出处列对齐。
fn severity_tag(s: Severity) -> String {
    let (text, style) = match s {
        Severity::Error => ("error", Style::new().red().bold()),
        Severity::Warn => ("warn ", Style::new().yellow().bold()),
        Severity::Info => ("info ", muted()),
    };
    style.apply_to(text).to_string()
}

/// 收尾:总数 + 严重度分布,再逐条说明**哪几项没跑、为什么**。
///
/// 后半截不是补充说明,是这份报告可信的前提:六项里跑了几项、
/// 剩下的是被旗标关掉还是缺前置条件,不写出来的话,一份"没发现问题"
/// 的报告和一份"什么都没查"的报告长得一模一样。
fn render_doctor_footer(report: &DoctorReport, args: &DoctorArgs) {
    let count = |s: Severity| report.findings.iter().filter(|f| f.severity == s).count();
    let (errors, warns, infos) = (
        count(Severity::Error),
        count(Severity::Warn),
        count(Severity::Info),
    );
    let breakdown: Vec<String> = [(errors, "error"), (warns, "warn"), (infos, "info")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
    let tail = if breakdown.is_empty() {
        "nothing to fix".to_string()
    } else {
        format!("· {}", breakdown.join(" · "))
    };
    println!(
        "  {} {} {}",
        if report.findings.is_empty() {
            ok_mark().to_string()
        } else {
            warn_mark().to_string()
        },
        style(format!(
            "{} across {}",
            plural(report.findings.len(), "finding"),
            plural(report.checks_run.len(), "check")
        ))
        .bold(),
        muted().apply_to(tail)
    );
    for check in ALL_CHECKS {
        if report.checks_run.iter().any(|c| c == check) {
            continue;
        }
        println!(
            "  {}",
            muted().apply_to(format!("{check}: {}", skip_reason(check, args)))
        );
    }
}

/// `--secrets` 打开的那一项。字面量与 core 的 `CHECK_SECRETS` 同源
/// （测试 `跳过原因指名要加哪个旗标` 钉住这一点）。
const CHECK_SECRETS: &str = "secrets";
/// `--ping` 打开的那一项。
const CHECK_MCP: &str = "mcp-reachability";

/// 某一项没跑的原因。按优先级:先看是不是没被 `--check` 选中,
/// 再看是不是缺旗标,都不是就只剩缺索引一种可能。
///
/// 每条都要带上**怎么才能跑起来**。一句光秃秃的 "not run" 等于让用户
/// 自己去翻 `--help` 猜是哪个开关。
fn skip_reason(check: &str, args: &DoctorArgs) -> String {
    if !args.checks.is_empty() && !args.checks.iter().any(|c| c == check) {
        return "not run (--check selected other checks)".to_string();
    }
    match check {
        CHECK_SECRETS => "not run (pass --secrets)".to_string(),
        CHECK_MCP => "not run (pass --ping)".to_string(),
        // 其余三项只依赖索引;它们被跳过时 core 已经把详情写进 warnings。
        _ => "not run (needs the index — run `duster scan`)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 只有测试要造 SkillCopy(渲染路径只读它,不构造),放在这里非测试构建才不报未用。
    use duster_core::skill_ops::SkillCopy;

    #[test]
    fn highlight_按区间着色且不破坏区间外文本() {
        let s = "abc你好def";
        let got = highlight(s, &[(3, 9)], true);
        assert_eq!(got, "abc\x1b[1;31m你好\x1b[0m".to_owned() + "def");
        // 不着色时原样返回。
        assert_eq!(highlight(s, &[(3, 9)], false), s);
    }

    #[test]
    fn 锁冲突文案映射退出码_5() {
        let err = anyhow::anyhow!(
            "index database is locked by another duster instance: database is locked"
        );
        assert_eq!(exit_code_for(&err), EXIT_LOCKED);
        assert_eq!(
            exit_code_for(&anyhow::anyhow!("some other error")),
            EXIT_ERROR
        );
    }

    #[test]
    fn relative_time_分档() {
        assert_eq!(relative_time(None), "never");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert_eq!(relative_time(Some(now_ms)), "just now");
        assert!(relative_time(Some(now_ms - 5 * 60 * 1000)).contains("minutes ago"));
        assert!(relative_time(Some(now_ms - 3 * 3600 * 1000)).contains("hours ago"));
        assert!(relative_time(Some(now_ms - 50 * 86400 * 1000)).contains("days ago"));
        // 单数档:这一列在每张表里都出现,「1 days ago」是被复印最多的错字。
        assert_eq!(relative_time(Some(now_ms - 61 * 1000)), "1 minute ago");
        assert_eq!(relative_time(Some(now_ms - 3601 * 1000)), "1 hour ago");
        assert_eq!(relative_time(Some(now_ms - 86401 * 1000)), "1 day ago");
    }

    /// 计数格:0 与缺失都退化成占位横杠,非零原样。
    #[test]
    fn count_cell_零与缺失都是横杠() {
        assert_eq!(count_cell(None), "-");
        assert_eq!(count_cell(Some(&0)), "-");
        assert_eq!(count_cell(Some(&7)), "7");
        assert_eq!(count_cell_u64(Some(&0)), "-");
        assert_eq!(count_cell_u64(Some(&7)), "7");
    }

    /// clap 命令树自检:子命令、参数冲突、重名旗标写错了在这里就炸,
    /// 不用等用户敲到那条命令。
    #[test]
    fn clap_命令树自检() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    /// `--archive` / `--no-archive` 是三态输入的两个开关,同时给必须被 clap 挡住:
    /// "既打包又不打包"没有含义,替用户挑一个才是危险的。
    #[test]
    fn archive_与_no_archive_互斥() {
        assert!(
            Cli::try_parse_from([
                "duster",
                "prune",
                "--older-than",
                "30d",
                "--archive",
                "--no-archive",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["duster", "prune", "--older-than", "30d", "--archive"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["duster", "uninstall", "qoder", "--archive", "--no-archive"])
                .is_err()
        );
        // 三态收敛:未表态就是未表态,不许默认成任何一边。
        assert_eq!(archive_choice(true, false), Some(true));
        assert_eq!(archive_choice(false, true), Some(false));
        assert_eq!(archive_choice(false, false), None);
    }

    /// `--older-than` 只收带 `d` 的天数。裸 `30` 是"30 天还是 30 周"的歧义,
    /// 而这个参数决定删什么,歧义必须被拒。
    #[test]
    fn older_than_只接受带_d_的天数() {
        assert_eq!(parse_older_than("30d").unwrap(), 30);
        assert!(parse_older_than("30").is_err());
    }

    /// agent 持锁(`duster_fs::lockprobe::LockedError` 的文案)也要落退出码 5:
    /// 匹配不上会把"退出 agent 再来"这条唯一解法降级成一句普通报错。
    #[test]
    fn agent_持锁文案也映射退出码_5() {
        let err = anyhow::anyhow!(
            "/Users/x/.codex/logs_2.sqlite is locked by another process: codex is running. \
             Quit the agent and try again"
        );
        assert_eq!(exit_code_for(&err), EXIT_LOCKED);
    }

    /// 拒绝执行 ≠ 执行失败。core 明说"一个字节都没动"的四种拒绝落 4,
    /// 脚本据此区分"什么都没做"与"做坏了"。
    #[test]
    fn 拒绝执行文案映射退出码_4() {
        for msg in [
            "refusing to execute prune in --json mode without --yes",
            "archive estimate is 900 MiB ... above the 200 MiB auto-archive limit",
            "confirmation required: type the agent id verbatim, `--confirm qoder`",
            "uninstall preflight failed (1 of 3 checks); nothing was removed:",
        ] {
            assert_eq!(
                exit_code_for(&anyhow::anyhow!("{msg}")),
                EXIT_CONFIRM_DENIED,
                "{msg}"
            );
        }
        // 锁先判:被锁有自己的解法,不该被拒绝执行的分支吞掉。
        let both = anyhow::anyhow!("refusing to execute: the file is locked by another process");
        assert_eq!(exit_code_for(&both), EXIT_LOCKED);
    }

    /// 默认预览、显式执行:`--dry-run` 压过 `--yes`,`--json` 下没有"当场问"。
    #[test]
    fn 同意来源与退出码() {
        assert_eq!(consent_of(true, false), Consent::Granted);
        assert_eq!(consent_of(true, true), Consent::Preview);
        assert_eq!(consent_of(false, false), Consent::Preview);
        assert_eq!(settle(OutputMode::Json, Consent::Ask), Consent::Preview);
        assert_eq!(settle(OutputMode::Human, Consent::Ask), Consent::Ask);
        assert_eq!(settle(OutputMode::Json, Consent::Granted), Consent::Granted);
        // 什么都没动 = 4;动了但有子项失败 = 3;全成 = 0。
        assert_eq!(exec_exit_code(false, 0), EXIT_CONFIRM_DENIED);
        assert_eq!(exec_exit_code(true, 0), EXIT_OK);
        assert_eq!(exec_exit_code(true, 2), EXIT_PARTIAL);
    }

    /// 计划与报告的 warnings 去重保序:core 会把计划 warning 抄进报告,
    /// 原样输出等于让用户读两遍同一句话。
    #[test]
    fn warnings_合并去重且保序() {
        let plan = ["mcp 清理排在下个里程碑".to_string(), "b".to_string()];
        let report = ["b".to_string(), "c".to_string()];
        assert_eq!(
            merge_warnings(&plan, &report),
            ["mcp 清理排在下个里程碑", "b", "c"]
        );
    }

    /// 未执行的收尾必须把"怎么才算动"原样给出来:dry-run 报告的用法是
    /// 重定向成文件、读完再回来敲那条命令,不能靠用户回忆。
    #[test]
    fn 未执行的收尾话术带上下一条命令() {
        let preview = not_done_hint(false, "duster clean --yes");
        assert!(preview.contains("duster clean --yes"), "{preview}");
        let declined = not_done_hint(true, "duster prune --older-than 30d --yes");
        assert!(
            declined.contains("duster prune --older-than 30d --yes"),
            "{declined}"
        );
    }

    /// 计划分组:软件本体永远最后一组,可清理的在前,顺序固定——
    /// `--dry-run` 报告要能重定向成文件后 diff 两次运行。
    #[test]
    fn 计划分组_install_永远在最后() {
        fn item(kind: &str, level: Option<CleanLevel>, cleanable: bool) -> PlanItem {
            PlanItem {
                agent_id: "codex".into(),
                kind: kind.into(),
                clean_level: level,
                path: PathBuf::from("/tmp/x"),
                key: "k".into(),
                rid: 1,
                what: "w".into(),
                why: "y".into(),
                impact: "i".into(),
                archived: false,
                action: if cleanable {
                    Action::RemoveDir
                } else {
                    Action::Keep
                },
                bytes: 10,
                cleanable,
                generation: false,
                last_used_ms: None,
            }
        }
        let items = [
            item("install", None, false),
            item("session", None, true),
            item("artifact", Some(CleanLevel::L2), true),
            item("artifact", Some(CleanLevel::L1), true),
        ];
        let labels: Vec<String> = group_plan(items.iter())
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        assert_eq!(labels, ["artifact l1", "artifact l2", "session", "install"]);
        // install 行的标记是它在计划里的全部意义:让用户看见为什么不动。
        assert!(archive_mark(&items[0]).contains(NOT_CLEANABLE));
        assert!(!archive_mark(&items[1]).contains(NOT_CLEANABLE));
    }

    /// 同一个动作在计划行与执行结果表里必须是同一句话。core 给的是 [`Action`]
    /// 的 serde 名,这条断言把两条渲染路径钉在一起——漏一个变体就会在
    /// 同一屏里冒出 `remove_dir` 与 "delete folder" 两种叫法。
    #[test]
    fn 动作名在计划与结果里一致() {
        for action in [
            Action::Vacuum,
            Action::RemoveSidecar,
            Action::RemoveFile,
            Action::RemoveDir,
            Action::TruncateFile,
            Action::CompressFile,
            Action::Keep,
        ] {
            let serde_name = serde_json::to_string(&action).unwrap();
            let raw = serde_name.trim_matches('"');
            assert_eq!(action_phrase(raw), action_label(action), "{raw}");
        }
        // 认不出的原样透出,不猜。
        assert_eq!(action_phrase("rewrite_universe"), "rewrite_universe");
    }

    /// 复数:"1 items" 这种小破绽会让人连带怀疑其余数字。
    #[test]
    fn plural_单复数() {
        assert_eq!(plural(1, "item"), "1 item");
        assert_eq!(plural(0, "item"), "0 items");
        assert_eq!(plural(69, "item"), "69 items");
    }

    fn f(check: &str, severity: Severity) -> Finding {
        Finding {
            check: check.to_string(),
            severity,
            subject: "x".to_string(),
            detail: "d".to_string(),
            fix: None,
        }
    }

    /// 体检的退出码只认 Error 一档:扫出明文凭据是**结果**,不是失败,
    /// 报非零会让 `duster doctor && deploy` 在有发现时永远过不去;
    /// 而配置解析不了、库损坏这类 Error 必须让脚本停。
    #[test]
    fn doctor_只有_error_档落部分成功() {
        let mut r = DoctorReport::default();
        assert_eq!(doctor_exit_code(&r), EXIT_OK);

        r.findings.push(f("secrets", Severity::Warn));
        r.findings.push(f("dangling-reference", Severity::Info));
        assert_eq!(doctor_exit_code(&r), EXIT_OK);

        r.findings.push(f("config-syntax", Severity::Error));
        assert_eq!(doctor_exit_code(&r), EXIT_PARTIAL);
    }

    /// 没跑的那几项必须说清**怎么才能跑起来**。一句光秃秃的 "not run"
    /// 等于让用户回去翻 `--help` 猜是哪个开关。
    ///
    /// 顺带钉住两个字面量:它们要和 core 的检查名对得上,否则页脚会把
    /// "缺 --secrets" 说成"缺索引"——名字在 core 那边改一个字,这里就红。
    #[test]
    fn 跳过原因指名要加哪个旗标() {
        assert!(ALL_CHECKS.contains(&CHECK_SECRETS));
        assert!(ALL_CHECKS.contains(&CHECK_MCP));

        let none = DoctorArgs {
            secrets: false,
            ping: false,
            agents: Vec::new(),
            checks: Vec::new(),
        };
        assert!(skip_reason(CHECK_SECRETS, &none).contains("--secrets"));
        assert!(skip_reason(CHECK_MCP, &none).contains("--ping"));
        assert!(skip_reason("sqlite-integrity", &none).contains("duster scan"));

        // 被 `--check` 挑剩下的,原因是"你没选它",不是"你少给了个旗标"。
        let picked = DoctorArgs {
            checks: vec!["config-syntax".to_string()],
            ..none
        };
        assert!(skip_reason(CHECK_SECRETS, &picked).contains("--check"));
        assert!(skip_reason("sqlite-integrity", &picked).contains("--check"));
    }

    /// 严重度三档在人类模式下必须分得开(等宽 5 列,好让出处列对齐),
    /// 三个标签互不相同。
    #[test]
    fn 严重度标签定宽且互不相同() {
        let tags: Vec<String> = [Severity::Error, Severity::Warn, Severity::Info]
            .into_iter()
            .map(severity_tag)
            .collect();
        for t in &tags {
            assert_eq!(display_width(t), 5, "{t:?}");
        }
        assert_ne!(tags[0], tags[1]);
        assert_ne!(tags[1], tags[2]);
    }

    // ---------------------------------------------------------------------
    // skill copies / open 双 id 空间(B2)
    // ---------------------------------------------------------------------

    /// `skill dedupe` 是历史动词,无过渡期:必须被 clap 拒掉,而不是
    /// 默默当 `copies` 的别名用——脚本一旦开始依赖旧名,改名就白改了。
    #[test]
    fn skill_命令树里没有_dedupe_只剩_copies() {
        assert!(Cli::try_parse_from(["duster", "skill", "copies"]).is_ok());
        assert!(Cli::try_parse_from(["duster", "skill", "dedupe"]).is_err());
        // --json 的命令串随之改名,不许两头都能叫。
        assert!(Cli::try_parse_from(["duster", "skill", "copies", "--json"]).is_ok());
    }

    /// 一份测试用的副本。install_bytes 由调用方给——「INSTALLED 列该不该
    /// 出现」只取决于它。
    fn copy(agent: &str, path: &str, bytes: u64, install_bytes: u64) -> SkillCopy {
        SkillCopy {
            agent_id: agent.into(),
            path: PathBuf::from(path),
            tree_hash: "abc".into(),
            bytes,
            install_bytes,
        }
    }

    fn group(name: &str, copies: Vec<SkillCopy>) -> SkillGroup {
        SkillGroup {
            name: name.into(),
            state: DupState::Identical,
            copies,
            diff: None,
            warnings: Vec::new(),
        }
    }

    /// INSTALLED 列只在报告里真有非零安装体积时出现:一台机器的清单
    /// 没给 skill 声明 install_paths,整列就是 `0 B`,竖在那里既占
    /// PATH 列的宽度,又让人去找一行不存在的信息。
    #[test]
    fn installed_列_全零省略_有非零才出现() {
        let all_zero = render_skill_groups(&[group(
            "foo",
            vec![
                copy("claude-code", "/a/foo", 100, 0),
                copy("codex", "/b/foo", 100, 0),
            ],
        )]);
        assert!(!all_zero.contains("INSTALLED"), "{all_zero}");
        assert!(all_zero.contains("PATH"), "{all_zero}");
        // 路径原样留下,截断是 flex_col 的事(重定向成文件时不能丢)。
        assert!(all_zero.contains("/b/foo"), "{all_zero}");

        let one_nonzero = render_skill_groups(&[group(
            "foo",
            vec![
                copy("claude-code", "/a/foo", 100, 0),
                copy("codex", "/b/foo", 100, 4096),
            ],
        )]);
        assert!(one_nonzero.contains("INSTALLED"), "{one_nonzero}");
        assert!(one_nonzero.contains("4 KB"), "{one_nonzero}");
    }

    /// codex 型会话夹具:假 home + 真扫描,与 cmd/session.rs 的测试同一套。
    /// 两场会话:一场 0 轮(只有 session_meta)、一场 1 轮。轮次 tid 是
    /// 从 1 起连续自增的,而 rid 也一样——单场有轮次的会话,它的 rid 几乎
    /// 必然撞上某个 tid(`open` 按契约 turn 优先),所以要留一场 0 轮会话
    /// 当「纯会话 id」。返回 (临时目录, home, 索引路径)。
    fn session_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();
        let codex = home.join(".codex");
        std::fs::create_dir_all(codex.join("sessions/2025/01/01")).unwrap();
        std::fs::write(codex.join("config.toml"), b"# empty\n").unwrap();
        let meta_line = concat!(
            r#"{"timestamp":"2025-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"{id}","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/Users/me/Code/agent-duster","cli_version":"1"}}"#,
            "\n",
        );
        // 0 轮会话:只有 session_meta,用来占一个不属于任何 tid 的 rid。
        let a = codex.join("sessions/2025/01/01/rollout-2025-01-01T00-00-00-aaaa.jsonl");
        std::fs::write(&a, meta_line.replace("{id}", "aaaa")).unwrap();
        // 1 轮会话:user 轮,占据 tid 1。
        let b = codex.join("sessions/2025/01/01/rollout-2025-01-01T00-00-00-bbbb.jsonl");
        std::fs::write(
            &b,
            concat!(
                r#"{"timestamp":"2025-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"bbbb","timestamp":"2025-01-01T00:00:00.000Z","cwd":"/Users/me/Code/agent-duster","cli_version":"1"}}"#,
                "\n",
                r#"{"timestamp":"2025-01-01T00:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello duster"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let index = home.join(".agent-duster/index.db");
        duster_core::scan::scan(&duster_core::scan::ScanOptions {
            home: Some(home.clone()),
            index_path: Some(index.clone()),
            full: false,
        })
        .expect("scan 夹具");
        (tmp, home, index)
    }

    /// `open` 的数字不标自明:会话 id 要渲染整场会话而不是报错,
    /// turn id 照旧渲染单轮,两边都不是才报错且点名两个命令。
    #[test]
    fn open_给会话_id_渲染整场会话而非报错() {
        let (_tmp, _home, index) = session_fixture();
        let rows = duster_core::session::list(
            Some(&index),
            &duster_core::session::SessionFilter::default(),
        )
        .unwrap();
        assert_eq!(rows.len(), 2, "夹具里该有两场会话");
        // 挑一个「不是任何 tid」的 rid:按契约 turn 优先,只有查无 tid
        // 才会落到会话 id,测试必须真的走上那条路。
        let rid = rows
            .iter()
            .map(|r| r.rid)
            .find(|&r| open_turn(Some(&index), r).is_err())
            .expect("夹具里该有一个不属于任何 tid 的会话 id");

        // 会话 id → 会话渲染路径。
        match resolve_open(Some(&index), rid).unwrap() {
            OpenTarget::Session(_) => {}
            other => panic!("{rid} 是会话 id,应解析成 Session: {other:?}"),
        }
        // 第一条轮次(tid 1)→ 轮次渲染路径(全新库 tid 从 1 起)。
        match resolve_open(Some(&index), 1).unwrap() {
            OpenTarget::Turn(t) => assert_eq!(t.role, "user"),
            other => panic!("tid 1 应解析成 Turn: {other:?}"),
        }
        // 两边都不是:报错要同时点名两个命令和各自的数字来源。
        let err = resolve_open(Some(&index), 999_999).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("duster search"), "{msg}");
        assert!(msg.contains("duster session list"), "{msg}");

        // 整条命令走通:会话 id 返回成功,而不是落到报错退出码。
        assert_eq!(
            cmd_open(OutputMode::Human, Some(&index), rid, false),
            EXIT_OK
        );
        assert_eq!(
            cmd_open(OutputMode::Human, Some(&index), 999_999, false),
            EXIT_ERROR
        );
    }
}
