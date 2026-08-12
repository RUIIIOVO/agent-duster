//! 交互式启动菜单:裸 `duster`(无子命令)时进入。
//!
//! - TTY 下:彩色 banner + 上下箭头选择命令,缺的参数当场补问。
//! - 非 TTY 或 `--json`:交互无意义,退化为打印帮助。
//! - banner 与菜单全部走 stderr,stdout 仍只放命令结果,与 output.rs 的
//!   stdout/stderr 契约一致。
//! - 每一张列表——菜单、参数选择、`browse` 的浏览表、勾选表——都是
//!   `prompt.rs` 里那两个自研控件([`prompt_pick`] / [`prompt_checklist`]);
//!   dialoguer 只剩它的 `ColorfulTheme` 在用(`?` / `✔` 这套视觉词汇)。
//!   理由见 [`menu_pick`] 与 [`prompt_pick`]:页高必须由知道自己在屏幕上
//!   印了几行的人来定,而 dialoguer 的控件从终端行数自己算。
//!
//! # 为什么是两层
//!
//! M2 之后 CLI 有二十多个子命令。一屏铺开二十多行不叫菜单,叫转储:
//! `mcp` 与 `session` 那两组里的五六条彼此只差一个名词,平铺在一起谁也扫不动。
//! 所以顶层按名词收口——动词照旧平铺,`session` / `memory` / `mcp` 三组各收成
//! 一条,选中进自己那一屏。到二层为止,再深就该去敲命令行了。
//!
//! # 两条不许破的规矩
//!
//! - **只发真实存在的参数组合**。每条路由都对着 clap 的定义抄;菜单里不许
//!   出现一个 `--help` 里没有的旗标,也不许替用户猜一个默认值。
//! - **危险动作照旧先出计划**。`clean` / `prune` / `uninstall` 走
//!   [`Consent::Ask`];`session prune` 与 `mcp sync` 用同一张勾选表
//!   ([`approve_prune_plan`] / `mcp_sync` 的目标勾选表)确认。
//!   顺序不能换——先看清单后表态才是有效确认。

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::CommandFactory;
use console::{Style, Term, style};
use dialoguer::theme::{ColorfulTheme, Theme};

use crate::prompt::{
    Checklist, Picker, prompt_checklist, prompt_confirm, prompt_line, prompt_pick,
};

use duster_core::mcp::{self, SyncOptions, SyncOutcome};
use duster_core::memory::{self, MemoryEntry};
use duster_core::plan::{Action, OLDER_THAN_PRESETS, Plan, PlanItem, parse_older_than};
use duster_core::search::{self, SearchFilter, SearchHit};
use duster_core::session::{self, SessionFilter, SessionRow};
use duster_core::skill_ops::{self, SkillGroup};
use duster_core::status::{AgentStatus, status};
use duster_fs::path::display_tilde;
use duster_model::CleanLevel;

use crate::browse::browse;
use crate::cmd;
use crate::output::{
    EXIT_CONFIRM_DENIED, EXIT_ERROR, EXIT_OK, EXIT_PARTIAL, OutputMode, Table, accent,
    display_width, human_bytes, muted, ok_mark, truncate_width, warn_mark, worse,
};
use crate::{
    Cli, Consent, DoctorArgs, UninstallArgs, cmd_clean, cmd_doctor, cmd_open, cmd_prune, cmd_scan,
    cmd_skill_link, cmd_status, cmd_uninstall, fail, render_warnings,
};

/// 菜单条目:命令名、一句话说明、名字的颜色(逐条不同,即「多彩」)、选中后的去向。
struct MenuItem {
    name: &'static str,
    desc: &'static str,
    color: fn(&Style) -> Style,
    route: Route,
}

