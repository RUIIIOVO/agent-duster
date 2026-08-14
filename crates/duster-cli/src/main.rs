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
use duster_core::delete::{DeleteOptions, DeleteReport};
use duster_core::doctor::{Finding, SELF_CHECKS, SelfReport, Severity, self_check};
use duster_core::freshness::{Freshness, ensure_fresh};
use duster_core::plan::{Action, Plan, PlanFilter, PlanItem, Verb, parse_older_than};
use duster_core::prune::{PruneOptions, PruneOutcome, prune, prune_filtered};
use duster_core::scan::{ScanOptions, ScanReport, scan};
use duster_core::search::{SearchFilter, SearchHit, TurnDetail, open_turn, search};
use duster_core::session::SessionDetail;
use duster_core::skill_ops::{self, DupState, LinkMode, LinkReport, SkillGroup, link};
use duster_core::status::{StatusReport, status};
use duster_core::uninstall::{
    PackageOutcome, PreflightCheck, SharedAction, SharedEditOutcome, UninstallOptions, uninstall,
};
use duster_fs::path::display_tilde;
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
        (
            "duster status",
            "See disk use per agent, and what it keeps",
        ),
        (
            "duster search \"docker\"",
            "Find a past conversation by its words",
        ),
        ("duster open t42", "Read the whole turn that search found"),
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
            "duster skill list",
            "See every skill, and which ones live in more than one place",
        ),
        ("duster doctor", "Check that duster itself is set up right"),
        (
            "duster mcp list",
            "See every MCP server declaration, agent by agent",
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
        /// Which turn or conversation to read. Three forms: `t<turn-id>`
        /// (from `duster search`), `s<conversation-id>` (from `duster
        /// session list`), or a bare `<number>` that matches either —
        /// e.g. t42 / s500 / 42. Bare numbers that match both are rejected
        /// with two pasteable commands, so nothing gets silently picked
        #[arg(value_name = "ID")]
        id: String,
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
    /// See every MCP server you have, one row per agent that declares it
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
    /// Check that duster itself is set up right: index database, adapters,
    /// folder permissions, the SQLite databases your agents keep, version
    ///
    /// This one is about duster and this machine, not about your agents'
    /// leftover configs — the checks that looked at those (plaintext keys,
    /// broken links, bad configs) were removed in an earlier round, together
    /// with every status flag that switched them.
    // 旗标全清是这两轮改名的另一半:`--no-secrets` / `--ping` / `--agent` /
    // `--check` 连同它们伺候的六项检查整体撤掉,status 的三个旗标(issues /
    // deep / ping)这一轮也删了。`brew doctor` 查 brew、`flutter doctor` 查
    // flutter,从来没有一个 doctor 是查别人家的;这条命令从前查遍了除 duster
    // 以外的一切,名字在撒谎——所以换的是名字底下的东西,不是名字。如今
    // doctor 只查「duster 自己装好没有」,外加 agent 的 SQLite 库读不读得动
    // (那是环境事实,见 duster_core::doctor::check_sqlite 的归属理由)。
    Doctor,
}

#[derive(Subcommand)]
enum SkillCmd {
    /// List every skill, marking the ones in more than one place and the copies that drifted
    List,
    /// Give another agent the same skill, sharing one copy on disk
    Link {
        /// Skill name as `duster skill list` prints it
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
    /// Delete one copy of a skill, after packing it into ~/agent-duster-exports
    Rm {
        /// Skill name as `duster skill list` prints it
        name: String,
        /// Which agent's copy to delete. Required when several agents have
        /// this skill — duster never deletes all copies on your behalf
        #[arg(long, value_name = "AGENT")]
        agent: Option<String>,
        /// Which copy, by the PATH column of `duster skill list`. Required
        /// when one agent keeps this skill in several directories
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Delete without packing a copy first (for scripts)
        #[arg(long)]
        no_archive: bool,
        /// Print what would be deleted and stop
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let mode = is_json_mode(cli.json);
    let index = cli.index.as_deref();
    // 读索引的一次性命令先把库对齐磁盘。「索引」是实现细节,不该是用户
    // 要先学会的概念(见 needs_fresh_index 里的三个例外)。
    if needs_fresh_index(cli.command.as_ref()) {
        refresh_index(mode, index);
    }
    let code = match &cli.command {
        Some(Command::Scan { full }) => cmd_scan(mode, index, *full),
        Some(Command::Status) => cmd_status(mode, index),
        Some(Command::Search {
            query,
            agents,
            limit,
        }) => cmd_search(mode, index, query, agents.clone(), *limit),
        // open 的 id 是自由文本,clap 不校验形状;解析失败是用法错误,
        // 与 `prune` 的 `--older-than` 值不合法同落退出码 2。
        Some(Command::Open { id, full }) => match parse_open_id(id) {
            Ok(parsed) => cmd_open(mode, index, parsed, *full),
            Err(msg) => usage(mode, "open", &msg),
        },
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
            // uninstall 没有 `--yes`:授权凭据是 `--confirm` 的确认串。
            // 相等性由 cmd_uninstall 校验,不相等就降级成预览。
            if *dry_run {
                Consent::Preview
            } else {
                Consent::Granted
            },
        ),
        Some(Command::Skill { action }) => match action {
            SkillCmd::List => cmd_skill_list(mode, index),
            SkillCmd::Link {
                name,
                from,
                to,
                dry_run,
            } => cmd_skill_link(mode, index, name, from, to, *dry_run),
            SkillCmd::Rm {
                name,
                agent,
                path,
                no_archive,
                dry_run,
            } => cmd_skill_rm(
                mode,
                index,
                None,
                name,
                agent.as_deref(),
                path.as_deref(),
                !*no_archive,
                *dry_run,
            ),
        },
        Some(Command::Memory { action }) => cmd::memory::run(mode, index, action),
        Some(Command::Mcp { action }) => cmd::mcp::run(mode, index, action.clone()),
        Some(Command::Session { action }) => cmd::session::run(mode, index, action.clone()),
        Some(Command::Diff(args)) => cmd::diff::run(mode, args),
        Some(Command::Doctor) => cmd_doctor(mode, index),
        // 裸 `duster`:交互式启动菜单(TTY),否则打印帮助。
        None => interactive::run(mode, index),
    };
    std::process::exit(code);
}

/// 一次性命令跑之前要不要先把索引对齐磁盘。
///
/// 「索引」是实现细节,不该是用户要先学会的概念——他敲 `duster search`,要的是
/// 搜到东西,不是先被告知「请先运行 duster scan」。增量刷新实测 0.52 秒、冷启动
/// 全量 4.14 秒(见 [`duster_core::freshness`]),一次性命令等得起这一下。
///
/// 三个例外,都不是为了省那半秒:
/// - `scan`:它本身就是扫描,先扫一遍等于扫两遍。
/// - `doctor`:它自检的是 duster 自己(索引库、adapter 清单、目录权限、版本),
///   一个字节都不从 agent 索引里读——更何况「这台机器还没索引过」正是它要报的
///   一项,顺手建一次库就把那条诊断本身抹掉了。core 的 `doctor.rs` 写着同一句,
///   两头口径不许走散。
/// - `diff`:比的是命令行给的那两个路径,一个字节都不从索引里读。
///
/// 裸 `duster`(`None`)也不在这里等:交互菜单把扫描扔进后台线程,菜单立刻就出来
/// (见 [`interactive`] 里的 `BackgroundScan`),在这里同步等一遍正好把那件事作废。
///
/// 不留 `_ => true` 的兜底:下一条命令加进来时,编译器必须逼作者在这两档里选一边,
/// 而不是替他默认一个。
fn needs_fresh_index(command: Option<&Command>) -> bool {
    match command {
        Some(
            Command::Status
            | Command::Search { .. }
            | Command::Open { .. }
            | Command::Clean { .. }
            | Command::Prune { .. }
            | Command::Uninstall { .. }
            | Command::Skill { .. }
            | Command::Memory { .. }
            | Command::Mcp { .. }
            | Command::Session { .. },
        ) => true,
        Some(Command::Scan { .. } | Command::Doctor | Command::Diff(_)) | None => false,
    }
}

/// 跑命令之前把索引对齐磁盘。**绝不阻断命令**:扫描失败降级成一行 stderr
/// warning,该读的照读——库多半还在盘上(读到的只是旧一点的数据),而每个
/// 只读入口自己还有 `ensure_exists` 兜底。
///
/// 只有 [`Freshness::Built`] 说一句。`Refreshed` / `UpToDate` / `SkippedLocked`
/// 一律静默:半秒钟的事,报出来只是噪音;撞锁更不是错(另一个 duster 实例正在写,
/// 读路径全是 `open_readonly`,照样看得见)。
///
/// 那一句印在扫描**之后**,因为「这是不是第一次」只有扫过才知道。要在动手之前
/// 预告,CLI 就得自己再推一遍缺省库路径,而那是 duster-core 的口径,抄过来就成了
/// 第二份真相。所以它是回执而不是预告:解释刚才那四秒去哪了,并且说清它不会再来
/// 第二次。
///
/// `--json` 下不印这句回执——它是说给盯着终端等的人听的,而信封里没有它的位置。
/// warning 照印:数据是不是旧了,机器也该知道。
fn refresh_index(mode: OutputMode, index: Option<&Path>) {
    match ensure_fresh(index) {
        Ok(Freshness::Built) if mode == OutputMode::Human => eprintln!(
            "  {} {}",
            ok_mark(),
            muted().apply_to(
                "built the index for the first time; later runs only re-read what changed"
            )
        ),
        Ok(_) => {}
        Err(e) => eprintln!(
            "  {} {}",
            warn_mark(),
            muted().apply_to(format!(
                "index refresh failed: {e:#}; using what the index already has"
            ))
        ),
    }
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
        // 别说「skipped」:悬空软链那类行是**入库了**的(size 0),这正是
        // 它们能在 `skill list` 里以 broken 露头的前提。这一行只该说
        // 「这些项没量全」,不能宣布它们不存在。
        eprintln!(
            "  {}",
            muted().apply_to(
                "Those items were indexed with what could be read; everything else was indexed in full."
            )
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
            muted().apply_to("installed software (never touched by clean/prune)")
        );
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// `duster status`。
///
/// 上一轮把体检整个撤了:issues / deep / ping 三个旗标连同它们伺候的六项
/// 检查一起删掉,屏尾也不再有那行问题摘要,退出码回到纯 0/失败——这一屏
/// 现在就是一张 agent 表加足迹。agent 的 SQLite 库读不读得动改由
/// `duster doctor` 的自检去查(见 `duster_core::doctor::check_sqlite` 的
/// 归属理由)。
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
    } else {
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
        OutputMode::Human => {
            // 「无命中」的提示走 stderr(过程),命中列表才是结果。
            if hits.is_empty() {
                eprintln!();
                eprintln!("  {} No matches for {}", warn_mark(), style(query).bold());
                eprintln!(
                    "  {}",
                    muted().apply_to(
                        "Search needs 3 characters or more. If the index is stale, run `duster scan`."
                    )
                );
            } else {
                println!("{}", render_search_human(&hits, query));
            }
        }
    }
    EXIT_OK
}

/// 命中列表的整块渲染。返回而不是边算边印:这样它能被测试逐字核对
/// (与 cmd/session.rs 的 `render_list` 同一个理由)。`hits` 非空——
/// 「无命中」的提示走 stderr,由 [`cmd_search`] 直接印。
fn render_search_human(hits: &[SearchHit], query: &str) -> String {
    let color = use_color();
    let dot = muted().apply_to("·").to_string();
    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!(
        "  {}\n",
        muted().apply_to(format!(
            "{} {} for \"{query}\"",
            hits.len(),
            if hits.len() == 1 { "match" } else { "matches" }
        ))
    ));
    for h in hits {
        let file = Path::new(&h.resource_path).file_name().map_or_else(
            || h.resource_path.clone(),
            |f| f.to_string_lossy().into_owned(),
        );
        // 命中行的第一格是 `t<tid>`(不是裸 `#<tid>`):turn id 与会话 id
        // 都是裸数字、长得一模一样,前缀让 search 的定位符可以直接粘进
        // `duster open`,也不会和 session list 的 `s<rid>` 混为一谈。
        out.push_str(&format!(
            "  {}  {} {dot} {} {dot} {} {dot} {}\n",
            style(format!("t{}", h.tid)).yellow().bold(),
            accent().apply_to(&h.agent_id),
            // 会话文件名可以长到七十列(codex 的 rollout-<时间>-<uuid>),
            // 截断保住一行一条;定位靠 tid,文件名只是上下文。
            muted().apply_to(truncate_width(&file, 32)),
            muted().apply_to(format!("turn {}", h.seq)),
            style(&h.role).magenta()
        ));
        out.push_str(&format!("      {}\n", highlight(&h.snippet, &h.highlights, color)));
    }
    out.push('\n');
    out.push_str(&format!(
        "  {}",
        muted().apply_to("Read one in full: duster open t<id>")
    ));
    out
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

/// open 的寻址:前缀一上来就锁定空间,裸数字两个都试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenId {
    /// `t<n>`:只查轮次空间,查不到就报错,不退化去查会话。
    Turn(i64),
    /// `s<n>`:只查会话空间,同理。
    Session(i64),
    /// 裸 `<n>`:两个空间都试(向后兼容脚本);两边都命中时报错让用户消歧。
    Either(i64),
}

/// 三种可接受的形态,报错时一条给全,用户不用回去翻 --help。
const OPEN_ID_FORMS: &str = "accepted forms: t<turn-id> (from `duster search`), \
     s<conversation-id> (from `duster session list`), or a bare <number> that \
     matches either — e.g. t42 / s500 / 42";

/// 把 `open` 的 id 参数解析成 [`OpenId`]。纯函数,不碰 IO,形状契约
/// 全在这里定死,`cmd_open` 只消费结果。
///
/// 前缀只认小写,与 search / session list 印出来的一字不差:印的是 `t42`,
/// 那就该粘 `t42`,宽容大小写只会让报错文案和实际输出对不上号。
/// 数字只认纯 ASCII 十进制:空 `t`、`t-1`、`+3`、`3_0` 都得死在这里,
/// 不靠 `parse` 的宽容度兜底(与 parse_older_than 同一套判据)。
fn parse_open_id(s: &str) -> Result<OpenId, String> {
    let raw = s.trim();
    // 首字节是 ASCII 的 t/s 才算前缀:CJK 等多字节字符的首字节 ≥ 0x80,
    // 不可能误判成前缀。
    let (space, rest) = match raw.as_bytes().first() {
        Some(b't') => (Some(OpenSpace::Turn), &raw[1..]),
        Some(b's') => (Some(OpenSpace::Session), &raw[1..]),
        _ => (None, raw),
    };
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid id {s:?}: {OPEN_ID_FORMS}"));
    }
    let n: i64 = match rest.parse() {
        Ok(n) => n,
        // 位数溢出 i64。正常 id 只有 5~6 位,超长不是笔误就是攻击。
        Err(_) => {
            return Err(format!("invalid id {s:?}: number is out of range. {OPEN_ID_FORMS}"))
        }
    };
    Ok(match space {
        Some(OpenSpace::Turn) => OpenId::Turn(n),
        Some(OpenSpace::Session) => OpenId::Session(n),
        None => OpenId::Either(n),
    })
}