/// 选中一条之后去哪。
#[derive(Clone, Copy)]
enum Route {
    /// 跑一条命令。需要补参数的,由这个函数自己问完再跑。
    Run(fn(OutputMode, Option<&Path>) -> i32),
    /// 进这个命令组自己的那一屏。
    Group(&'static Menu),
    /// 顶层是退出,子菜单是退回上一层。
    Leave,
}

/// 勾选表/单选菜单交给 `prompt_pick`/`prompt_checklist` 时,原语自己还要
/// 画 prompt 1 行、页脚 1 行,再留 1 行余量;表头那一行由控件内部再减
/// (`body_page`),调用方不要重复扣。
///
/// banner 与上一级回执**不算**在内:它们滚出屏幕是无害的,只要控件自己画的
/// 那一整块不超过终端高度,退场时 `clear_last_lines` 就能原样擦干净。
const CHECKLIST_RESERVED: usize = 3;

/// 一屏菜单:一句问话 + 一组条目。
struct Menu {
    prompt: &'static str,
    items: &'static [MenuItem],
}

/// 顶层那一屏。按**使用频率**排:扫描 / 体检 / 清理这三档是日常常客,靠上;
/// 会话 / 记忆 / 搜索是翻旧账,沉底。红色留给唯一会删光一个 agent 的那条。
///
/// `diff` 从这里撤了:它是通用文件/文件夹对比,和系统 `diff` 撞车——对比任意
/// 两个路径的工具用户手里已经满地都是,占掉一个顶层槽位不划算。引擎不删:
/// `Command::Diff` 在命令行原样保留,菜单里它仍然出现在有上下文的地方
/// (`skill copies` 的组详情里比两份副本,`mcp diff` 里比两家声明)。
static TOP: Menu = Menu {
    prompt: "Pick a command",
    items: &TOP_ITEMS,
};

static TOP_ITEMS: [MenuItem; 14] = [
    MenuItem {
        name: "scan",
        desc: "Find your agents and index what they store",
        color: |s| s.clone().green(),
        route: Route::Run(|m, i| cmd_scan(m, i, false)),
    },
    MenuItem {
        name: "scan --full",
        desc: "Same, but re-read every file from scratch",
        color: |s| s.clone().yellow(),
        route: Route::Run(|m, i| cmd_scan(m, i, true)),
    },
    MenuItem {
        name: "status",
        desc: "See which agent uses how much disk",
        color: |s| s.clone().blue(),
        route: Route::Run(cmd_status),
    },
    MenuItem {
        name: "doctor --secrets",
        desc: "Run all six checks, including plain-text API keys",
        color: |s| s.clone().blue(),
        route: Route::Run(run_doctor),
    },
    MenuItem {
        name: "clean",
        desc: "Reclaim caches, logs and SQLite slack",
        color: |s| s.clone().green(),
        route: Route::Run(prompt_clean),
    },
    MenuItem {
        name: "prune",
        desc: "Archive and remove what you stopped using",
        color: |s| s.clone().yellow(),
        route: Route::Run(prompt_prune),
    },
    // 危险动作只有这一个,红色留给它;quit 让位成白色。
    MenuItem {
        name: "uninstall",
        desc: "Remove everything one agent keeps on disk",
        color: |s| s.clone().red(),
        route: Route::Run(prompt_uninstall),
    },
    MenuItem {
        name: "mcp",
        desc: "See, compare and copy MCP server declarations",
        color: |s| s.clone().blue(),
        route: Route::Group(&MCP),
    },
    MenuItem {
        name: "skill copies",
        desc: "Find the same skill installed in more than one place",
        color: |s| s.clone().magenta(),
        route: Route::Run(skill_copies),
    },
    MenuItem {
        name: "session",
        desc: "Browse, export and shrink your conversations",
        color: |s| s.clone().cyan(),
        route: Route::Group(&SESSION),
    },
    MenuItem {
        name: "memory",
        desc: "Read what your agents remember about you",
        color: |s| s.clone().magenta(),
        route: Route::Group(&MEMORY),
    },
    MenuItem {
        name: "search",
        desc: "Search the text of past conversations",
        color: |s| s.clone().magenta(),
        route: Route::Run(prompt_search),
    },
    MenuItem {
        name: "open",
        desc: "Read one whole turn found by search",
        color: |s| s.clone().cyan(),
        route: Route::Run(prompt_open),
    },
    MenuItem {
        name: "quit",
        desc: "Leave",
        color: |s| s.clone().white(),
        route: Route::Leave,
    },
];

static SESSION: Menu = Menu {
    prompt: "Pick a session command",
    items: &SESSION_ITEMS,
};

static SESSION_ITEMS: [MenuItem; 5] = [
    MenuItem {
        name: "list",
        desc: "List conversations, most recently used first",
        color: |s| s.clone().cyan(),
        route: Route::Run(session_list),
    },
    MenuItem {
        name: "show",
        desc: "Print one whole conversation",
        color: |s| s.clone().cyan(),
        route: Route::Run(session_show),
    },
    MenuItem {
        name: "export",
        desc: "Write one conversation out as Markdown or JSON",
        color: |s| s.clone().green(),
        route: Route::Run(session_export),
    },
    MenuItem {
        name: "prune",
        desc: "Shrink conversations you stopped using (text is kept)",
        color: |s| s.clone().yellow(),
        route: Route::Run(session_prune),
    },
    MenuItem {
        name: "back",
        desc: "Back to the main menu",
        color: |s| s.clone().white(),
        route: Route::Leave,
    },
];

static MEMORY: Menu = Menu {
    prompt: "Pick a memory command",
    items: &MEMORY_ITEMS,
};

static MEMORY_ITEMS: [MenuItem; 3] = [
    MenuItem {
        name: "list",
        desc: "Show every memory your agents keep, and how big it is",
        color: |s| s.clone().magenta(),
        route: Route::Run(memory_list),
    },
    MenuItem {
        name: "show",
        desc: "Print one memory in full",
        color: |s| s.clone().cyan(),
        route: Route::Run(memory_show),
    },
    MenuItem {
        name: "back",
        desc: "Back to the main menu",
        color: |s| s.clone().white(),
        route: Route::Leave,
    },
];

static MCP: Menu = Menu {
    prompt: "Pick an mcp command",
    items: &MCP_ITEMS,
};

static MCP_ITEMS: [MenuItem; 6] = [
    MenuItem {
        name: "list",
        desc: "List every MCP server you have, merged across agents",
        color: |s| s.clone().blue(),
        route: Route::Run(mcp_list),
    },
    MenuItem {
        name: "show",
        desc: "Show one server in full, with credentials masked",
        color: |s| s.clone().cyan(),
        route: Route::Run(mcp_show),
    },
    MenuItem {
        name: "diff",
        desc: "Compare the same server as two agents declare it",
        color: |s| s.clone().cyan(),
        route: Route::Run(mcp_diff),
    },
    MenuItem {
        name: "sync",
        desc: "Copy one server's declaration into other agents",
        color: |s| s.clone().yellow(),
        route: Route::Run(mcp_sync),
    },
    MenuItem {
        name: "ping",
        desc: "Start each server once to see whether it still answers",
        color: |s| s.clone().green(),
        route: Route::Run(mcp_ping),
    },
    MenuItem {
        name: "back",
        desc: "Back to the main menu",
        color: |s| s.clone().white(),
        route: Route::Leave,
    },
];

/// 裸 `duster` 入口:TTY 进菜单循环,否则打印帮助。
pub fn run(mode: OutputMode, index: Option<&Path>) -> i32 {
    // --json 或任一端不是终端:打印帮助而不是挂起等待输入。
    if mode == OutputMode::Json
        || !std::io::stdin().is_terminal()
        || !std::io::stderr().is_terminal()
    {
        let _ = Cli::command().print_help();
        return EXIT_OK;
    }

    banner();
    run_menu(&TOP, mode, index)
}

/// 一屏菜单的循环。选中即执行,组进下一屏,`Leave`/Esc 退出这一屏,
/// 带着最后一条命令的退出码回去——裸 `duster` 的退出码是它跑过的最后一件事。
fn run_menu(menu: &Menu, mode: OutputMode, index: Option<&Path>) -> i32 {
    let items = rendered_items(menu.items);
    let mut last_code = EXIT_OK;
    loop {
        match menu_pick(menu.prompt, &items, 0) {
            Ok(Some(i)) => match menu.items[i].route {
                Route::Run(f) => {
                    eprintln!();
                    last_code = f(mode, index);
                    eprintln!();
                }
                Route::Group(sub) => {
                    eprintln!();
                    last_code = run_menu(sub, mode, index);
                }
                Route::Leave => return last_code,
            },
            // Esc / Ctrl-C:与 quit/back 同一个出口。
            Ok(None) => return last_code,
            Err(_) => return EXIT_ERROR,
        }
    }
}

/// 一屏单选菜单:问一次,回执一行,交出下标。
///
/// 全部菜单与 `browse` 走同一个控件([`prompt_pick`])。dialoguer 的 `Select`
/// 页高由它自己从终端行数算(`paging.rs`),banner 那四行与上一级的回执它
/// 都不知道——矮终端里十四行的顶层菜单会被顶出屏幕,而 ←/→ 在它手里是
/// 「翻页」,与 banner 上写的 `›` 是两码事,按下去会静静跳掉一整页。页高
/// 只能由知道自己印了几行的人来定。
///
/// 回执由这里印:控件退场时把自己画的行擦干净,「我选了哪一条」得留下。
/// `Ok(None)` = Esc,`Err(())` = 拿不到终端(非 TTY 走不到这里)。
fn menu_pick(prompt: &str, items: &[String], start: usize) -> Result<Option<usize>, ()> {
    let theme = menu_theme();
    let picked = prompt_pick(
        &theme,
        Picker {
            prompt,
            header: None,
            items,
            page: crate::browse::viewport(CHECKLIST_RESERVED),
            start,
        },
    )
    .map_err(|_| ())?;
    if let Some(i) = picked {
        let mut buf = String::new();
        theme
            .format_select_prompt_selection(&mut buf, prompt, &items[i])
            .expect("writing to a String cannot fail");
        eprintln!("{buf}");
    }
    Ok(picked)
}

/// 彩色 banner:名字 + 版本 + 一句话 + 操作提示,全部走 stderr。
fn banner() {
    eprintln!();
    eprintln!(
        "  {} {}",
        style("duster").cyan().bold(),
        style(concat!("v", env!("CARGO_PKG_VERSION"))).dim()
    );
    eprintln!(
        "  {}",
        style("See and clean up what your AI coding agents leave on disk").dim()
    );
    // `›` 是**行上的标记**(这一条是一组),不是一个键——写成「› = a group」
    // 而不是「› opens a group」:后者读起来像「按 → 进组」,而 ←/→ 是翻页。
    eprintln!(
        "  {}",
        style("↑/↓ move · ←/→ page · Enter run · › = a group · Esc back").dim()
    );
    eprintln!();
}

/// 菜单主题:条目自带颜色,把选中态的整行覆盖色关掉(否则会盖住条目色),
/// 只留高亮箭头前缀标记当前行。
fn menu_theme() -> ColorfulTheme {
    ColorfulTheme {
        active_item_style: Style::new(),
        ..ColorfulTheme::default()
    }
}

/// 预渲染菜单行:命令名各自着色、对齐,说明置灰。
///
/// 名字与说明之间那一列只有两种字符:组是 `›`,别的是空格。用户扫一眼
/// 就知道哪几条会当场跑、哪几条只是换一屏。
fn rendered_items(items: &[MenuItem]) -> Vec<String> {
    let width = items.iter().map(|m| m.name.len()).max().unwrap_or(0);
    items
        .iter()
        .map(|m| {
            let name = (m.color)(&Style::new().bold()).apply_to(format!("{:<width$}", m.name));
            let mark = if matches!(m.route, Route::Group(_)) {
                "›"
            } else {
                " "
            };
            format!("{name} {} {}", style(mark).dim(), style(m.desc).dim())
        })
        .collect()
}

/// 顶层的 `doctor --secrets`:菜单这一条就叫这个名字,所以只开凭据扫描那个
/// 旗标;其余五项不需要旗标,照跑。`--ping` 会起进程,菜单里不替用户决定
/// (要 ping 就去 `mcp ping`,那一条自己会问)。
fn run_doctor(mode: OutputMode, index: Option<&Path>) -> i32 {
    cmd_doctor(
        mode,
        index,
        DoctorArgs {
            secrets: true,
            ping: false,
            agents: Vec::new(),
            checks: Vec::new(),
        },
    )
}

// ---------------------------------------------------------------------------
// session / memory / mcp:M2 的三组(diff 已从菜单撤下,见顶层菜单的注释)
// ---------------------------------------------------------------------------

// `session list` / `session show` / `search` / `mcp ping` 的默认值与命令行
// 同一个常量,不再照抄。抄错的代价是菜单里印的行数与命令行不一样,不会更
// 危险,但一样是谎。

/// `session list` → 逐条浏览,选中即 `session show`(折叠视图,与命令行默认
/// 同档)。Esc 回菜单,什么都没发生——浏览是 TTY 上的增益,不是新契约。
fn session_list(mode: OutputMode, index: Option<&Path>) -> i32 {
    let agents = match prompt_agent(index) {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let rows = match session::list(
        index,
        &SessionFilter {
            agents,
            project: None,
            older_than_days: None,
            min_bytes: None,
            limit: cmd::session::DEFAULT_LIST_LIMIT,
            now_ms: None,
        },
    ) {
        Ok(r) => r,
        Err(e) => return fail(mode, "session-list", &e),
    };
    if rows.is_empty() {
        // 与 cmd::session::render_list 的措辞一致。
        println!();
        println!(
            "  {}",
            muted().apply_to(
                "No conversations matched. If that looks wrong, run `duster scan` first."
            )
        );
        return EXIT_OK;
    }
    let (header, labels) = session_list_rows(&rows);
    match browse("Which conversation", &header, &labels) {
        Some(i) => cmd::session::run(
            mode,
            index,
            cmd::session::SessionCmd::Show {
                rid: rows[i].rid,
                limit: cmd::session::DEFAULT_SHOW_LIMIT,
                full: false,
            },
        ),
        None => EXIT_OK,
    }
}

/// `session list` 的浏览行:列与 `cmd::session::render_list` 一致(ID / AGENT /
/// PROJECT / TURNS / SIZE / LAST USED),单元格口径照抄那一份——这里不是
/// 第二个真相,行文走散会让菜单和命令行对同一场会话给出两种读法。
fn session_list_rows(rows: &[SessionRow]) -> (String, Vec<String>) {
    const HEAD: [&str; 6] = ["ID", "AGENT", "PROJECT", "TURNS", "SIZE", "LAST USED"];
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            [
                r.rid.to_string(),
                r.agent_id.clone(),
                session_project_cell(r.cwd.as_deref()),
                r.turns.to_string(),
                session_size_cell(r.bytes, r.compressed),
                crate::relative_time(r.last_turn_ms),
            ]
        })
        .collect();
    let mut w = [0usize; 6];
    for (c, width) in w.iter_mut().enumerate() {
        *width = cells
            .iter()
            .map(|r| display_width(&r[c]))
            .chain(std::iter::once(HEAD[c].len()))
            .max()
            .unwrap_or(0);
    }
    // 行宽这笔账:`❯ ` 前缀 2 + 前五列定宽 + 五组列间各 2 空格共 10 + 尾部
    // 余量 1(见 [`row_budget`])。最后一列(LAST USED)吃余量:它天然就短
    // (「3d ago」这个量级),平时原样给,只有整行逼近终端宽时才截它——
    // 不设总宽上限的话,没有人算过整行,PROJECT 截到 28 也只是把问题
    // 挪到别处。
    let last_w = row_budget(2)
        .saturating_sub(10 + w[..5].iter().sum::<usize>())
        .max(4);
    let line = |r: &[String; 6]| {
        format!(
            "{:>w0$}  {:<w1$}  {:<w2$}  {:>w3$}  {:>w4$}  {}",
            r[0],
            r[1],
            r[2],
            r[3],
            r[4],
            truncate_width(&r[5], last_w),
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
            w4 = w[4],
        )
    };
    let header = line(&HEAD.map(str::to_string));
    (header, cells.iter().map(line).collect())
}