/// 前缀 → 寻址空间。与 [`OpenId`] 分开:解析先定空间,再填数字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenSpace {
    Turn,
    Session,
}

/// 把 [`OpenId`] 解析成轮次或整场会话。
///
/// 前缀一上来就锁定空间:`t<n>` 只在轮次空间查、`s<n>` 只在会话空间查,
/// 查不到就报错,不退化去另一个空间——用户既然标了前缀,想要的已经写明,
/// 替他去另一档找只会把「id 打错了」静默变成「找到了别的东西」。
/// 裸 `<n>` 保留老行为(两个空间都试、向后兼容脚本),但两档都命中时
/// **不许自己挑一个**:tid 与 rid 都是自增主键,同一个数字迟早同时存在,
/// 闷声返回轮次会静默吞掉用户想开的会话,所以这时必须报错并给出两条
/// 可直接粘贴的 `duster open t<n>` / `duster open s<n>` 让用户消歧。
fn resolve_open(index: Option<&Path>, id: OpenId) -> anyhow::Result<OpenTarget> {
    match id {
        OpenId::Turn(n) => match open_turn(index, n) {
            Ok(d) => Ok(OpenTarget::Turn(d)),
            // 只报轮次空间查无,顺带指一下另一种可能,不替用户换空间。
            Err(e) if is_missing(&e) => anyhow::bail!(
                "no turn with id t{n}. Turn ids come from `duster search` \
                 (they look like t42); use `duster open s{n}` only if you \
                 meant a conversation."
            ),
            Err(e) => Err(e),
        },
        OpenId::Session(n) => match duster_core::session::show(index, n) {
            Ok(d) => Ok(OpenTarget::Session(d)),
            Err(e) if is_missing(&e) => anyhow::bail!(
                "no conversation with id s{n}. Conversation ids come from \
                 `duster session list` (they look like s500); use \
                 `duster open t{n}` only if you meant a turn."
            ),
            Err(e) => Err(e),
        },
        OpenId::Either(n) => {
            // 先按轮次查。turn 报的不是「查无此项」就直接透传:turn 存在但
            // 源文件读不动是另一回事,拿同一个数字去 show 只会把错误再包一层
            // (判据只能匹配 Display 的稳定文案,与 REFUSAL_PHRASES 同一套
            // 跨层字符串契约,两侧各有测试守着)。
            let turn = match open_turn(index, n) {
                Ok(d) => Some(d),
                Err(e) if is_missing(&e) => None,
                Err(e) => return Err(e),
            };
            let sess = match duster_core::session::show(index, n) {
                Ok(d) => Some(d),
                Err(e) if is_missing(&e) => None,
                Err(e) => return Err(e),
            };
            match (turn, sess) {
                // 两档都命中:不许自己挑一个——见 [`resolve_open`] 的文档,
                // 这就是那条「数字撞车」的定时炸弹,现在拆掉。
                (Some(_), Some(_)) => anyhow::bail!(
                    "id {n} matches both a turn id and a conversation id. \
                     Pick one: `duster open t{n}` for the turn (from \
                     `duster search`), or `duster open s{n}` for the \
                     conversation (from `duster session list`)"
                ),
                (Some(d), None) => Ok(OpenTarget::Turn(d)),
                (None, Some(d)) => Ok(OpenTarget::Session(d)),
                // 两个都落空:报错必须同时点名两个命令和各自的数字来源,
                // 让用户知道自己用的是哪一档数字、该去哪一档找。
                (None, None) => anyhow::bail!(
                    "id {n} matches neither a turn id nor a conversation id. \
                     `duster search` prints turn ids (t42); `duster session \
                     list` prints conversation ids (s500). Use one of those \
                     to find a valid id."
                ),
            }
        }
    }
}

/// 「查无此项」判据:open_turn 与 session::show 各自的缺省文案都含这句。
fn is_missing(err: &anyhow::Error) -> bool {
    err.to_string().contains("does not exist")
}

fn cmd_open(mode: OutputMode, index: Option<&Path>, id: OpenId, full: bool) -> i32 {
    let target = match resolve_open(index, id) {
        Ok(t) => t,
        Err(e) => return fail(mode, "open", &e),
    };
    // 裸数字的解读说明:两档 id 长得一模一样,不点破用户下次还会拿
    // session list 的 id 来 open,再吃一次同样的错。显式前缀(t42 / s500)
    // 不用解释——空间是用户自己写的。
    let notice: Option<String> = match (&id, &target) {
        (OpenId::Either(n), OpenTarget::Turn(_)) => Some(format!(
            "id {n} is a turn id — showing the turn (conversation ids come \
             from `duster session list`)"
        )),
        (OpenId::Either(n), OpenTarget::Session(_)) => Some(format!(
            "id {n} is a conversation id — showing the conversation (turn \
             ids come from `duster search`)"
        )),
        _ => None,
    };
    let warnings: Vec<String> = notice.into_iter().collect();
    match target {
        OpenTarget::Turn(detail) => match mode {
            OutputMode::Json => emit_json("open", &detail, &warnings),
            OutputMode::Human => {
                // 裸数字的解读说明先落地:它解释了下面这轮是怎么被挑出来的。
                for w in &warnings {
                    println!("  {}", muted().apply_to(w));
                }
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
        OpenTarget::Session(detail) => match mode {
            // 解读说明与 Turn 臂同源(warnings),在渲染正文前落地。
            OutputMode::Json => emit_json("open", &detail, &warnings),
            OutputMode::Human => {
                for w in &warnings {
                    println!("  {}", muted().apply_to(w));
                }
                cmd::session::render_show(&detail, full);
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
    /// 已授权:`--yes`(clean / prune)或 `--confirm <agent>` / 菜单里的
    /// 两道确认(uninstall)。
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
///
/// 辅音 + `y` 结尾走 `-ies`（`copy` → `copies`）：`skill rm` 报的就是
/// copy 数,"2 copys" 一样是破绽。元音 + `y`（`day`）照旧只加 `s`。
/// 不做更全的英语变形表——这里的名词是闭集(item / copy / conversation /
/// server / skill / memory / declaration),够用即止。
pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        return format!("{n} {word}");
    }
    let mut chars = word.chars().rev();
    match (chars.next(), chars.next()) {
        (Some('y'), Some(prev)) if !matches!(prev, 'a' | 'e' | 'i' | 'o' | 'u') => {
            format!("{n} {}ies", &word[..word.len() - 1])
        }
        _ => format!("{n} {word}s"),
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
        // not cleanable 的组逐项印 what/why/impact 是纯复读:13 个 install
        // 项的三段话逐字相同,39 行里有 36 行是同一句,把真正的清理计划挤到
        // 屏幕外。折成「每项一行 what」+ 组尾一句共用的 why:`what` 本身就
        // 带着 agent、路径与体积(install 行的 `bytes` 按契约恒为 0,真体积
        // 只在这句话里),而 `impact` 逐项点名各自的 agent,共用一句会对另外
        // 十二项说谎——所以共用的只有那句与 agent 无关的 why,处置办法写成
        // 不点名的通用式。逐项三段仍在 `--json` 里(脚本的入口,不靠人眼扫)。
        if cleanable {
            for item in items {
                render_plan_item(item);
            }
        } else {
            for item in &items {
                println!("    {}", accent().apply_to(&item.what));
            }
            if let Some(first) = items.first() {
                println!(
                    "      {} {}",
                    muted().apply_to(format!("{:<6}", "why")),
                    muted().apply_to(&first.why)
                );
                println!(
                    "      {} {}",
                    muted().apply_to(format!("{:<6}", "how")),
                    muted().apply_to(
                        "Reclaim it only with `duster uninstall <agent>`, which takes all of that agent's data with it."
                    )
                );
            }
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
    // 而 core 的确认校验在生成计划之前——不补齐的话预览(以及 `--dry-run` /
    // 确认串不匹配降级成的预览)根本走不到报告。真正的授权只有一处:执行路径
    // 把用户给的那个串原样交给 core,由它再校验一次。
    let preview = || build(false, Some(args.agent.clone()));
    let report = match uninstall(&if consent == Consent::Granted {
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
    }
    // 交互路径的确认不在这一层:菜单里两道 y/N 过完才以 Granted 进来
    // (见 interactive::prompt_uninstall),这里不再有当场问人的分支——
    // 卸载的全部授权逻辑收进「确认串 == agent id」这一道门,CLI 与菜单同门。

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
    let hint = (!report.executed).then(|| not_done_hint(false, &next));
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

/// `pub(crate)` 而不是私有:同一条实现要能被交互菜单的 skill 屏调用,
/// 一条命令只有一份渲染。
pub(crate) fn cmd_skill_list(mode: OutputMode, index: Option<&Path>) -> i32 {
    let groups = match skill_ops::list(index) {
        Ok(g) => g,
        Err(e) => return fail(mode, "skill-list", &e),
    };
    match mode {
        OutputMode::Json => emit_json("skill-list", &groups, &[]),
        OutputMode::Human => println!("{}", render_skill_groups(&groups)),
    }
    EXIT_OK
}

/// DupState 的人话。五档同居 STATE 一列,行文要能并排读。
fn dup_state_label(state: DupState) -> &'static str {
    match state {
        // 不能跟着说 "identical":一份副本说「完全相同」是句胡话——和谁相同?
        // 读者会以为自己漏看了另一行,回头去数表格。
        DupState::Single => "only copy",
        DupState::Identical => "identical",
        DupState::Drifted => "drifted",
        // 软链:指向别处的实体,没有自己的内容;悬空则连目标都没了。
        DupState::Linked => "linked",
        DupState::Broken => "broken",
    }
}

/// skill 副本表 + 组内差异摘要 + 一行合计。整块返回而不是边算边印:
/// 这样「INSTALLED 列该不该出现」能被测试逐字核对。
fn render_skill_groups(groups: &[SkillGroup]) -> String {
    if groups.is_empty() {
        // 空不再是「没有重复」:表里装的是全部 skill,空意味着一个 skill 都
        // 没索引到。两件事读者的下一步动作完全不同——前者不用做什么,后者要去扫。
        return format!(
            "\n  {}",
            muted().apply_to(
                "No skill is indexed. Run `duster scan` first — if you just added one, run it again."
            )
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
            // 名字只在首行写——视觉上把一组连成一块。STATE 列不行：
            // 软链副本的 linked/broken 是这一行自己的事,放组头会把
            // 「坏的是哪一份」藏进第一行(悬空的那份未必排第一)。
            let name = if i == 0 {
                g.name.clone()
            } else {
                String::new()
            };
            let mut row = vec![
                name,
                dup_state_label(c.state).to_string(),
                c.agent_id.clone(),
                human_bytes(c.bytes),
            ];
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

    // 悬空软链的修法:删掉它。只给建议字符串,不做自动删除——用户目录里的
    // 东西,duster 只动自己建出来的。
    for g in groups.iter().filter(|g| g.state == DupState::Broken) {
        for c in g.copies.iter().filter(|c| c.state == DupState::Broken) {
            out.push_str(&format!(
                "\n\n  {} {}",
                style("broken").red().bold(),
                format!("rm {} (dangling symlink)", c.path.display())
            ));
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
    // 两个数必须一起报:表里装的是**全部** skill,只印「34 skills」会被读成
    // 「我有 34 个重复」,只印「13 in more than one place」又丢掉了「我一共有
    // 多少 skill」——那正是 list 的本职答案。
    //
    // 同一份 skill 也可能在一个 agent 里躺两处(目录名不同、声明的名字相同),
    // 所以说法是「不止一个地方」而不是「不止一个 agent」。
    // 「不止一个地方」按现场副本数算,不按状态:悬空的单条软链也是 Broken
    // 组,但它只在一个地方,算进 multi 等于把它说成"装了两处"。
    let multi = groups
        .iter()
        .filter(|g| g.copies.len() > 1)
        .count();
    out.push_str(&format!(
        "\n\n  {} {} {}",
        muted().apply_to("Total"),
        style(plural(groups.len(), "skill")).bold(),
        muted().apply_to(format!(
            "· {multi} in more than one place · {drifted} drifted · {} could be shared with `duster skill link`",
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

/// `duster skill rm`：删除一个 skill 的一份副本。全部业务在
/// [`duster_core::skill_ops::remove`]——软链只 unlink 不归档、真实目录
/// 归档后整棵删、删完清索引，都在那边。这里只翻旗标、渲染回执、映射退出码。
///
/// 一个 skill 名装在多家时 `--agent` 必选，同一家装了多份时 `--path` 再必选：
/// 缺了都报错列出候选，**绝不默认删全部**（见 core 的文档）。归档默认开；
/// `--no-archive` 是脚本用的显式关闭。`--dry-run` 只报将删什么。
///
/// `home` 只给测试注入：真实运行恒为 None（= 真实用户主目录）。
pub(crate) fn cmd_skill_rm(
    mode: OutputMode,
    index: Option<&Path>,
    home: Option<&Path>,
    name: &str,
    agent: Option<&str>,
    path: Option<&str>,
    archive: bool,
    dry_run: bool,
) -> i32 {
    let report = match skill_ops::remove(
        &DeleteOptions {
            index_path: index.map(Path::to_path_buf),
            home: home.map(Path::to_path_buf),
            archive,
            dry_run,
        },
        name,
        agent,
        path,
    ) {
        Ok(r) => r,
        Err(e) => return fail(mode, "skill-rm", &e),
    };
    match mode {
        OutputMode::Json => emit_json("skill-rm", &report, &report.warnings),
        OutputMode::Human => println!("{}", render_skill_rm(&report, dry_run)),
    }
    render_warnings(&report.warnings);
    if report.warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    }
}

/// 删除回执的人读排版。整块返回而不是边算边印：这样它能被测试逐字核对。
///
/// 干跑与真跑分行文：预览说 `would delete` 并把归档去处也预告出来；
/// 真跑说 `deleted` + 释放字节，归档包路径必须亮出来——那是用户唯一的退路。
fn render_skill_rm(report: &DeleteReport, dry_run: bool) -> String {
    let mut out = format!(
        "\n  {} {} {}\n",
        if dry_run { warn_mark() } else { ok_mark() },
        style(if dry_run {
            format!(
                "would delete {} · {}",
                crate::plural(report.removed.len(), "copy"),
                human_bytes(report.freed_bytes)
            )
        } else {
            format!(
                "deleted {} · {} freed",
                crate::plural(report.removed.len(), "copy"),
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
    // 四种收场各说各话。干跑里 `archived` 读作「将归档到哪」(目录,不是
    // 编造的包名),`None` 则说明这一批全是软链副本——它们只 unlink、没有
    // 自己的内容可归档,笼统说「会归档」就是假话。
    match (&report.archived, dry_run) {
        (Some(dir), true) => out.push_str(&format!(
            "  {} {}\n",
            muted().apply_to("would archive into"),
            accent().apply_to(display_tilde(dir))
        )),
        (Some(archived), false) => out.push_str(&format!(
            "  {} {}\n",
            muted().apply_to("archived to"),
            accent().apply_to(display_tilde(archived))
        )),
        (None, true) => out.push_str(&format!(
            "  {}\n",
            muted().apply_to(
                "nothing to archive — a symlink copy is only unlinked, its target is left alone"
            )
        )),
        (None, false) => out.push_str(&format!(
            "  {}\n",
            muted().apply_to("no archive was made (--no-archive, or a symlink copy that has nothing of its own)")
        )),
    }
    out
}

// ---------------------------------------------------------------------------
// doctor:duster 自己的自检
// ---------------------------------------------------------------------------

/// duster 自己的自检。
///
/// 不收任何旗标——上上轮的四个(`--no-secrets` / `--ping` / `--agent` /
/// `--check`)连同它们伺候的六项检查已经整体撤掉,status 也不再捎带它们;
/// 这一轮又把 status 的 issues / deep / ping 三个旗标一起删了。留在这里的
/// 是「duster 自己装好没有」:索引库、adapter 清单、home 目录权限、agent 的
/// SQLite 库读不读得动、版本。退出码同样回到纯 0/失败——发现是输出,
/// 不是失败:一份报出了问题的自检报告仍然是成功的诊断,脚本据此跑
/// `duster doctor && deploy` 不会被诊断结果挡住。
fn cmd_doctor(mode: OutputMode, index: Option<&Path>) -> i32 {
    let report = match self_check(index) {
        Ok(r) => r,
        Err(e) => return fail(mode, "doctor", &e),
    };
    match mode {
        OutputMode::Json => emit_json("doctor", &report, &report.warnings),
        OutputMode::Human => {
            println!("{}", render_doctor(&report));
            render_warnings(&report.warnings);
        }
    }
    EXIT_OK
}

/// 自检报告:先两行事实(版本、索引库在哪多大),再按检查分组,组内一条一段。
///
/// 分组顺序恒为 [`SELF_CHECKS`](即自检的执行顺序),不按发现数排——同一台
/// 机器两次运行的输出要能直接 diff。
///
/// **跑过却没发现的检查也占一行**(一句 `ok`)。这份报告的价值有一半在
/// "这一项我查过了",而一片空白既可能是"查过没事"也可能是"根本没查";
/// 让读者去猜,是这类工具最容易骗人的地方。
///
/// 整块返回而不是边算边印:上面那条规矩要能被测试逐字核对(与
/// [`render_skill_groups`] 同一个理由)。
fn render_doctor(report: &SelfReport) -> String {
    let mut out = format!(
        "\n  {} {}\n",
        muted().apply_to("duster"),
        style(&report.version).bold()
    );
    // 库的位置与体积单独占一行,而不是等出了问题才由某条 finding 捎出来:
    // 「库在哪、多大」是读者下一步要用的东西(去看它、去删它),而 `index-db`
    // 那一项的发现说的是「哪里不对」,两回事。
    out.push_str(&format!(
        "  {} {} {}\n",
        muted().apply_to("index"),
        report.index_path,
        muted().apply_to(match report.index_bytes {
            Some(bytes) => human_bytes(bytes),
            // None = 库还不存在。这不一定是错(`duster scan` 一跑就有了),
            // 该不该报成一条发现由 core 的 `index-db` 那一项定,这里只说事实。
            None => "not built yet".to_string(),
        })
    ));
    for check in SELF_CHECKS {
        if !report.checks_run.iter().any(|c| c == check) {
            continue;
        }
        out.push_str(&format!("\n  {}\n", accent().bold().apply_to(check)));
        let mut found = 0usize;
        for f in report.findings.iter().filter(|f| f.check == check) {
            out.push_str(&render_finding(f));
            found += 1;
        }
        if found == 0 {
            out.push_str(&format!("    {}\n", muted().apply_to("ok — nothing found")));
        }
    }
    out.push_str(&render_doctor_footer(report));
    out
}

/// 一条发现:`<严重度> <出处>`,说明缩进一层,有修法再来一行。
///
/// 出处**不截断**:它是文件路径、库路径或目录,用户要照着它去改东西,
/// 截掉的正是要复制的那一截。
///
/// 整块返回而不是边算边印:自检报告要能被测试逐字核对,而印出去的字节没有
/// 抓手。每行自带换行,调用方把若干条串起来就是一组。
fn render_finding(f: &Finding) -> String {
    let mut out = format!("    {} {}\n", severity_tag(f.severity), f.subject);
    out.push_str(&format!("      {}\n", muted().apply_to(&f.detail)));
    if let Some(fix) = &f.fix {
        out.push_str(&format!("      {} {fix}\n", muted().apply_to("fix:")));
    }
    out
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

/// 收尾:总数 + 严重度分布,再逐条说明**哪几项没跑**。
///
/// 后半截不是补充说明,是这份报告可信的前提:五项里跑了几项、剩下的为什么
/// 没跑,不写出来的话,一份"没发现问题"的报告和一份"什么都没查"的报告长得
/// 一模一样。
///
/// 这里不再有"你少给了个旗标"那一档——doctor 已经没有旗标了。五项自检全都
/// 无条件跑,一项没跑只能是它自己没跑起来,原因由 core 写进
/// [`SelfReport::warnings`],紧跟着这份报告印出来(见 [`cmd_doctor`]);
/// 一句光秃秃的 "not run" 会让读者以为是自己漏了个参数,而现在压根没有参数
/// 可漏,所以这句话的差事是把眼睛引到那条 warning 上去。
fn render_doctor_footer(report: &SelfReport) -> String {
    let count = |s: Severity| report.findings.iter().filter(|f| f.severity == s).count();
    let (errors, warns, infos) = (
        count(Severity::Error),
        count(Severity::Warn),
        count(Severity::Info),
    );
    // 前缀看的是**可行动的**严重度,不是有没有 findings。自检的 `version` 与
    // `adapters` 两项按设计恒发一条 info(「这一项我查过了」本身就是结论),
    // 拿 findings 非空当判据的话,一份一切正常的报告永远顶着一个 `!` ——
    // 三次之后没人再看那个感叹号,而它本该是唯一值得看的东西。
    let actionable = errors + warns;
    let breakdown: Vec<String> = [(errors, "error"), (warns, "warn"), (infos, "info")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, label)| format!("{n} {label}"))
        .collect();
    let checks = plural(report.checks_run.len(), "check");
    // info 的条数照旧报出来:它是「查过了」的凭据,`✔ nothing to fix` 少了它
    // 就和「什么都没查」难以分辨——这份报告一半的价值在后面那半句。
    let headline = if actionable == 0 {
        format!("nothing to fix across {checks}")
    } else {
        format!("{} across {checks}", plural(report.findings.len(), "finding"))
    };
    let tail = if breakdown.is_empty() {
        String::new()
    } else {
        format!("· {}", breakdown.join(" · "))
    };
    let mut out = format!(
        "\n  {} {} {}",
        if actionable == 0 {
            ok_mark().to_string()
        } else {
            warn_mark().to_string()
        },
        style(headline).bold(),
        muted().apply_to(tail)
    );
    for check in SELF_CHECKS {
        if report.checks_run.iter().any(|c| c == check) {
            continue;
        }
        out.push_str(&format!(
            "\n  {}",
            muted().apply_to(format!("{check}: not run (see the warning below)"))
        ));
    }
    out
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
        // 辅音 + y → -ies：`skill rm` 报的就是 copy 数,"2 copys" 是破绽。
        assert_eq!(plural(1, "copy"), "1 copy");
        assert_eq!(plural(2, "copy"), "2 copies");
        // 元音 + y 照旧只加 s。
        assert_eq!(plural(2, "day"), "2 days");
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

    /// 一份自检报告夹具。
    fn self_report(checks_run: &[&str], findings: Vec<Finding>) -> SelfReport {
        SelfReport {
            findings,
            checks_run: checks_run.iter().map(|c| (*c).to_string()).collect(),
            warnings: Vec::new(),
            version: "9.9.9".to_string(),
            index_path: "/tmp/duster/index.db".to_string(),
            index_bytes: Some(4096),
        }
    }

    /// 自检报告里**跑过却没发现的项也占一行**。一片空白既可能是「查过没事」
    /// 也可能是「根本没查」,而这份报告一半的价值就在「这一项我查过了」。
    #[test]
    fn 自检报告里跑过没发现的项也占一行() {
        let out = render_doctor(&self_report(
            &SELF_CHECKS,
            vec![f("adapters", Severity::Warn)],
        ));
        for check in SELF_CHECKS {
            assert!(out.contains(check), "{check} 该占一行: {out}");
        }
        // 全部跑过、一项有发现,其余各项各一句 ok。
        assert_eq!(
            out.matches("ok — nothing found").count(),
            SELF_CHECKS.len() - 1,
            "{out}"
        );
        // 两行事实:版本、库在哪多大。
        assert!(out.contains("9.9.9"), "{out}");
        assert!(out.contains("/tmp/duster/index.db"), "{out}");
        assert!(out.contains("4 KB"), "{out}");
        // 全跑过时不许出现「没跑」那一档。
        assert!(!out.contains("not run"), "{out}");

        // 没跑的项各占一行,而且说清去哪找原因:doctor 已经没有旗标可漏,
        // 一句光秃秃的 "not run" 只会让读者去翻 --help 找一个不存在的开关。
        let partial = render_doctor(&self_report(&["index-db"], Vec::new()));
        assert_eq!(
            partial.matches("not run").count(),
            SELF_CHECKS.len() - 1,
            "{partial}"
        );
        assert!(partial.contains("warning"), "{partial}");

        // 库还没建时说的是那句事实,不是一个 0 B。
        let mut fresh = self_report(&SELF_CHECKS, Vec::new());
        fresh.index_bytes = None;
        let out = render_doctor(&fresh);
        assert!(out.contains("not built yet"), "{out}");
        assert!(!out.contains("0 B"), "{out}");
    }

    /// doctor 不收旗标,status 也不再收——他检那六个连同它们的开关
    /// (`--no-secrets` / `--ping` / `--agent` / `--check`)早已搬走,这一轮
    /// 又把 status 自己的 issues / deep / ping 三个旗标一起删了。旧旗标
    /// 必须被 clap 拒掉而不是默默忽略:脚本里留着 `--deep` 却什么都不发生,
    /// 比报错难查得多。
    #[test]
    fn doctor_与_status_都不再收他检旗标() {
        assert!(Cli::try_parse_from(["duster", "doctor"]).is_ok());
        assert!(Cli::try_parse_from(["duster", "status"]).is_ok());
        for argv in [
            ["duster", "doctor", "--ping"].as_slice(),
            &["duster", "doctor", "--no-secrets"],
            &["duster", "doctor", "--agent", "codex"],
            &["duster", "doctor", "--check", "secrets"],
        ] {
            assert!(
                Cli::try_parse_from(argv.iter().copied()).is_err(),
                "{argv:?} 该被拒"
            );
        }
        // status 的旧旗标逐个钉死会被拒。旗标串在运行时拼出来,让"这个
        // 旗标不存在"这件事不靠源码里的字面量背书——老脚本里写什么,
        // 这里就拒什么。
        for name in ["issues", "deep", "ping"] {
            let flag = format!("--{name}");
            assert!(
                Cli::try_parse_from(["duster", "status", flag.as_str()]).is_err(),
                "duster status {flag} 该被拒"
            );
        }
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
    // skill list / open 双 id 空间(B2)
    // ---------------------------------------------------------------------

    /// `copies` 与 `dedupe` 都是历史动词,无过渡期:必须被 clap 拒掉,而不是
    /// 默默当 `list` 的别名用——脚本一旦开始依赖旧名,改名就白改了。
    /// `link` 不在这次改名里,照旧能叫。
    #[test]
    fn skill_命令树里只剩_list_与_link() {
        assert!(Cli::try_parse_from(["duster", "skill", "list"]).is_ok());
        assert!(Cli::try_parse_from(["duster", "skill", "copies"]).is_err());
        assert!(Cli::try_parse_from(["duster", "skill", "dedupe"]).is_err());
        assert!(
            Cli::try_parse_from(["duster", "skill", "link", "foo", "--from", "a", "--to", "b"])
                .is_ok()
        );
        // --json 的命令串随之改名,不许两头都能叫。
        assert!(Cli::try_parse_from(["duster", "skill", "list", "--json"]).is_ok());
    }

    /// 读索引的一次性命令一律先把库对齐磁盘,三条例外一条都不许多、不许少。
    ///
    /// `scan` 与 `doctor` 那两条是产品判断,不是优化:前者本身就是扫描,后者
    /// 自检的是 duster 自己、一个字节都不从 agent 索引里读(而且「库还没建」
    /// 正是它要报的一项,顺手建一次就把那条诊断抹掉了)。裸 `duster` 也不在
    /// 这里等——菜单自己把扫描扔进后台线程,同步等一遍正好把那件事作废。
    #[test]
    fn 只有读索引的一次性命令先刷新索引() {
        let needs = |argv: &[&str]| {
            let cli = Cli::try_parse_from(argv.iter().copied()).expect("这条 argv 该解析得动");
            needs_fresh_index(cli.command.as_ref())
        };
        for argv in [
            ["duster", "status"].as_slice(),
            &["duster", "search", "hello"],
            &["duster", "open", "t42"],
            &["duster", "clean"],
            &["duster", "prune", "--older-than", "30d"],
            &["duster", "uninstall", "qoder"],
            &["duster", "skill", "list"],
            &["duster", "memory", "list"],
            &["duster", "mcp", "list"],
            &["duster", "session", "list"],
        ] {
            assert!(needs(argv), "{argv:?} 读索引,该先刷新");
        }
        assert!(!needs(&["duster", "scan"]), "scan 本身就是扫描");
        assert!(!needs(&["duster", "doctor"]), "自检不读 agent 索引");
        assert!(!needs(&["duster", "diff", "a", "b"]), "diff 比路径,不读库");
        assert!(!needs(&["duster"]), "裸 duster 的菜单自己在后台扫");
    }

    /// 一份测试用的副本。install_bytes 由调用方给——「INSTALLED 列该不该
    /// 出现」只取决于它。state 是占位,`group()` 按组级结果统一改写。
    fn copy(agent: &str, path: &str, bytes: u64, install_bytes: u64) -> SkillCopy {
        SkillCopy {
            agent_id: agent.into(),
            path: PathBuf::from(path),
            state: DupState::Identical,
            link_target: None,
            tree_hash: "abc".into(),
            bytes,
            install_bytes,
        }
    }

    /// 状态跟着副本数走,与 `skill_ops::list` 同一条口径:一份是 `Single`,
    /// 多份默认 `Identical`。
    fn group(name: &str, copies: Vec<SkillCopy>) -> SkillGroup {
        let state = if copies.len() < 2 {
            DupState::Single
        } else {
            DupState::Identical
        };
        // 逐行状态跟随组级结果:STATE 列按副本渲染,夹具得和 `skill_ops::list`
        // 产出同形,否则渲染路径的测试测的是假数据。
        let copies: Vec<SkillCopy> = copies
            .into_iter()
            .map(|mut c| {
                c.state = state;
                c
            })
            .collect();
        SkillGroup {
            name: name.into(),
            state,
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

    /// 单份组不再被滤掉,表要容得下「一组一行」。状态列印的是 `only copy`,
    /// 不是 `identical`;组内差异摘要是 DRIFTED 专属,单份组一行都不该多出来
    /// (名字在整块里只出现一次)。
    #[test]
    fn 单份组渲染成一行且状态是_only_copy() {
        let out = render_skill_groups(&[group("solo", vec![copy("claude-code", "/a/x", 100, 0)])]);
        assert!(out.contains("only copy"), "{out}");
        assert!(!out.contains("identical"), "{out}");
        assert_eq!(out.matches("solo").count(), 1, "{out}");
    }

    /// 合计行必须两个数一起报:表里装的是全部 skill,只印总数会被读成
    /// 「我有 35 个重复」,只印重复数又丢了 list 的本职答案。
    #[test]
    fn 合计行同时报出总数与装在多处的个数() {
        let out = render_skill_groups(&[
            group("solo", vec![copy("claude-code", "/a/x", 100, 0)]),
            group(
                "twins",
                vec![
                    copy("claude-code", "/a/y", 100, 0),
                    copy("codex", "/b/y", 100, 0),
                ],
            ),
        ]);
        assert!(out.contains("2 skills"), "{out}");
        assert!(out.contains("1 in more than one place"), "{out}");
    }

    /// 空表不再意味着「没有重复」——表里装的是全部 skill,空就是一个 skill
    /// 都没索引到。两件事的下一步动作完全不同,文案不能共用。
    #[test]
    fn 空结果说的是没索引到_skill_而不是没有重复() {
        let out = render_skill_groups(&[]);
        assert!(out.contains("No skill is indexed"), "{out}");
        assert!(out.contains("duster scan"), "{out}");
        assert!(!out.contains("nothing to share"), "{out}");
    }

    /// codex 型会话夹具:假 home + 真扫描,与 cmd/session.rs 的测试同一套。
    /// 两场会话:一场 0 轮(只有 session_meta)、一场 1 轮。轮次 tid 是
    /// 从 1 起连续自增的,而 rid 也一样——单场有轮次的会话,它的 rid 几乎
    /// 必然撞上某个 tid(裸数字两档都命中会报错让用户消歧,见
    /// `open_两档撞车时报错并给出两条命令`),所以要留一场 0 轮会话
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
        .unwrap()
        .rows;
        assert_eq!(rows.len(), 2, "夹具里该有两场会话");
        // 挑一个「不是任何 tid」的 rid:裸数字按契约先查 turn,只有查无 tid
        // 才会落到会话 id,测试必须真的走上那条路。
        let rid = rows
            .iter()
            .map(|r| r.rid)
            .find(|&r| open_turn(Some(&index), r).is_err())
            .expect("夹具里该有一个不属于任何 tid 的会话 id");

        // s<rid> 只在会话空间查:会话渲染路径。
        match resolve_open(Some(&index), OpenId::Session(rid)).unwrap() {
            OpenTarget::Session(_) => {}
            other => panic!("{rid} 是会话 id,应解析成 Session: {other:?}"),
        }
        // t1 → 轮次渲染路径(全新库 tid 从 1 起)。
        match resolve_open(Some(&index), OpenId::Turn(1)).unwrap() {
            OpenTarget::Turn(t) => assert_eq!(t.role, "user"),
            other => panic!("tid 1 应解析成 Turn: {other:?}"),
        }
        // 两边都不是:报错要同时点名两个命令和各自的数字来源。
        let err = resolve_open(Some(&index), OpenId::Either(999_999)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("duster search"), "{msg}");
        assert!(msg.contains("duster session list"), "{msg}");

        // 整条命令走通:会话 id 返回成功,而不是落到报错退出码。
        assert_eq!(
            cmd_open(OutputMode::Human, Some(&index), OpenId::Session(rid), false),
            EXIT_OK
        );
        assert_eq!(
            cmd_open(
                OutputMode::Human,
                Some(&index),
                OpenId::Either(999_999),
                false
            ),
            EXIT_ERROR
        );
    }

    /// 解析器的形状契约:三种前缀、大小写、空串、非数字、负数、溢出,
    /// 全在纯函数里定死,IO 路径只消费结果。
    #[test]
    fn parse_open_id_形状() {
        assert_eq!(parse_open_id("t42"), Ok(OpenId::Turn(42)));
        assert_eq!(parse_open_id("s500"), Ok(OpenId::Session(500)));
        assert_eq!(parse_open_id("42"), Ok(OpenId::Either(42)));
        // 交互输入常常手滑带空格,收。
        assert_eq!(parse_open_id(" 42 "), Ok(OpenId::Either(42)));
        assert_eq!(parse_open_id(" t42 "), Ok(OpenId::Turn(42)));
        // 前缀只认小写,与 search / session list 印出来的一字不差。
        assert!(parse_open_id("T42").is_err());
        assert!(parse_open_id("S500").is_err());
        // 空串、空前缀、非数字、符号数字全在这里死掉。
        assert!(parse_open_id("").is_err());
        assert!(parse_open_id("  ").is_err());
        assert!(parse_open_id("abc").is_err());
        assert!(parse_open_id("t").is_err());
        assert!(parse_open_id("s").is_err());
        assert!(parse_open_id("t-1").is_err());
        assert!(parse_open_id("-1").is_err());
        assert!(parse_open_id("+1").is_err());
        assert!(parse_open_id("1_000").is_err());
        assert!(parse_open_id("t1x").is_err());
        assert!(parse_open_id("t1.5").is_err());
        // 溢出 i64:正常 id 只有 5~6 位,超长不是笔误就是攻击。
        assert!(parse_open_id("t99999999999999999999").is_err());
        assert!(parse_open_id("s99999999999999999999").is_err());
        assert!(parse_open_id("99999999999999999999").is_err());
        // 报错文案说清期望形状并给可直接照抄的例子。
        let msg = parse_open_id("abc").unwrap_err();
        assert!(msg.contains("t42"), "{msg}");
        assert!(msg.contains("s500"), "{msg}");
        let msg = parse_open_id("99999999999999999999").unwrap_err();
        assert!(msg.contains("out of range"), "{msg}");
    }

    /// 消歧:同一个数字在两个空间都存在时,`open` 不许自己挑一个——
    /// 必须报错,且报错里同时给出可直接粘贴的 t<n> 与 s<n> 两条命令。
    /// tid 与 rid 都是自增主键,数字迟早撞车,闷声返回轮次会静默吞掉
    /// 用户想开的会话。
    #[test]
    fn open_两档撞车时报错并给出两条命令() {
        let (_tmp, _home, index) = session_fixture();
        let rows = duster_core::session::list(
            Some(&index),
            &duster_core::session::SessionFilter::default(),
        )
        .unwrap()
        .rows;
        // 找一个 rid 同时也是某个 tid 的数字:夹具里 tid 从 1 起、rid 也
        // 从 1 起,几乎必然撞车;找不到就说明夹具变了,测试该红。
        let collision = rows
            .iter()
            .map(|r| r.rid)
            .find(|&r| open_turn(Some(&index), r).is_ok())
            .expect("夹具里该有一个同时是 tid 的会话 id");

        // 裸数字两档都命中:报错,且两条命令都在,让用户照着挑。
        let err = resolve_open(Some(&index), OpenId::Either(collision)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&format!("duster open t{collision}")),
            "{msg}"
        );
        assert!(
            msg.contains(&format!("duster open s{collision}")),
            "{msg}"
        );

        // 同一个数字标上前缀就锁定空间:t<n> 拿轮次、s<n> 拿会话,
        // 撞车不再影响任何一边。
        match resolve_open(Some(&index), OpenId::Turn(collision)).unwrap() {
            OpenTarget::Turn(_) => {}
            other => panic!("t{collision} 应解析成 Turn: {other:?}"),
        }
        match resolve_open(Some(&index), OpenId::Session(collision)).unwrap() {
            OpenTarget::Session(_) => {}
            other => panic!("s{collision} 应解析成 Session: {other:?}"),
        }
    }

    /// `t<n>` 只查轮次空间:查不到就报错,不退化去查会话——用户标了前缀,
    /// 想要的已经写明,替他去另一档找只会把打错的 id 静默变成「找到了
    /// 别的东西」。`s<n>` 同理。
    #[test]
    fn open_带前缀查空_不退化到另一空间() {
        let (_tmp, _home, index) = session_fixture();
        let rows = duster_core::session::list(
            Some(&index),
            &duster_core::session::SessionFilter::default(),
        )
        .unwrap()
        .rows;
        // 挑一个「是 rid 但不是 tid」的数字:同数字的会话明明存在,
        // t<n> 也必须报错而不是落到 Session。
        let rid_only = rows
            .iter()
            .map(|r| r.rid)
            .find(|&r| open_turn(Some(&index), r).is_err())
            .expect("夹具里该有一个不属于任何 tid 的会话 id");
        let err = resolve_open(Some(&index), OpenId::Turn(rid_only)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&format!("no turn with id t{rid_only}")), "{msg}");
        assert!(msg.contains("duster search"), "{msg}");
        assert!(!msg.contains("matches neither"), "{msg}");
    }

    /// search 的命中行:turn id 印成 `t42`(不是裸 `#42`),方便直接粘进
    /// `duster open`。行里带 CJK 也不歪:文件名截断与 snippet 高亮都按
    /// 显示宽度走,前缀只换了一格,没动其余部件的排版。
    #[test]
    fn search_命中行_turn_id_带_t_前缀() {
        let hits = [
            SearchHit {
                tid: 42,
                rid: 1,
                agent_id: "codex".into(),
                resource_path: "/tmp/rollout-2025-01-01-会话-aaaa.jsonl".into(),
                seq: 3,
                role: "user".into(),
                byte_off: 0,
                byte_len: 4,
                snippet: "你好 duster".into(),
                highlights: vec![(6, 12)],
            },
            SearchHit {
                tid: 103_753,
                rid: 2,
                agent_id: "claude-code".into(),
                resource_path: "/tmp/rollout-2025-01-02T00-00-00-bbbb.jsonl".into(),
                seq: 1,
                role: "assistant".into(),
                byte_off: 0,
                byte_len: 0,
                snippet: "run duster scan".into(),
                highlights: vec![],
            },
        ];
        let out = render_search_human(&hits, "duster");
        // 每行第一格是 t<tid>,没有残留的 # 前缀。
        assert!(out.contains("t42"), "{out}");
        assert!(out.contains("t103753"), "{out}");
        assert!(!out.contains("#42"), "{out}");
        assert!(!out.contains("#"), "不该再有 # 前缀:{out}");
        // 页脚给可直接粘贴的命令。
        assert!(out.contains("duster open t<id>"), "{out}");
        // CJK:会话文件名与 snippet 原样保留(截断与高亮没把字符切坏)。
        assert!(out.contains("会话"), "{out}");
        assert!(out.contains("你好 duster"), "{out}");
    }

    /// `skill rm` 的 CLI 接线:同名两家未指名报错列出候选且一份不删;
    /// 指名后只删那家;干跑不动盘。核心逻辑的逐字节断言在 duster-core 的
    /// skill_ops::remove 测试里,这里只钉外壳的接线与退出码。
    #[test]
    fn skill_rm_接线() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let index = home.join(".agent-duster/index.db");
        // 两个 agent 各一份 foo(内置清单覆盖 claude-code 与 codex)。
        for (rel, body) in [(".claude", "left body"), (".codex", "right body")] {
            let root = home.join(rel).join("skills").join("foo");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("SKILL.md"),
                format!("---\nname: foo\ndescription: fixture\n---\n\n{body}\n"),
            )
            .unwrap();
        }
        duster_core::scan::scan(&duster_core::scan::ScanOptions {
            home: Some(home.clone()),
            index_path: Some(index.clone()),
            full: false,
        })
        .expect("scan 夹具");

        // 未指名:报错列出候选,一份不删。
        let code = cmd_skill_rm(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            "foo",
            None,
            None,
            true,
            false,
        );
        assert_eq!(code, EXIT_ERROR, "多家未指名必须报错");
        assert!(home.join(".claude/skills/foo").is_dir());
        assert!(home.join(".codex/skills/foo").is_dir());

        // 指名 claude-code:只删那家,codex 分毫不动,归档包出现。
        let code = cmd_skill_rm(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            "foo",
            Some("claude-code"),
            None,
            true,
            false,
        );
        assert_eq!(code, EXIT_OK);
        assert!(!home.join(".claude/skills/foo").exists(), "被点名的那份要删");
        assert!(home.join(".codex/skills/foo").is_dir(), "没点名的那份不动");
        let archives: Vec<_> = std::fs::read_dir(home.join("agent-duster-exports"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "zst"))
            .collect();
        assert_eq!(archives.len(), 1, "真实内容必须先归档: {archives:?}");

        // 干跑:不归档、不删。
        let code = cmd_skill_rm(
            OutputMode::Human,
            Some(&index),
            Some(&home),
            "foo",
            Some("codex"),
            None,
            true,
            true,
        );
        assert_eq!(code, EXIT_OK);
        assert!(home.join(".codex/skills/foo").is_dir(), "预览不许删");
        assert_eq!(
            std::fs::read_dir(home.join("agent-duster-exports")).unwrap().count(),
            1,
            "预览不许新增归档包"
        );
    }
}