/// 项目列:cwd 的最后两段,与 `cmd::session::project_cell` 同口径。
fn session_project_cell(cwd: Option<&str>) -> String {
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

/// 体积列:已压缩的带 `zst` 标记,与 `cmd::session::size_cell` 同口径。
fn session_size_cell(bytes: u64, compressed: bool) -> String {
    if compressed {
        format!("{} zst", human_bytes(bytes))
    } else {
        human_bytes(bytes)
    }
}

/// `session show`:rid 来自上一屏那张表的 ID 列。
/// 菜单里没有 `--full` 这档(折叠 + 截断正是交互阅读要的形态),
/// 恒以折叠视图跑。
fn session_show(mode: OutputMode, index: Option<&Path>) -> i32 {
    let rid = match prompt_rid() {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd::session::run(
        mode,
        index,
        cmd::session::SessionCmd::Show {
            rid,
            limit: cmd::session::DEFAULT_SHOW_LIMIT,
            full: false,
        },
    )
}

/// `session export`:`out` 恒为 None,即写到 stdout。
/// 菜单里问一个文件路径等于让用户在提示符后面拼路径,而重定向是 shell 的活。
fn session_export(mode: OutputMode, index: Option<&Path>) -> i32 {
    let rid = match prompt_rid() {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let formats = [
        "markdown — headings and prose".to_string(),
        "json — one object".to_string(),
    ];
    let format = match menu_pick("Which format", &formats, 0) {
        Ok(Some(0)) => cmd::session::Format::Markdown,
        Ok(Some(_)) => cmd::session::Format::Json,
        // Esc:退回菜单,什么都没发生。
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd::session::run(
        mode,
        index,
        cmd::session::SessionCmd::Export {
            rid,
            format,
            out: None,
        },
    )
}

/// `session prune`:阈值、范围、归档三问,然后出计划、勾选确认、执行
/// (见 [`approve_prune_plan`] 与 [`cmd::session::run_prune_interactive`])。
/// `--export-first` 不问:归档那一步已经把内容打包带走了,再多一份
/// Markdown 副本是两笔账。
///
/// 早先这一条不走勾选表,只有「全做 / 全不做」两个答案,而且那条通用
/// 路径靠退出码 4 做控制流——两条罪状一起删了:同一张计划不该有两种
/// 权力,`clean` / `prune` 能留下一两条,会话与 MCP 也该能。所以这里
/// 也走勾选表:部分保留对全部清理由此成立。
fn session_prune(mode: OutputMode, index: Option<&Path>) -> i32 {
    let age = match prompt_age() {
        Ok(Some(a)) => a,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let agents = match prompt_agent(index) {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let archive = match prompt_yes_no("Pack a copy into ~/agent-duster-exports first?", true) {
        Ok(Some(b)) => b,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd::session::run_prune_interactive(mode, index, age, agents, archive)
}

/// `memory list` → 逐条浏览,选中即 `memory show`(key 是行尾那一串,原样递,
/// 不经过用户的手抄)。Esc 回菜单,什么都没发生。
fn memory_list(mode: OutputMode, index: Option<&Path>) -> i32 {
    let agents = match prompt_agent(index) {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let all = match memory::list(index, None) {
        Ok(l) => l,
        Err(e) => return fail(mode, "memory-list", &e),
    };
    let entries: Vec<&MemoryEntry> = all
        .entries
        .iter()
        .filter(|e| agents.is_empty() || agents.contains(&e.agent_id))
        .collect();
    // 有 warning 说明这张表不全:与命令行同一档退出码(3),提示照常上 stderr。
    let code = if all.warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    };
    render_warnings(&all.warnings);
    if entries.is_empty() {
        println!();
        println!(
            "  {}",
            muted().apply_to(cmd::memory::empty_list_message(&agents, &all.entries))
        );
        return code;
    }
    let (header, labels, keys) = cmd::memory::browse_rows(&entries);
    match browse("Which memory", &header, &labels) {
        // 下钻后不能把「这张表不全」的 warning 退出码吞掉。
        Some(i) => worse(
            code,
            cmd::memory::run(
                mode,
                index,
                &cmd::memory::MemoryCmd::Show {
                    key: keys[i].clone(),
                },
            ),
        ),
        None => code,
    }
}

/// `memory show`:key 是上一屏 PATH 列里的那一串,原样贴进来。
/// 贴错了由 `memory show` 自己给最接近的几条,菜单不重写一遍那套建议。
fn memory_show(mode: OutputMode, index: Option<&Path>) -> i32 {
    let key = match prompt_required("Key, exactly as `memory list` prints it") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd::memory::run(mode, index, &cmd::memory::MemoryCmd::Show { key })
}

/// `mcp list` → 逐条浏览,选中即 `mcp show`。Esc 回菜单,什么都没发生。
fn mcp_list(mode: OutputMode, index: Option<&Path>) -> i32 {
    let list = match mcp::list(index) {
        Ok(l) => l,
        Err(e) => return fail(mode, "mcp-list", &e),
    };
    let code = if list.warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    };
    render_warnings(&list.warnings);
    if list.servers.is_empty() {
        // 与 cmd::mcp 的 EMPTY_LIST 措辞一致。
        println!();
        println!(
            "  {}",
            muted().apply_to(
                "No MCP server is indexed. Run `duster scan` first — if you just added one, run it again."
            )
        );
        return code;
    }
    let (header, labels) = cmd::mcp::browse_rows(&list);
    match browse("Which server", &header, &labels) {
        // 下钻后不能把「这张表不全」的 warning 退出码吞掉。
        Some(i) => worse(
            code,
            cmd::mcp::run(
                mode,
                index,
                cmd::mcp::McpCmd::Show {
                    name: list.servers[i].name.clone(),
                },
            ),
        ),
        None => code,
    }
}

/// `mcp show`:名字来自 `mcp list` 的 NAME 列。
fn mcp_show(mode: OutputMode, index: Option<&Path>) -> i32 {
    let name = match prompt_required("Server name, as `mcp list` prints it") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd::mcp::run(mode, index, cmd::mcp::McpCmd::Show { name })
}

/// `mcp diff`:两边的 agent 都必须点名——`mcp list` 的冲突块里印的就是这两个。
fn mcp_diff(mode: OutputMode, index: Option<&Path>) -> i32 {
    let name = match prompt_required("Server name, as `mcp list` prints it") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let from = match prompt_required("Agent on the left-hand side") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let to = match prompt_required("Agent on the right-hand side") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd::mcp::run(mode, index, cmd::mcp::McpCmd::Diff { name, from, to })
}

/// `mcp sync`:这一组里唯一会写别人配置文件的动作,所以走两段式——
/// `plan_sync` 先出计划(一个字节都不写),目标勾选表确认后 `apply_sync`
/// 只对勾过的目标动手。
///
/// `--from` 留空即不带这个旗标:只有一个 agent 声明它时 core 自己认得出来,
/// 多于一个才会要求点名,那句报错比菜单在这里瞎猜一个来源有用得多。
///
/// 早先这一条不走勾选表,只有「全做 / 全不做」两个答案,而且那条通用
/// 路径靠退出码 4 做控制流——两条罪状一起删了:同一张计划不该有两种
/// 权力,`clean` / `prune` 能留下一两条,会话与 MCP 也该能。所以这里
/// 目标也走勾选表:勾几个就写几个。
fn mcp_sync(mode: OutputMode, index: Option<&Path>) -> i32 {
    let name = match prompt_required("Server name, as `mcp list` prints it") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let from = match prompt_optional("Copy from which agent (leave empty to let duster pick)") {
        Ok(LineOutcome::Value(v)) => Some(v),
        Ok(LineOutcome::Empty) => None,
        Ok(LineOutcome::Esc) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let raw = match prompt_required("Copy into which agents, comma separated") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    // 与 clap 的 `value_delimiter = ','` 同一把切法。
    let to: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if to.is_empty() {
        eprintln!();
        eprintln!("  {}", style("No target agent — nothing to do.").dim());
        return EXIT_OK;
    }

    // 计划阶段:只出计划,一个字节都不写(`dry_run: true` / `yes: false`)。
    let plan_opts = SyncOptions {
        index_path: index.map(Path::to_path_buf),
        home: None,
        name,
        from,
        to,
        dry_run: true,
        yes: false,
    };
    let plan = match mcp::plan_sync(&plan_opts) {
        Ok(p) => p,
        Err(e) => return fail(mode, "mcp-sync", &e),
    };
    // 计划先打全:复用命令行那份渲染器,含「一个字节都没写」的明说。
    println!();
    println!("{}", cmd::mcp::render_sync(&plan.targets, true));
    if plan.targets.is_empty() {
        // 渲染器已经印了 "No target left to write to.",这里只落退出码:
        // 什么都没动(4),与命令行无 `--yes` 的收场一致。
        return EXIT_CONFIRM_DENIED;
    }

    // 目标勾选表:一行一个目标(agent · 动作 · 去处),默认全勾——用户
    // 取消掉不想写的那几个,而不是从零勾。
    let labels = sync_target_rows(&plan.targets);
    let picked = match prompt_checklist(
        &menu_theme(),
        Checklist {
            prompt: "Space toggles · a all/none · Enter syncs the checked · Esc cancels",
            header: None,
            items: &labels,
            checked: vec![true; plan.targets.len()],
            select_all: false,
            page: crate::browse::viewport(CHECKLIST_RESERVED),
        },
    ) {
        Ok(Some(p)) => p,
        Ok(None) => return EXIT_OK, // Esc:什么都没发生,与其余问句一致
        Err(_) => return EXIT_ERROR,
    };
    if picked.is_empty() {
        // 一个不勾 = 什么都没动,与其余勾选表同一档退出码(4)。
        eprintln!();
        eprintln!(
            "  {}",
            muted().apply_to("Nothing selected — nothing was written.")
        );
        return EXIT_CONFIRM_DENIED;
    }

    // 执行阶段:勾选表就是那份同意(`dry_run: false` / `yes: true`),
    // 只对勾过的 agent 真写盘(`allow` = 勾选行的 agent_id)。
    let apply_opts = SyncOptions {
        dry_run: false,
        yes: true,
        ..plan_opts
    };
    let allow_ids: Vec<String> = picked.iter().map(|&i| plan.targets[i].agent_id.clone()).collect();
    let outcomes = match mcp::apply_sync(&apply_opts, &plan, Some(&allow_ids)) {
        Ok(o) => o,
        Err(e) => return fail(mode, "mcp-sync", &e),
    };
    println!();
    println!("{}", cmd::mcp::render_sync(&outcomes, false));
    cmd::mcp::sync_exit(false, &outcomes)
}

/// 目标勾选表的行:`agent · action · path`。前两列按**本批**数据定宽
/// (用 `display_width` 量,与 [`checklist_rows`] 同一套量法——宽字符不会
/// 顶歪下一列),路径列吃掉余量、截断给出——整行宽有上限,路径不截的话
/// 80 列的终端上一行就是 90 列,必然折行(见 [`row_budget`])。
fn sync_target_rows(plan: &[SyncOutcome]) -> Vec<String> {
    sync_target_rows_at(plan, term_width())
}

/// 宽度算法本体;`cols` 由测试直接给(60 / 80 / 200),不必 mock 终端。
fn sync_target_rows_at(plan: &[SyncOutcome], cols: usize) -> Vec<String> {
    let cells: Vec<[String; 3]> = plan
        .iter()
        .map(|o| {
            [
                o.agent_id.clone(),
                o.action.clone(),
                display_tilde(Path::new(&o.path)),
            ]
        })
        .collect();
    let mut w = [0usize; 2];
    for (c, width) in w.iter_mut().enumerate() {
        *width = cells
            .iter()
            .map(|r| display_width(&r[c]))
            .max()
            .unwrap_or(0);
    }
    // 行宽这笔账:`❯ [x] ` 前缀 6 + 两列定宽 + 两个 ` · ` 分隔符共 6 +
    // 尾部余量 1。路径列只能拿余量(与 [`checklist_rows`] 同一套量法),
    // 下限 16 是「还能认出是哪个目录」的最窄宽度。
    let path_w = row_budget_at(cols, 6)
        .saturating_sub(6 + w[0] + w[1])
        .max(16);
    cells
        .iter()
        .map(|r| {
            format!(
                "{:<w0$} · {:<w1$} · {}",
                r[0],
                r[1],
                truncate_width(&r[2], path_w),
                w0 = w[0],
                w1 = w[1]
            )
        })
        .collect()
}

/// `mcp ping`:留空即全部试一遍。会起进程,所以名字这一问不能替用户跳过。
fn mcp_ping(mode: OutputMode, index: Option<&Path>) -> i32 {
    let name = match prompt_optional("Only this server (leave empty to try all)") {
        Ok(LineOutcome::Value(v)) => Some(v),
        Ok(LineOutcome::Empty) => None,
        Ok(LineOutcome::Esc) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd::mcp::run(
        mode,
        index,
        cmd::mcp::McpCmd::Ping {
            name,
            timeout_ms: cmd::mcp::DEFAULT_PING_TIMEOUT_MS,
        },
    )
}

// ---------------------------------------------------------------------------
// skill copies:组列表 → 组详情(副本表 + link / 对比)
// ---------------------------------------------------------------------------

/// `skill copies` → 组列表逐条浏览,选中进组详情。Esc 回菜单,什么都没发生。
fn skill_copies(mode: OutputMode, index: Option<&Path>) -> i32 {
    let groups = match skill_ops::copies(index) {
        Ok(g) => g,
        Err(e) => return fail(mode, "skill-copies", &e),
    };
    if groups.is_empty() {
        // 与 cmd_skill_copies 的渲染措辞一致。
        println!();
        println!(
            "  {}",
            muted().apply_to("Every skill lives in exactly one agent — nothing to share.")
        );
        return EXIT_OK;
    }
    let (header, labels) = skill_group_rows(&groups);
    match browse("Which skill", &header, &labels) {
        Some(i) => skill_group_detail(mode, index, &groups[i]),
        None => EXIT_OK,
    }
}

/// 组列表的浏览行:名字 + 状态 + 副本数。选中进详情,细节都在那一屏。
/// 第三列(副本数)吃余量,整行宽有 [`row_budget`] 上限。
fn skill_group_rows(groups: &[SkillGroup]) -> (String, Vec<String>) {
    const HEAD: [&str; 3] = ["SKILL", "STATE", "COPIES"];
    let cells: Vec<[String; 3]> = groups
        .iter()
        .map(|g| {
            [
                g.name.clone(),
                crate::dup_state_label(g.state).to_string(),
                // 只给数字:`copy` 是不规则复数,`plural` 只会加 `s`,
                // "2 copys" 这种小破绽会让人连带怀疑其余数字。
                g.copies.len().to_string(),
            ]
        })
        .collect();
    let mut w = [0usize; 3];
    for (c, width) in w.iter_mut().enumerate() {
        *width = cells
            .iter()
            .map(|r| display_width(&r[c]))
            .chain(std::iter::once(HEAD[c].len()))
            .max()
            .unwrap_or(0);
    }
    // 行宽这笔账:`❯ ` 前缀 2 + 三列定宽 + 列间各 2 空格共 4 + 尾部余量 1。
    // 最后一列(COPIES)吃余量:它是纯数字列、天然就窄,平时按内容宽,
    // 只有 skill 名把行顶到终端宽附近时才截它——副本数没人读得出 5 位。
    let last_w = row_budget(2)
        .saturating_sub(4 + w[0] + w[1])
        .max(4);
    w[2] = w[2].min(last_w);
    let line = |r: &[String; 3]| {
        format!(
            "{:<w0$}  {:<w1$}  {:>w2$}",
            r[0],
            r[1],
            truncate_width(&r[2], w[2]),
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
        )
    };
    let header = line(&HEAD.map(str::to_string));
    (header, cells.iter().map(line).collect())
}

/// 一组 skill 的详情:副本表(agent / 内容 / install / 路径)+ 三个动作。
///
/// 顶层 `diff` 已从菜单撤下(见顶层菜单的注释),通用文件对比交给系统 `diff`;
/// 这一屏是 diff 引擎在菜单里剩下的两个有上下文的落脚点之一
/// (另一个是 `mcp diff`),所以「对比两份副本」必须在这里。
fn skill_group_detail(mode: OutputMode, index: Option<&Path>, g: &SkillGroup) -> i32 {
    // INSTALLED 只在真有编译产物时出一列——与 `render_skill_groups` 同一条
    // 规矩:一整列 0 B 既占掉 PATH 要的宽度,又让人以为那里有意义可读。
    let show_install = g.copies.iter().any(|c| c.install_bytes > 0);
    let mut head = vec!["AGENT", "CONTENT"];
    if show_install {
        head.push("INSTALLED");
    }
    head.push("PATH");
    let path_col = head.len() - 1;
    let mut t = Table::new(head);
    t.color_col(0, accent());
    if show_install {
        t.right_align(&[1, 2]);
    } else {
        t.right_align(&[1]);
    }
    t.color_col(path_col, muted());
    for c in &g.copies {
        let mut row = vec![c.agent_id.clone(), human_bytes(c.bytes)];
        if show_install {
            row.push(human_bytes(c.install_bytes));
        }
        row.push(c.path.display().to_string());
        t.push_row(row);
    }
    // 路径列吃终端余量:这一屏是 TTY 上的阅读屏,超宽就截;要完整路径走
    // `duster skill copies --json`,那里永不截断。
    t.flex_col(path_col);

    println!();
    println!("  {}", accent().bold().apply_to(&g.name));
    println!("  {}", muted().apply_to(crate::dup_state_label(g.state)));
    println!("{}", t.render());
    // DRIFTED 的差异摘要:改了哪儿是 DRIFTED 唯一有用的信息,塞进表格必被截断。
    if let Some(diff) = &g.diff {
        println!();
        println!("  {}", muted().apply_to("What drifted:"));
        for line in diff.lines() {
            println!("    {line}");
        }
    }

    // 动作。默认落在 back——回车不该触发写操作(link 会动目标 agent 的磁盘)。
    let actions = [
        "link this skill into another agent (one copy on disk)".to_string(),
        "compare two copies".to_string(),
        "back".to_string(),
    ];
    match menu_pick(&format!("{}: what next", g.name), &actions, 2) {
        Ok(Some(0)) => skill_link_action(mode, index, g),
        Ok(Some(1)) => skill_compare_action(mode, index, g),
        // Esc 与 back 同路:回菜单。
        _ => EXIT_OK,
    }
}

/// link 动作:选一个源副本,再选目标 agent,然后跑 `skill link`——与命令行
/// 同一份实现(含链接后实测与失败回滚),菜单只负责把两个名字问出来。
fn skill_link_action(mode: OutputMode, index: Option<&Path>, g: &SkillGroup) -> i32 {
    let from = match pick_copy(g, "Copy to share") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let to = match pick_target_agent(index, &from) {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd_skill_link(mode, index, &g.name, &from, &to, false)
}

/// 对比动作:选两份副本,跑通用 diff 引擎(`duster diff` / `mcp diff` 共用
/// 那一份渲染器,`-`/`+` 的读法全项目一致)。
fn skill_compare_action(mode: OutputMode, _index: Option<&Path>, g: &SkillGroup) -> i32 {
    let Some((a_idx, b_idx)) = pick_copy_pair(g) else {
        return EXIT_OK;
    };
    cmd::diff::run(
        mode,
        &cmd::diff::DiffArgs {
            a: g.copies[a_idx].path.clone(),
            b: g.copies[b_idx].path.clone(),
            include_same: false,
            no_line_level: false,
        },
    )
}

/// 单选一个副本(link 的源)。组内副本的行就在上面的详情表里,这里只选 agent。
fn pick_copy(g: &SkillGroup, prompt: &str) -> Result<Option<String>, ()> {
    let rows: Vec<String> = g.copies.iter().map(|c| c.agent_id.clone()).collect();
    match menu_pick(prompt, &rows, 0)? {
        Some(i) => Ok(Some(rows[i].clone())),
        None => Ok(None),
    }
}

/// 选目标 agent(link 的 to)。候选来自索引(status 的聚合,排除源);
/// 索引缺失就退回自由输入——自由输入总能给出一条路。
fn pick_target_agent(index: Option<&Path>, exclude: &str) -> Result<Option<String>, ()> {
    let agents: Vec<String> = agent_choices(index)
        .map(|a| {
            a.into_iter()
                .filter(|a| a.agent_id != exclude)
                .map(|a| a.agent_id)
                .collect()
        })
        .unwrap_or_default();
    if agents.is_empty() {
        return prompt_required("Target agent id");
    }
    match menu_pick("Which agent should get it", &agents, 0)? {
        Some(i) => Ok(Some(agents[i].clone())),
        None => Ok(None),
    }
}

/// 选两份不同的副本,返回它们在 `g.copies` 里的下标。
fn pick_copy_pair(g: &SkillGroup) -> Option<(usize, usize)> {
    let rows: Vec<String> = g.copies.iter().map(|c| c.agent_id.clone()).collect();
    let a = match menu_pick("Left side", &rows, 0) {
        Ok(Some(i)) => i,
        Ok(None) | Err(()) => return None,
    };
    let rest: Vec<usize> = (0..rows.len()).filter(|&i| i != a).collect();
    let rest_rows: Vec<String> = rest.iter().map(|&i| rows[i].clone()).collect();
    let b = match menu_pick("Right side", &rest_rows, 0) {
        Ok(Some(j)) => j,
        Ok(None) | Err(()) => return None,
    };
    Some((a, rest[b]))
}

/// 交互式 clean:只需要范围,清单打完当场确认。
fn prompt_clean(mode: OutputMode, index: Option<&Path>) -> i32 {
    match prompt_agent(index) {
        Ok(Some(agents)) => cmd_clean(mode, index, agents, Consent::Ask),
        Ok(None) => EXIT_OK,
        Err(()) => EXIT_ERROR,
    }
}

/// 交互式 prune:阈值问句见 [`prompt_age`],范围见 [`prompt_agent`]。
///
/// 归档在这里必须当场表态:未表态且预估超阈值时 core 会拒绝执行,
/// 而交互模式里"再去敲一遍带 --archive 的命令"是最没道理的收场。
fn prompt_prune(mode: OutputMode, index: Option<&Path>) -> i32 {
    let age = match prompt_age() {
        Ok(Some(a)) => a,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let agents = match prompt_agent(index) {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let archive = match prompt_yes_no("Pack a copy into ~/agent-duster-exports first?", true) {
        Ok(Some(b)) => Some(b),
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    // 代际超编是「按数量冗余」而不是「放久了」:不看 --older-than,所以
    // 单独一问,并把会删什么讲成人话——数据库备份这类保留最新 N 份的
    // 资源,第 N+1 份起会被一并清掉。默认关,与命令行旗标默认一致。
    let keep_generations = match prompt_yes_no(
        "Also prune surplus backups — resources that keep only the newest N copies, like database backups?",
        false,
    ) {
        Ok(Some(b)) => b,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd_prune(
        mode,
        index,
        agents,
        Some(age),
        archive,
        Consent::Ask,
        keep_generations,
    )
}

/// 交互式 uninstall:只问目标,逐字确认在计划打完之后
/// (见 [`confirm_uninstall`])。顺序不能换——先看清单再确认才是有效确认。
fn prompt_uninstall(mode: OutputMode, index: Option<&Path>) -> i32 {
    let agent = match prompt_required("Which agent to remove (its id, as `status` prints it)") {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    cmd_uninstall(
        mode,
        index,
        UninstallArgs {
            agent,
            // 菜单用户加不上旗标,所以给他完整的那个卸载:清单打全(包括要动
            // 别人家哪一个键),再逐字收确认。留一条指向已删程序的 MCP 声明
            // 不叫卸载干净。
            data_only: false,
            // 确认串留空:计划打完后由 confirm_uninstall 当场逐字收。
            confirm: None,
            // 交互模式默认先导出:菜单用户不会想到自己该加旗标。
            export_first: true,
            keep: Vec::new(),
            archive: Some(true),
            // 代跑包管理器只能来自一个显式的旗标,菜单里不提供。
            run_package_manager: false,
        },
        Consent::Ask,
    )
}

// ---------------------------------------------------------------------------
// 问句
// ---------------------------------------------------------------------------
//
// 同一件事只有一句问法:范围、阈值、y/N、必填与选填的一行文本各一个函数。
// 两个入口问同一件事却措辞不同,用户会以为它们问的不是一回事。

/// 范围:多选 agent。`Ok(Some(vec))`,空 vec = 全部;`Ok(None)` = Esc 或者
/// 一个都没勾,调用方原样退回菜单,什么都没发生。
///
/// 第一行固定是 `All agents`,其后每一行来自真实索引(`status` 的聚合),
/// 带着「总量 / 可回收量」两个数字——用户不用凭记忆敲 agent id,看体积
/// 就能决定谁值得清。
///
/// **默认全勾**,用户取消掉不想动的那几个——与计划勾选表同一个方向
/// (见 [`approve_checklist`]):这一屏问的是范围,而命令行不带 `--agent`
/// 就是全部,所以"全部"才是这屏的默认答案。勾 `All agents` 会真的把
/// 每一行都勾上(见 [`prompt_checklist`] 的 `select_all`);它勾满时提交,
/// 与不带 `--agent` 同义。
///
/// 一个都不勾**不再等于全部**。早先那版把"空 = 全部"这层等价藏在代码里:
/// 屏幕上十一行全是空框、`All agents` 也没勾,回车却清了所有 agent。
/// 一张说了谎的勾选表比没有它更糟,所以现在空勾选就是「什么都没选」,
/// 说一句然后退回菜单。
///
/// 多选能成立,是因为命令行那头 `--agent a,b` 是真实组合(clap 的
/// `value_delimiter`,见 main.rs 的旗标定义)——菜单只发 CLI 表达得出的
/// 参数组合,这条规矩写在模块文档里。
///
/// 索引缺失或一个 agent 都没有时退回旧的行内输入:一屏没有任何可选项的
/// 菜单是死胡同,而自由输入总能给出一条路。
fn prompt_agent(index: Option<&Path>) -> Result<Option<Vec<String>>, ()> {
    let Some(agents) = agent_choices(index) else {
        return match prompt_optional("Only this agent (leave empty for all)") {
            Ok(LineOutcome::Value(v)) => Ok(Some(vec![v])),
            Ok(LineOutcome::Empty) => Ok(Some(Vec::new())),
            Ok(LineOutcome::Esc) => Ok(None),
            Err(()) => Err(()),
        };
    };

    let mut items: Vec<String> = Vec::with_capacity(agents.len() + 1);
    items.push("All agents".to_string());
    items.extend(agent_choice_rows(&agents));
    let checked = vec![true; items.len()];
    let picked = match prompt_checklist(
        &menu_theme(),
        Checklist {
            prompt: "Which agents · Space toggles · a all/none · Enter confirms · Esc cancels",
            header: None,
            items: &items,
            checked,
            select_all: true,
            // 页高按终端算,和 browse 同一把尺子:写死 10 会让 11 个 agent 在
            // 一个 40 行的终端里也被切成两页。
            page: crate::browse::viewport(CHECKLIST_RESERVED),
        },
    ) {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(None), // Esc:回菜单,什么都没发生
        Err(_) => return Err(()),
    };

    // 一个都没勾:不猜"那就全部吧"。这一屏默认全勾,取消到空是个明确的
    // 动作,唯一诚实的读法是「先不清了」。
    if picked.is_empty() {
        eprintln!(
            "  {}",
            muted().apply_to("no agents checked · nothing to do")
        );
        return Ok(None);
    }
    // 勾满(含 `All agents` 行)= 全部,与命令行不带 `--agent` 同义。
    if picked.contains(&0) {
        eprintln!("  {} {}", ok_mark(), muted().apply_to("agents · all"));
        return Ok(Some(Vec::new()));
    }
    let chosen: Vec<String> = picked
        .iter()
        .map(|&i| agents[i - 1].agent_id.clone())
        .collect();
    eprintln!(
        "  {} {}",
        ok_mark(),
        muted().apply_to(format!("agents · {}", chosen.join(", ")))
    );
    Ok(Some(chosen))
}

/// 从索引读 agent 名单。索引缺失或一个 agent 都没有 = None,
/// 调用方据此退回自由输入。
fn agent_choices(index: Option<&Path>) -> Option<Vec<AgentStatus>> {
    let report = status(index).ok()?;
    if report.agents.is_empty() {
        return None;
    }
    Some(report.agents)
}

/// 多选行的正文:agent id、总量、可回收量。前两列定宽、字节数右对齐,
/// 列宽按**本批**数据量,不写死——与 [`checklist_rows`] 同一套量法
/// (`display_width`,宽字符不会顶歪下一列)。第三列(可回收量)吃余量:
/// 前两列把行顶到终端宽附近时先截它,整行宽不越过 [`row_budget`]。
fn agent_choice_rows(agents: &[AgentStatus]) -> Vec<String> {
    let cells: Vec<[String; 3]> = agents
        .iter()
        .map(|a| {
            [
                a.agent_id.clone(),
                human_bytes(a.bytes),
                human_bytes(a.clean_bytes.values().sum()),
            ]
        })
        .collect();
    let mut w = [0usize; 3];
    for (c, width) in w.iter_mut().enumerate() {
        *width = cells
            .iter()
            .map(|r| display_width(&r[c]))
            .max()
            .unwrap_or(0);
    }
    // 行宽这笔账:`❯ [x] ` 前缀 6 + 三列定宽 + 列间空格 2 + 尾部固定后缀
    // 「 reclaimable」12(空格 1 + 单词 11)+ 尾部余量 1。最后一列(可回收
    // 量)吃余量:平时按内容宽,定宽列逼近终端宽时才截它。
    let last_w = row_budget(6)
        .saturating_sub(14 + w[0] + w[1])
        .max(8);
    w[2] = w[2].min(last_w);
    cells
        .iter()
        .map(|r| {
            format!(
                "{:<w0$} {:>w1$} {:>w2$} reclaimable",
                r[0],
                r[1],
                truncate_width(&r[2], w[2]),
                w0 = w[0],
                w1 = w[1],
                w2 = w[2],
            )
        })
        .collect()
}

/// 陈旧阈值。三档来自 [`OLDER_THAN_PRESETS`](duster_core::plan::OLDER_THAN_PRESETS),
/// 自定义输入过 `parse_older_than` 校验(和 `--older-than` 同一把尺子)。
///
/// `Ok(None)` = Esc,调用方原样退回菜单,什么都没发生。
fn prompt_age() -> Result<Option<String>, ()> {
    let theme = ColorfulTheme::default();
    let mut choices: Vec<String> = OLDER_THAN_PRESETS
        .iter()
        .map(|d| format!("older than {d} days"))
        .collect();
    choices.push("custom…".to_string());
    let picked = match menu_pick("How old is old enough", &choices, 0) {
        Ok(Some(i)) => i,
        Ok(None) => return Ok(None),
        Err(()) => return Err(()),
    };
    match OLDER_THAN_PRESETS.get(picked) {
        Some(days) => Ok(Some(format!("{days}d"))),
        None => {
            let v = |s: &str| {
                parse_older_than(s)
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            };
            match prompt_line(&theme, "Age in days, e.g. 45d", Some(&v)) {
                Ok(Some(s)) => Ok(Some(s)),
                // 自定义输入里按 Esc:连阈值问句一起取消,调用方回菜单。
                Ok(None) => Ok(None),
                Err(_) => Err(()),
            }
        }
    }
}

/// 一句 y/N,带默认值。方向键或 `y`/`n` 切换,**回车才确认**。
/// `Ok(None)` = Esc,调用方退回菜单。
fn prompt_yes_no(prompt: &str, default: bool) -> Result<Option<bool>, ()> {
    prompt_confirm(&ColorfulTheme::default(), prompt, default).map_err(|_| ())
}

/// 必填的一行文本。空白挡在这里而不是放下去:下游只会得到一句更远的
/// "找不到这个名字",而错其实出在这一行。
///
/// `Ok(None)` = Esc 取消,调用方原样回菜单。
fn prompt_required(prompt: &str) -> Result<Option<String>, ()> {
    let v = |s: &str| {
        if s.trim().is_empty() {
            Err("this one cannot be empty".to_string())
        } else {
            Ok(())
        }
    };
    match prompt_line(&ColorfulTheme::default(), prompt, Some(&v)) {
        Ok(Some(s)) => Ok(Some(s.trim().to_string())),
        Ok(None) => Ok(None),
        Err(_) => Err(()),
    }
}

/// 选填问句的结果。提交了值 / 提交了空 / Esc 取消——后两个都是 `None` 型
/// 结果,必须分开:留空是「按默认走」,Esc 是「这次不做了」。
enum LineOutcome {
    Value(String),
    Empty,
    Esc,
}

/// 选填的一行文本:留空 = 不带这个旗标,由命令自己走它的默认;
/// Esc = 取消整个问句,调用方回菜单。
fn prompt_optional(prompt: &str) -> Result<LineOutcome, ()> {
    match prompt_line(&ColorfulTheme::default(), prompt, None) {
        Ok(Some(s)) => {
            let s = s.trim().to_string();
            if s.is_empty() {
                Ok(LineOutcome::Empty)
            } else {
                Ok(LineOutcome::Value(s))
            }
        }
        Ok(None) => Ok(LineOutcome::Esc),
        Err(_) => Err(()),
    }
}

/// 会话 id:`session list` 的 ID 列。`Ok(None)` = Esc 取消。
fn prompt_rid() -> Result<Option<i64>, ()> {
    let v = |s: &str| {
        s.trim()
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| "please type a number, from the ID column".to_string())
    };
    match prompt_line(
        &ColorfulTheme::default(),
        "Conversation id, from the ID column of `session list`",
        Some(&v),
    ) {
        Ok(Some(s)) => Ok(Some(
            s.trim().parse::<i64>().expect("validator already checked"),
        )),
        Ok(None) => Ok(None),
        Err(_) => Err(()),
    }
}

// ---------------------------------------------------------------------------
// 计划确认
// ---------------------------------------------------------------------------

/// 确认结果。`Aborted`(Ctrl-C / 终端不可用)与 `No`(用户的决定)必须分开:
/// 前者是外壳异常,后者是"什么都没动"这个正常收场(退出码 4)。
pub enum Approval {
    Yes,
    No,
    Aborted,
}

/// 勾选确认的结果。`Run(allow)` 里是用户**逐项过目并勾过**的「路径 + 动作」。
///
/// 记白名单而不是黑名单:出计划与执行之间隔着用户读清单的时间,执行那一趟
/// 会重新出一遍计划——黑名单下,这期间新冒出来的项(agent 刚写完一个日志)
/// 会被当成「没被跳过」执行掉,而用户从没见过它。白名单下它只是没被勾,
/// 安安静静留着。见 `duster_core::plan::PlanFilter`。
pub enum PlanApproval {
    /// 用户逐项过目并勾过的「路径 + 动作」白名单。
    Run(Vec<(PathBuf, Action)>),
    /// 用户明确表示不做(一个都没勾 / Esc / 最终确认选 No)。
    No,
    /// 计划本身没有可动项,用户没被问过任何问题。
    Nothing,
    /// 终端不可用或真 IO 错误。Esc 不算这一类——它由 `No` 表达。
    Aborted,
}

/// 勾选列表的措辞。两个动词只差一个词形,但那个词就长在「按下 Enter 会发生
/// 什么」那一行上,写错等于问错。
#[derive(Clone, Copy)]
enum Verb {
    Clean,
    Prune,
}

impl Verb {
    /// 第三人称单数,用在提示行 `Enter {} the checked`。
    fn present_s(self) -> &'static str {
        match self {
            Verb::Clean => "cleans",
            Verb::Prune => "prunes",
        }
    }

    /// 首字母大写的祈使式,用在最后一问 `{} 3 items (…)?`。
    fn imperative(self) -> &'static str {
        match self {
            Verb::Clean => "Clean",
            Verb::Prune => "Prune",
        }
    }
}

/// 交互式 `clean` 的清单确认。见 [`approve_checklist`]。
pub fn approve_clean_plan(plan: &Plan) -> PlanApproval {
    approve_checklist(plan, Verb::Clean)
}

/// 交互式 `prune` 的清单确认。见 [`approve_checklist`]。
pub fn approve_prune_plan(plan: &Plan) -> PlanApproval {
    approve_checklist(plan, Verb::Prune)
}

/// 菜单里的清单确认:一张对齐的勾选表,取代全量 what / why / impact 转储。
///
/// 命令行路径不换——`--dry-run` 报告要能重定向成文件逐条核对,那是契约;
/// 菜单用户要的是一眼扫得完的形状:每行五列(agent / 类别 / 可回收体积 /
/// 闲置天数 / 路径),空格切换、方向键移动、Enter 提交,默认全勾——用户
/// 取消掉想保留的,而不是从零勾。
///
/// **依据(`why`)不进这张表**。它一句话写不进一行,而挤掉的恰恰是真正决定
/// 去留的那几个数字——早先那版把 `why` 截到 42 列塞在行尾,结果每行都以
/// "…" 收场,谁也读不到。要读全文就去看 `--dry-run` 那份报告。
///
/// **`install` 行不进这一屏**。它按定义永不参与勾选(`actionable()` 就把它
/// 滤掉了),十几行"duster never touches it"只会把真正要选的那几行顶出屏幕;
/// 那一桶的体量与去处由收尾报告的「installed software → duster uninstall」
/// 一行给出,信息不丢。
///
/// 全勾 → `Run(全部路径)`;部分勾 → `Run(勾选路径)`;一个不勾 → `No`。
///
/// 白名单而不是跳过清单:批准与执行之间计划会重算,黑名单下重算新冒出来
/// 的项会被执行而用户从没见过它;白名单下只有勾过的路径能通过,新冒出来
/// 的自然被挡在外面。所以全勾也交出完整路径列表,空 vec 不当「全部」的哨兵。
///
/// `clean` 与 `prune` 共用这一屏。两者的同意模型确实不同(clean 可以
/// `--yes` 成习惯,prune 不行),但那条区别管的是**命令行要不要逐项过目**;
/// 菜单里既然已经把清单摆在眼前,"这一条我想留着"就必须有地方表达——
/// 早先那版按类别逐组问、一个 no 就整体中止,等于让用户为了留一条缓存
/// 放弃整趟清理。
fn approve_checklist(plan: &Plan, verb: Verb) -> PlanApproval {
    let items: Vec<&PlanItem> = plan.actionable().collect();
    if items.is_empty() {
        println!(
            "  {}",
            muted().apply_to("Nothing to do. If that looks wrong, run `duster scan` first.")
        );
        return PlanApproval::Nothing;
    }

    // 勾选表:表头由勾选表自己印(与条目正文对齐),表格退场时一起擦掉——
    // 早先那版把表头 println 到 stdout、表格画在 stderr,退场靠
    // `Term::stdout().clear_last_lines(1)` 去追,两条流的行数各算一套,
    // 差一行就留下一个没有表的表头。现在整屏由一个函数画、一个函数擦。
    let (header, labels) = checklist_rows(&items, now_ms());
    let picked = match prompt_checklist(
        &menu_theme(),
        Checklist {
            prompt: &format!(
                "Space toggles · a all/none · Enter {} the checked · Esc cancels",
                verb.present_s()
            ),
            header: Some(&header),
            items: &labels,
            // 默认全勾:用户取消掉想保留的,而不是从零勾。
            checked: vec![true; items.len()],
            // 这一屏没有「全选」行:每一行都是一条真实的删除项,凑一行假的
            // 在最上面,勾选表就不再是「清单本身」了。`a` 键管全勾全不勾。
            select_all: false,
            page: crate::browse::viewport(CHECKLIST_RESERVED),
        },
    ) {
        Ok(Some(p)) => p,
        // Esc 是用户的决定,不是外壳异常:`prompt_checklist` 的 `Ok(None)` 只
        // 可能来自 Esc——Ctrl-C 由终端送 SIGINT,进程当场结束,根本走不到这里。
        // 归进 `Aborted` 会让「我不想清了」落到「一般错误」那一档(退出码 1)。
        Ok(None) => return PlanApproval::No,
        Err(_) => return PlanApproval::Aborted,
    };
    if picked.is_empty() {
        return PlanApproval::No;
    }

    // 提交前最后一问:勾选的数量与体积一起过目,回车不该等于同意。
    let bytes: u64 = picked.iter().map(|&i| items[i].bytes).sum();
    match ask(
        &ColorfulTheme::default(),
        &format!(
            "{} {} ({})? The un-checked ones stay untouched.",
            verb.imperative(),
            crate::plural(picked.len(), "item"),
            human_bytes(bytes)
        ),
    ) {
        Approval::Yes => {}
        Approval::No => return PlanApproval::No,
        Approval::Aborted => return PlanApproval::Aborted,
    }

    // 白名单同时带路径与动作:重算那一趟里同一条路径可能换了刀(清单升级把
    // 某条 artifact 从 l0 改成 l1),用户勾的是 `vacuum`,执行的会是 `remove_file`。
    PlanApproval::Run(
        picked
            .iter()
            .map(|&i| (items[i].path.clone(), items[i].action))
            .collect(),
    )
}

/// 勾选表的表头 + 每行正文。列宽按**这一批**数据算,不写死:同一列在不同
/// agent 组合下宽度不同,写死只会让某一批全是空格、另一批全被截断。
///
/// 前四列定宽,路径吃掉剩下的终端宽度;窄终端下先截路径,数字列一格不让——
/// 「多大、闲置多久」是这一屏存在的理由,截掉它们就等于回到一堆路径。
fn checklist_rows(items: &[&PlanItem], now: i64) -> (String, Vec<String>) {
    checklist_rows_at(items, now, term_width())
}

/// 宽度算法本体;`cols` 由测试直接给(60 / 80 / 200),不必 mock 终端。
fn checklist_rows_at(items: &[&PlanItem], now: i64, cols: usize) -> (String, Vec<String>) {
    let cells: Vec<[String; 5]> = items
        .iter()
        .map(|i| {
            [
                i.agent_id.clone(),
                row_kind(i).to_string(),
                human_bytes(i.bytes),
                idle_days(i.last_used_ms, now),
                display_tilde(&i.path),
            ]
        })
        .collect();

    const HEAD: [&str; 5] = ["AGENT", "KIND", "FREES", "IDLE", "PATH"];
    // 前四列:表头与本批数据里最宽的那个。第五列(路径)不参与,它拿余量。
    let mut w = [0usize; 4];
    for (c, width) in w.iter_mut().enumerate() {
        *width = cells
            .iter()
            .map(|r| display_width(&r[c]))
            .chain(std::iter::once(HEAD[c].len()))
            .max()
            .unwrap_or(0);
    }
    // 行宽这笔账,逐项列:条目前缀 `❯ [x] ` 占 6 列(位置标记 1 + 空格 1 +
    // `[x]` 3 + 空格 1)+ 前四列定宽合计 + 列间空格 4 + 尾部余量 1(行宽
    // 碰到终端宽就被折成两个物理行,而控件按逻辑行计数、`clear_last_lines`
    // 按物理行擦,折一行残影每按一次翻一倍)。
    //
    // 旧版把前缀按 4 列算,少的 2 列全进了路径列,行宽恰好 = 终端宽 + 1,
    // 每一行都折——这就是交互式 clean 残影翻倍的根源。
    let fixed: usize = 6 + w.iter().sum::<usize>() + 4 + 1;
    let path_w = cols.saturating_sub(fixed).max(16);

    let line = |r: &[String; 5]| {
        format!(
            "{:<w0$} {:<w1$} {:>w2$} {:>w3$} {}",
            r[0],
            r[1],
            r[2],
            r[3],
            truncate_width(&r[4], path_w),
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
        )
    };
    let header = line(&HEAD.map(str::to_string));
    (header, cells.iter().map(line).collect())
}

/// 类别列。l2 是 prune 的固定地盘,它比库内 kind(`artifact`)更能说明
/// 「这行为什么在这里」;其余用 kind 原文。
/// 超编代际标 `l2 surplus`:它与按龄清理的 l2 项同屏出现,IDLE 列还都
/// 是「几天前」,不点破理由,读者就会把 16 天前的文件读成按 90d 删的。
fn row_kind(item: &PlanItem) -> &str {
    match item.clean_level {
        Some(CleanLevel::L2) if item.generation => "l2 surplus",
        Some(CleanLevel::L2) => "l2",
        _ => item.kind.as_str(),
    }
}

/// 闲置多久。没有可用时间戳时给 `?` ——**不是 0 天,也不是很久**:
/// 计划层对没有证据的项一律视为最近用过,这一列得跟那条口径一致。
fn idle_days(last_used_ms: Option<i64>, now: i64) -> String {
    match last_used_ms {
        Some(ms) => format!("{}d", ((now - ms) / 86_400_000).max(0)),
        None => "?".to_string(),
    }
}

/// 当前时间(Unix 毫秒)。只用于「闲置多久」这一列的展示。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 终端宽度;拿不到(管道、非 TTY)时按 100 列算。这一屏只在 TTY 下出现,
/// 拿不到宽度是异常而不是常态,给个够宽的数比给 80 更少截断。
fn term_width() -> usize {
    Term::stderr()
        .size_checked()
        .map_or(100, |(_, cols)| cols as usize)
}

/// 一行最多能占多宽:终端宽减控件前缀,再留 1 列——行宽碰到终端宽就会
/// 被折成两个物理行,而控件按逻辑行计数、`clear_last_lines` 按物理行擦,
/// 折一行残影每按一次翻一倍。所有行生成器(`checklist_rows` / `sync_target
/// _rows` / `agent_choice_rows` / `session_list_rows` / `skill_group_rows` /
/// `search_rows`)共用它,不许各写各的预算。
fn row_budget(prefix: usize) -> usize {
    row_budget_at(term_width(), prefix)
}

/// 纯算术版:终端宽由调用方给(测试直接嗂 60 / 80 / 200),不必 mock 终端。
fn row_budget_at(cols: usize, prefix: usize) -> usize {
    cols.saturating_sub(prefix + 1)
}

/// uninstall 的同意:逐字输入 agent id,不接受一个 y。
/// 这个动作删的是用户全部历史会话与记忆,一次按键换不来它。
pub fn confirm_uninstall(agent: &str) -> Approval {
    match prompt_line(
        &ColorfulTheme::default(),
        &format!("Type `{agent}` to remove it, anything else to stop"),
        None,
    ) {
        Ok(Some(typed)) if typed.trim() == agent => Approval::Yes,
        Ok(Some(_)) => Approval::No,
        Ok(None) | Err(_) => Approval::Aborted,
    }
}

/// 一句 y/N。默认 No —— 回车不该等于同意。方向键 / `y` / `n` 切换,
/// Enter 确认,`Esc` 与选 No 同路(都是「什么都没动」)。
fn ask(theme: &ColorfulTheme, prompt: &str) -> Approval {
    match prompt_confirm(theme, prompt, false) {
        Ok(Some(true)) => Approval::Yes,
        Ok(Some(false)) | Ok(None) => Approval::No,
        Err(_) => Approval::Aborted,
    }
}

/// 交互式收集 search 参数:query(≥3 字符)、可选 agent、limit(默认 20)。
/// 命中逐条浏览,选中即 `open` 整轮(折叠视图)。Esc 回菜单,什么都没发生。
fn prompt_search(mode: OutputMode, index: Option<&Path>) -> i32 {
    let theme = ColorfulTheme::default();
    let v = |s: &str| {
        if s.chars().count() >= 3 {
            Ok(())
        } else {
            Err("please type at least 3 characters".to_string())
        }
    };
    let query = match prompt_line(&theme, "Search for (3 characters or more)", Some(&v)) {
        Ok(Some(q)) => q,
        Ok(None) => return EXIT_OK, // Esc:回菜单,什么都没搜
        Err(_) => return EXIT_ERROR,
    };
    // 范围问句与三个清理动词共用一句,免得同一件事有两套措辞。
    let agents = match prompt_agent(index) {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let hits = match search::search(
        index,
        &query,
        &SearchFilter {
            agents,
            limit: crate::DEFAULT_SEARCH_LIMIT,
        },
    ) {
        Ok(h) => h,
        Err(e) => return fail(mode, "search", &e),
    };
    if hits.is_empty() {
        // 与 render_search_human 的措辞一致。
        eprintln!();
        eprintln!("  {} No matches for {}", warn_mark(), style(&query).bold());
        eprintln!(
            "  {}",
            muted().apply_to(
                "Search needs 3 characters or more. If the index is stale, run `duster scan`."
            )
        );
        return EXIT_OK;
    }
    let (header, labels) = search_rows(&hits, &query);
    match browse("Which turn to read", &header, &labels) {
        Some(i) => cmd_open(mode, index, hits[i].tid, false),
        None => EXIT_OK,
    }
}

/// 命中的浏览行:`#tid  agent  file  turn N · role`,与 `render_search_human`
/// 同一形状——文件名截到 32,定位靠 tid,文件名只是上下文。snippet 不进行:
/// 选中即打开整轮,摘要只在命令行那份列表里有意义。
///
/// role 吃余量,整行宽有 [`row_budget`] 上限:文件名截到 32 只封住了它自己,
/// 没人算过整行,tid + agent + 文件名 + turn 一起就能顶破终端宽。
fn search_rows(hits: &[SearchHit], query: &str) -> (String, Vec<String>) {
    let n = hits.len();
    let header = format!(
        "{n} {} for \"{query}\"",
        if n == 1 { "match" } else { "matches" }
    );
    let rows: Vec<String> = hits
        .iter()
        .map(|h| {
            let file = Path::new(&h.resource_path).file_name().map_or_else(
                || h.resource_path.clone(),
                |f| f.to_string_lossy().into_owned(),
            );
            let mut line = format!(
                "#{:<6} {}  {}  turn {} · ",
                h.tid,
                h.agent_id,
                truncate_width(&file, 32),
                h.seq,
            );
            // 行宽这笔账:`❯ ` 前缀 2 + 行内定宽部件(tid / agent / 文件名
            // ≤32 / turn 标签)+ 尾部余量 1。role 吃余量:它是这行里唯一
            // 可截的尾巴,其余部件各就各位。
            let role_w = row_budget(2).saturating_sub(display_width(&line)).max(4);
            line.push_str(&truncate_width(&h.role, role_w));
            line
        })
        .collect();
    (header, rows)
}

/// 交互式收集 open 参数:tid(来自 search 结果)。
fn prompt_open(mode: OutputMode, index: Option<&Path>) -> i32 {
    let v = |s: &str| {
        s.trim()
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| "please type a number, as search shows it".to_string())
    };
    let tid = match prompt_line(
        &ColorfulTheme::default(),
        "Turn id (the #42 shown by search)",
        Some(&v),
    ) {
        Ok(Some(t)) => t.trim().parse::<i64>().expect("validator already checked"),
        Ok(None) => return EXIT_OK, // Esc:回菜单
        Err(_) => return EXIT_ERROR,
    };
    // 菜单的 open 走默认档:工具轮折叠、正文截断。要全文去命令行加 `--full`。
    cmd_open(mode, index, tid, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从顶层沿着 `Route::Group` 走一遍,收齐每一屏。写死一张名单会漏掉
    /// 下一个加进来的组,而漏掉的那一屏正是没人检查的那一屏。
    fn all_menus() -> Vec<&'static Menu> {
        fn walk(menu: &'static Menu, out: &mut Vec<&'static Menu>) {
            out.push(menu);
            for item in menu.items {
                if let Route::Group(sub) = item.route {
                    walk(sub, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&TOP, &mut out);
        out
    }

    /// 上屏的每一个字节都是英文。中文只留在注释与测试名里——这条规矩
    /// 没有编译器守着,所以让它在这里失败。
    #[test]
    fn 菜单文案不含非_ascii() {
        for menu in all_menus() {
            assert!(menu.prompt.is_ascii(), "prompt: {}", menu.prompt);
            for item in menu.items {
                assert!(item.name.is_ascii(), "name: {}", item.name);
                assert!(item.desc.is_ascii(), "desc of {}: {}", item.name, item.desc);
            }
        }
    }

    /// 每一屏都得有一条走得出去的路。Esc 也能出去,但它不显示在屏幕上,
    /// 一屏没有出口的菜单看起来就是个死胡同。
    #[test]
    fn 每一屏都有一条离开的路() {
        for menu in all_menus() {
            assert!(
                menu.items.iter().any(|i| matches!(i.route, Route::Leave)),
                "no way out of: {}",
                menu.prompt
            );
        }
    }

    /// 同一屏里两行同名等于让用户抛硬币。
    #[test]
    fn 同一屏内命令名不重复() {
        for menu in all_menus() {
            let mut names: Vec<&str> = menu.items.iter().map(|i| i.name).collect();
            let total = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), total, "duplicate name in: {}", menu.prompt);
        }
    }

    /// M2 的三组必须在菜单上找得到。`--help` 里有而菜单里没有,对只会
    /// 敲一个 `duster` 的用户来说就等于不存在——这正是这次要修的那个洞。
    ///
    /// `diff` 不在此列:它是通用文件/文件夹对比,从顶层刻意撤下(见顶层
    /// 菜单的注释),引擎仍经 `skill copies` 的组详情与 `mcp diff` 可达。
    #[test]
    fn m2_三组都在顶层露面且_diff_已撤下() {
        for name in ["session", "memory", "mcp"] {
            assert!(
                TOP_ITEMS.iter().any(|i| i.name == name),
                "missing from the top menu: {name}"
            );
        }
        assert!(
            !TOP_ITEMS.iter().any(|i| i.name == "diff"),
            "diff 应从顶层菜单撤下"
        );
    }

    /// 组这一列的尖角只属于组:它是「这条不会当场跑」的唯一提示,
    /// 印错一个位置,用户就会以为选中它会执行点什么。
    #[test]
    fn 只有组才带尖角() {
        for menu in all_menus() {
            for (line, item) in rendered_items(menu.items).iter().zip(menu.items) {
                let line = console::strip_ansi_codes(line);
                assert_eq!(
                    line.contains('›'),
                    matches!(item.route, Route::Group(_)),
                    "{line}"
                );
            }
        }
    }

    /// 一条只填了这张表用得上的字段的计划项;`why` 塞满字,好证明它不进表。
    fn item(agent: &str, path: &str, bytes: u64, last_used_ms: Option<i64>) -> PlanItem {
        PlanItem {
            agent_id: agent.into(),
            kind: "artifact".into(),
            clean_level: Some(CleanLevel::L1),
            path: PathBuf::from(path),
            key: "k".into(),
            rid: -1,
            what: "what".into(),
            why: "REASON-THAT-MUST-NOT-BE-IN-THE-ROW".into(),
            impact: "impact".into(),
            archived: false,
            action: duster_core::plan::Action::RemoveDir,
            bytes,
            cleanable: true,
            generation: false,
            last_used_ms,
        }
    }

    /// 这张表是菜单里唯一的删除依据,所以它的四件事都得成立:五列都在、
    /// 路径列在每一行都从同一列开始(不对齐的表等于没有表)、体积与闲置
    /// 天数一格不少、`why` 一个字都不进来(它是把行挤爆的那个东西)。
    #[test]
    fn 勾选表五列对齐且不含依据长句() {
        let now = 1_000 * 86_400_000; // 第 1000 天,好让减法一眼看得懂
        let a = item(
            "claude-code",
            "/tmp/x/cache",
            300 * 1024,
            Some(now - 3 * 86_400_000),
        );
        let b = item("pi", "/tmp/x/logs", 2, None);
        let (header, rows) = checklist_rows(&[&a, &b], now);

        for col in ["AGENT", "KIND", "FREES", "IDLE", "PATH"] {
            assert!(header.contains(col), "缺列 {col}:{header}");
        }
        let at = |s: &str| s.find("/tmp/x/").or_else(|| s.find("PATH")).unwrap();
        assert_eq!(at(&header), at(&rows[0]), "表头与数据行的路径列必须同起点");
        assert_eq!(at(&rows[0]), at(&rows[1]), "两行的路径列必须同起点");

        assert!(
            rows[0].contains("300 KB") && rows[0].contains("3d"),
            "{}",
            rows[0]
        );
        // 没有可用时间戳 = 没有证据,给 `?`,不许算成 0 天。
        assert!(
            rows[1].contains(" ?") && !rows[1].contains("0d"),
            "{}",
            rows[1]
        );
        for row in &rows {
            assert!(!row.contains("REASON-THAT-MUST-NOT-BE-IN-THE-ROW"), "{row}");
        }
    }

    /// 勾选表的每一行(含表头)加上控件前缀 `❯ [x] `(6 列)后,显示宽度
    /// 必须严格小于终端宽度——行宽碰到终端宽就会折成两个物理行,而控件按
    /// 逻辑行计数、`clear_last_lines` 按物理行擦,残影每按一次翻一倍。
    /// 用超长路径逼路径列截断,并直接给 60 / 80 / 200 三个宽度(纯函数
    /// [`checklist_rows_at`],不必 mock 终端)。
    #[test]
    fn checklist_rows_行宽不超终端() {
        let now = 1_000 * 86_400_000;
        let a = item(
            "claude-code",
            "/very/long/path/that/goes/on/and/on/and/on/never/ending/cache/dir",
            300 * 1024,
            Some(now - 3 * 86_400_000),
        );
        let b = item("gpt-oss-120b", "/tmp/x/logs", 2, None);
        for cols in [60usize, 80, 200] {
            let (header, rows) = checklist_rows_at(&[&a, &b], now, cols);
            for line in std::iter::once(&header).chain(rows.iter()) {
                let w = 6 + display_width(line);
                assert!(
                    w < cols,
                    "{cols} 列终端:行宽 {w} 超限: {line}"
                );
            }
        }
    }

    /// 目标勾选表在 80 列终端下必须放得下那条出了名的长路径:路径列不截
    /// 的话,`claude-desktop · create · ~/Library/Application Support/…` 一行
    /// 就是 90 列,折行后控件按逻辑行计数、`clear_last_lines` 按物理行擦,
    /// 残影翻倍。路径列由 [`sync_target_rows_at`] 截进余量,这里逐个宽度验。
    #[test]
    fn sync_target_rows_行宽不超终端() {
        let plan = vec![SyncOutcome {
            agent_id: "claude-desktop".into(),
            path: "~/Library/Application Support/Claude/claude_desktop_config.json".into(),
            action: "create".into(),
            error: None,
            snapshot: None,
        }];
        for cols in [60usize, 80, 200] {
            let rows = sync_target_rows_at(&plan, cols);
            for line in &rows {
                let w = 6 + display_width(line);
                assert!(
                    w < cols,
                    "{cols} 列终端:行宽 {w} 超限: {line}"
                );
            }
        }
    }
}
