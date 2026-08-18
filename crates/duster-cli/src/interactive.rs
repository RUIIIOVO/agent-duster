//! 交互式启动菜单:裸 `duster`(无子命令)时进入。
//!
//! - TTY 下:彩色 banner + 上下箭头选择命令,缺的参数当场补问。
//! - 非 TTY 或 `--json`:交互无意义,退化为打印帮助。
//! - banner 与菜单全部走 stderr,stdout 仍只放命令结果,与 output.rs 的
//!   分工一致。
//! - **任何一屏一次 Esc = 返回上一层**。这是全菜单的导航契约:列表退回
//!   菜单、详情退回列表、过滤态先清过滤再退屏,谁也不用按第二次;back
//!   条目因此整个撤掉——同一扇门不装两个把手。
//! - 菜单 ↔ 命令的边界上做两件事:命令跑完**先停下等按键**
//!   ([`pause_for_menu`]),再清屏重绘菜单。少了那一停,菜单会在命令返回的
//!   同一瞬间盖掉结果,阅读窗口是 0 毫秒;mole 在同一个位置也停。只有真
//!   跑出结果的收场才停([`Outcome::shown`]);Esc 一路取消的收场不停
//!   ([`Outcome::silent`]),空屏上拦一道按键就是白拦。
//! - 清屏分两种,取决于屏上那一屏值不值得留:菜单屏用
//!   [`clear_menu_screen`](直接 `ESC[2J ESC[H` 擦掉,菜单进 scrollback
//!   只是噪音);命令结果那一屏用 [`scroll_out_and_clear`],先滚进
//!   scrollback 再擦——`ESC[2J` 在 ghostty / kitty 上是**连同 scrollback
//!   一起丢**(ghostty#905),直接擦会把用户刚要读的结果销毁掉。两者都走
//!   stderr、只对 TTY 说话。drill / browse 的列表→详情循环不在边界上,
//!   一条清屏都不发,光标带回原行的契约原样保留。
//! - 每一张列表——菜单、参数选择、`browse` 的浏览表、勾选表——都是
//!   `prompt.rs` 里的自研控件([`prompt_pick`] / [`prompt_checklist`] /
//!   [`prompt_browse`]);dialoguer 只剩它的 `ColorfulTheme` 在用
//!   (`?` / `✔` 这套视觉词汇)。理由见 [`menu_pick`] 与 [`prompt_pick`]:
//!   页高必须由知道自己在屏幕上印了几行的人来定,而 dialoguer 的控件从
//!   终端行数自己算。
//!
//! # 一层菜单 + 统一列表流
//!
//! M2 之后 CLI 有二十多个子命令,但菜单只剩一层:动词(clean / prune /
//! uninstall / status / doctor)选中即补参数、跑;名词(skill / mcp /
//! session / memory)选中**直接进列表**——browse 分页、`/` 子串过滤、
//! Space 勾选批量、Enter 下钻详情,详情屏底部挂上下文动作(形状见
//! [`drill`])。以前的二层名词菜单(`session › list / prune` 这种)整个
//! 撤了:那一屏只有一两条,每次都要多按一次键才摸到真正的表。
//!
//! 随之两条组内动词从菜单撤下、命令行原样保留:`session prune` 的按龄
//! 归档清理由顶层 `prune` 覆盖,同一个动词摆两层只会让人犹豫先按哪个,
//! 菜单**有意只在顶层放 prune**;`mcp ping` 的全量档也不再单列,逐个
//! ping 在 mcp 详情屏与勾选批量里都有。
//!
//! # 两条不许破的规矩
//!
//! - **只发真实存在的参数组合**。每条路由都对着 clap 的定义抄;菜单里不许
//!   出现一个 `--help` 里没有的旗标,也不许替用户猜一个默认值。批量动作
//!   也一样:勾选集合只是把同一条真实命令替用户敲 N 遍。
//! - **危险动作照旧先出计划**。`clean` / `prune` 走 [`Consent::Ask`],
//!   用勾选表确认([`approve_prune_plan`] / `mcp_sync_named` 的目标勾选表);
//!   `uninstall` 的护栏是菜单里的两道默认 No 的 y/N 确认
//!   (见 [`prompt_uninstall`]),确认串直接给 agent id,与命令行
//!   `--confirm <agent>` 走同一条执行门。
//!   顺序不能换——先看清单后表态才是有效确认。

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::CommandFactory;
use console::{Key, Style, Term, style};
use dialoguer::theme::{ColorfulTheme, Theme};

use crate::prompt::{
    Checklist, Picker, prompt_checklist, prompt_confirm, prompt_line, prompt_pick,
};

use duster_core::freshness::{Freshness, ensure_fresh};
use duster_core::mcp::{self, MergedServer, SyncOptions, SyncOutcome};
use duster_core::memory::{self, MemoryEntry, RemoveInspection, RemoveRequest, RemoveTarget};
use duster_core::plan::{Action, OLDER_THAN_PRESETS, Plan, PlanItem, binary_inside_owns, parse_older_than};
use duster_core::session::{self, SessionFilter, SessionRow};
use duster_core::skill_ops::{self, DupState, SkillGroup};
use duster_core::status::{AgentStatus, status};
use duster_fs::path::display_tilde;
use duster_model::CleanLevel;

use crate::browse::{Browsed, browse};
use crate::cmd;
use crate::output::{
    EXIT_CONFIRM_DENIED, EXIT_ERROR, EXIT_OK, EXIT_PARTIAL, OutputMode, Prefix, Table, accent,
    human_bytes, muted, ok_mark, truncate_width, warn_mark, worse, display_width,
};
use crate::{
    Cli, Consent, UninstallArgs, cmd_clean, cmd_doctor, cmd_prune, cmd_skill_link, cmd_skill_rm,
    cmd_status, cmd_uninstall, fail, render_warnings,
};

/// 菜单条目:命令名、一句话说明、名字的颜色(逐条不同,即「多彩」)、选中后的去向。
struct MenuItem {
    name: &'static str,
    desc: &'static str,
    color: fn(&Style) -> Style,
    route: Route,
}

/// 一条菜单动作(`Route::Run` 目标)的收场:退出码 + 回菜单前要不要停。
///
/// pause 的判据只有一条:这趟有没有往屏上印过用户还没读过的结果。跑了
/// 命令、打了错误或空表说明 = [`Outcome::shown`];Esc 一路取消(交互屏
/// 都自己擦干净了)= [`Outcome::silent`]——Esc 逐层退,一次一层,不该在
/// 空屏上再等一次按键。列表流的浏览退场也是 silent:屏上的一切都是用户
/// 自己翻出来的,退出前早读过了;回菜单前照样滚进 scrollback,只是不再
/// 拦一道「按键继续」。
#[derive(Clone, Copy)]
struct Outcome {
    code: i32,
    pause: bool,
}

impl Outcome {
    /// 跑出了结果:回菜单前停一下([`pause_for_menu`]),让人把结果读完。
    fn shown(code: i32) -> Self {
        Self { code, pause: true }
    }

    /// 取消收场,屏上没有没读过的东西:直接回菜单。
    fn silent(code: i32) -> Self {
        Self { code, pause: false }
    }
}

/// 选中一条之后去哪。
#[derive(Clone, Copy)]
enum Route {
    /// 跑一条命令。需要补参数的,由这个函数自己问完再跑;回菜单前停不停
    /// 由它交回的 [`Outcome`] 说了算。
    Run(fn(OutputMode, Option<&Path>) -> Outcome),
    /// 退出菜单。
    Leave,
}

/// 勾选表/单选菜单交给 `prompt_pick`/`prompt_checklist` 时,原语自己还要
/// 画 prompt 1 行、页脚 1 行,再留 1 行余量;表头那一行由控件内部再减
/// (`body_page`),调用方不要重复扣。
///
/// 上一级的回执**不算**在内:它滚出屏幕是无害的,只要控件自己画的那一整块
/// 不超过终端高度,退场时 `clear_last_lines` 就能原样擦干净。
///
/// 菜单头部([`draw_header`])**要算**,由 `run_menu` 现场申报给
/// [`menu_pick_reserving`]:清屏顶对齐之后,头部不再是「印一次就滚走」的
/// banner,它是这一屏的一部分,不申报就会把菜单尾巴顶出屏幕。
const CHECKLIST_RESERVED: usize = 3;

/// 一屏菜单:一句问话 + 一组条目。只剩顶层这一张(名词直接进列表,二层
/// 菜单已撤,见模块文档),留着这个结构是让「问话 + 条目」继续一起走。
struct Menu {
    prompt: &'static str,
    items: &'static [MenuItem],
}

/// 顶层那一屏。顺序是用户拍板的,照抄:清理三连(`clean` / `prune` / `uninstall`)
/// 置顶——它们是日常常客;`status` / `skill` 与三个名词(`mcp` / `session` /
/// `memory`)居中;`doctor` 沉底,紧挨 `quit`——它是自检,问的是「duster
/// 自己装好没有」,不是日常动作。红色留给唯一会删光一个 agent 的那条;
/// quit 白色。
///
/// `scan` 从这里撤了:[`ensure_fresh`] 上线后,裸 `duster` 进菜单就起后台
/// 扫描,一次性命令也各自刷新——用户能动手的时候索引**永远已经是新的**,
/// 菜单里再放一条「去扫描」等于把实现细节摆在门面上。`scan --full` 是
/// 「search 结果看着不对」时的重建逃生门,极少用,留在命令行即可
/// (`duster scan --full`);`Command::Scan` 在 clap 里原样保留。
///
/// `search` 也撤了:全文检索是「翻旧账」的低频动作,占一个顶层槽位不划算,
/// 而它的交互收集(query / agent / limit 三问)在命令行上本来就是一行的事。
/// `Command::Search` 在 clap 里原样保留。
///
/// `skill copies` 改叫 `skill list`,改名**带上了行为变化**——`list` 这个名字
/// 要求它名副其实:旧版只报装在多处的重复组(本机 12 个),全机只有一份的
/// skill(本机 23 个)在菜单里完全看不见;现在列的是**全部** skill,装在
/// 多处的在 AGENTS 列一眼可见。STATE 列标 identical / drifted / 全机单份,
/// 详情屏里还能 link 共享一份副本。
///
/// `diff` 从这里撤了:它是通用文件/文件夹对比,和系统 `diff` 撞车——对比任意
/// 两个路径的工具用户手里已经满地都是,占掉一个顶层槽位不划算。引擎不删:
/// `Command::Diff` 在命令行原样保留。菜单里已没有它的落脚点——`mcp` 详情屏
/// 的 compare 动作随 `mcp diff` 一起撤下,「几家声明一样吗」由铺平表的
/// STATE 列直接回答;要比具体的两条配置,拿列表里的两条 PATH 去喂系统
/// diff 或 `duster diff`。
///
/// `uninstall` 排在第 3 位,比以前靠前很多——这条不是随手排的:它的护栏是
/// 一张范围勾选表,随后只有一道 y/N 确认(见 [`prompt_uninstall`]);不是旧版
/// 那种两道默认 No 的确认。位置再靠前,误触也过不了这两层门。
static TOP: Menu = Menu {
    prompt: "Pick a command",
    items: &TOP_ITEMS,
};

static TOP_ITEMS: [MenuItem; 10] = [
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
        name: "status",
        // 体检已撤(status 只报家底,不再报发现),说明也跟着只说家底——
        // 「what needs attention」在没有发现机制的今天就是一句谎。
        desc: "See how much disk each agent uses",
        color: |s| s.clone().blue(),
        route: Route::Run(run_status),
    },
    MenuItem {
        name: "skill",
        // 列全部、标多处:单份组是常态(本机 35 个里 23 个只装在一个 agent),
        // 藏起来等于让用户以为那些 skill 不存在;装在多处的在 AGENTS 列一眼
        // 可见。58 字符,80 列终端下说明不截断(预算约 65,见 `rendered_items_at`)。
        desc: "List every skill, and flag the ones in more than one place",
        color: |s| s.clone().magenta(),
        route: Route::Run(skill_list),
    },
    MenuItem {
        name: "mcp",
        // compare 已随 mcp diff 撤下:铺平表每行一条声明、STATE 列给合并
        // 结论,「几家声明一样吗」看列表就是答案。
        desc: "See every MCP declaration, copy or sync them",
        color: |s| s.clone().blue(),
        route: Route::Run(mcp_list),
    },
    MenuItem {
        name: "session",
        // shrink(session prune)已从菜单撤到顶层 prune,说明只承诺列表
        // 真给得出的动作:浏览、导出。
        desc: "Browse and export your conversations",
        color: |s| s.clone().cyan(),
        route: Route::Run(session_list),
    },
    MenuItem {
        name: "memory",
        desc: "Read what your agents remember about you",
        color: |s| s.clone().magenta(),
        route: Route::Run(memory_list),
    },
    MenuItem {
        name: "doctor",
        desc: "Check duster itself: index, adapters, home directory and version",
        color: |s| s.clone().blue(),
        route: Route::Run(run_doctor),
    },
    MenuItem {
        name: "quit",
        desc: "Leave",
        color: |s| s.clone().white(),
        route: Route::Leave,
    },
];

/// 后台索引扫描:菜单不等它。
///
/// 冷启动全量建库实测 4.14 秒(11 agent / 634 会话 / 26,399 轮),增量 0.52 秒。
/// 用户敲的是 `duster`,他要的是那张菜单,不是先看四秒空屏——所以 banner 一画完
/// 就把 [`ensure_fresh`] 扔进一个线程,菜单立刻出来;真正要用到索引的那一刻
/// (选中一条命令)才回来收账([`BackgroundScan::join`])。挑一条菜单本身通常
/// 就花掉不止 4 秒,所以那次 join 几乎总是瞬返。
///
/// # 只有这一个线程
///
/// **不许再起第二个去画动画转圈**。[`menu_pick_reserving`] 阻塞在 `read_key`
/// 上,它自己重绘、自己算擦几行;另一个线程往 stderr 写字会和它抢同一个光标,
/// 留下擦不掉的残影(而且是每重绘一次翻一倍的那种)。头部那行 [`SCANNING_LINE`]
/// 是静态的,靠 [`run_menu`] 每一轮循环(= 用户每按一次键)重画来更新——
/// 0.5~4 秒的扫描到那时早跑完了,那一行自然消失。用一行不动的字换一个不会
/// 出错的屏幕,这笔买卖划算。
///
/// # 半路退出扫到一半的库
///
/// 用户进菜单三秒就按 `q`,`main` 会 `process::exit` 把这个线程连同它写了一半的
/// 事务一起掐掉。两样东西都扛得住,所以这里不做任何收尾:单实例锁是 `<db>.lock`
/// 上一笔没提交的 `BEGIN EXCLUSIVE`(`duster_index::db` 明说这样选就是为了不留
/// 死锁文件),进程一死内核就把 fd 上的锁放了;`index.db` 走 WAL,掐断等同断电,
/// 下次开库自己回滚,而扫描本身是 upsert,下一轮增量接着扫完。
///
/// # 为什么这里可以顺手建库
///
/// 进了菜单就一定会建库,这与 `doctor` 那条「不许顺手建库」不冲突:那条管的是
/// 一次性命令(见 main.rs 的 `needs_fresh_index`),体检报告的职责是描述现状;
/// 而交互式外壳本来就是围着索引转的,用户是进来干活的。
struct BackgroundScan(Option<JoinHandle<anyhow::Result<Freshness>>>);

impl BackgroundScan {
    /// 起线程。`index` 是借用,进线程前先要过所有权——线程的寿命不受这次调用的
    /// 栈帧约束,`&Path` 过不去。
    fn spawn(index: Option<&Path>) -> Self {
        let owned: Option<PathBuf> = index.map(Path::to_path_buf);
        Self(Some(std::thread::spawn(move || {
            ensure_fresh(owned.as_deref())
        })))
    }

    /// 还在扫吗——头部那一行的开关。
    ///
    /// 问的是**线程活没活**,不是「join 过没有」:线程自己跑完了,下一轮重画
    /// 头部时那一行就该消失,不必等到用户选中命令。
    fn running(&self) -> bool {
        self.0.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// 收账:等扫描结束,然后把手柄 `take` 掉——只 join 一次,后续轮次直接过。
    ///
    /// 三种收场**都不阻断菜单**。用户是来干活的,索引旧一点也比一屏报错强:
    /// - 扫描报错:降级成一行 warning。库要么还在盘上(读得到旧数据),要么
    ///   下游的 `ensure_exists` 会自己再试一次;
    /// - 撞上另一个 duster 实例的写锁([`Freshness::SkippedLocked`]):静默放行。
    ///   读路径全是 `open_readonly`,别人写的时候照样看得见,这不是错;
    /// - 线程 panic:同样降级成一行 warning,绝不把 panic 传染给菜单。
    ///
    /// 扫完**不报喜**:`Built` / `Refreshed` / `UpToDate` 一律静默。头部那行
    /// 消失就是回执;再补一句「11 agents · 4.9 GB · 刚刚更新」只会把用户的眼睛
    /// 从他正要选的那条命令上拽走。
    fn join(&mut self) {
        let Some(handle) = self.0.take() else {
            return;
        };
        // 真要等的时候先说一声:无声卡住的四秒和死机长得一模一样。
        if !handle.is_finished() {
            eprintln!(
                "  {}",
                muted().apply_to("waiting for the index scan to finish…")
            );
        }
        let why = match handle.join() {
            Ok(Ok(_)) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "the scan thread panicked".to_string(),
        };
        eprintln!(
            "  {} {}",
            warn_mark(),
            muted().apply_to(format!(
                "index refresh failed: {why}; showing what the index already has"
            ))
        );
    }
}

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

    // 首屏与「命令跑完回菜单」走同一套顶对齐:先把终端上已有的东西滚进
    // scrollback,再从左上角画菜单。
    //
    // 少了这一下,裸 `duster` 的招牌会直接画在 shell 提示符下面,上一次运行
    // 的残留、`ls` 的输出、上一条命令的报错全都留在菜单上方——而回菜单那条
    // 路(`scroll_out_and_clear`)是顶对齐的,同一个菜单两种长相,先看到的
    // 那一种还是更脏的那一种。用 scroll_out 而不是直接 2J:用户的历史输出
    // 是他自己的东西,滚进 scrollback 上翻还能找回来,擦掉就真没了。
    scroll_out_and_clear();

    // 菜单不等扫描:线程先跑起来,菜单立刻画(见 [`BackgroundScan`])。
    let mut scan = BackgroundScan::spawn(index);
    run_menu(&TOP, mode, index, &mut scan).code
}

/// 顶层菜单的循环。选中即执行,`Leave`/Esc 退出,带着最后一条命令的退出码
/// 回去——裸 `duster` 的退出码是它跑过的最后一件事。返回 [`Outcome`] 与
/// 动作函数同形(菜单自己的退场恒为 silent:菜单屏是可再生画面,没什么
/// 可停的),`run` 只取 `code`。
///
/// 清屏只在菜单 ↔ 命令的边界上,而且分两种:执行命令前擦掉菜单屏
/// ([`clear_menu_screen`]),命令收场后把屏上的东西滚进 scrollback 再擦
/// ([`scroll_out_and_clear`])。**停不停由命令的 [`Outcome`] 说了算**:
/// 真跑出结果的收场先停一道按键([`pause_for_menu`]);Esc 一路取消的
/// 收场直接回菜单——交互屏都自己擦干净了,空屏上拦按键就是白拦。滚屏
/// 两种收场都做:silent 收场时光标就在顶上,滚出去的至多一两行空白,
/// 不值得为省它们再分一条路径。`drill` / `browse` 的列表→详情循环不在
/// 这一层,一条清屏都不会发,光标带回原行的契约原样保留。
/// 选中后的回执(`menu_pick` 印的那一行)会被执行前的清屏擦掉:那是旧菜单
/// 屏幕的最后一口气,命令输出本身就是选择反馈,不必再留一句「✔ scan」。
/// 头部([`draw_header`])每一轮重画一次:清屏之后菜单从顶部起,招牌就是
/// 这一屏的门面,而不再是启动时印一次、之后滚走的 banner。它占的行数当场
/// 申报给 [`menu_pick_reserving`],否则矮终端里菜单尾巴会被顶出屏幕。
///
/// 后台索引扫描([`BackgroundScan`])横跨这整个循环:每一轮把「还在扫吗」
/// 现问现画进头部,选中一条命令时才 join。
fn run_menu(
    menu: &Menu,
    mode: OutputMode,
    index: Option<&Path>,
    scan: &mut BackgroundScan,
) -> Outcome {
    let items = rendered_items(menu.items);
    let mut last_code = EXIT_OK;
    loop {
        let above = draw_header(scan.running());
        match menu_pick_reserving(menu.prompt, &items, 0, above) {
            Ok(Some(i)) => match menu.items[i].route {
                Route::Run(f) => {
                    // 命令要读索引,这里才把后台扫描收回来(整趟只等这一次)。
                    // 放在清屏之前:那句「还在等」留在菜单下面,清屏之后屏幕
                    // 从顶部起就全是命令输出。
                    scan.join();
                    // 执行命令前:屏上只有菜单,擦掉就是了,命令输出从顶部开始。
                    clear_menu_screen();
                    let out = f(mode, index);
                    last_code = out.code;
                    // 只有真跑出结果的收场才拦一道按键;`q` 连菜单都不回,
                    // 直接退出。Esc 一路取消的收场屏上没有没读过的东西,
                    // 一次 Esc 一层,不多按。
                    if out.pause && !pause_for_menu() {
                        return Outcome::silent(last_code);
                    }
                    // 屏上的东西先滚进 scrollback,再擦、再重绘菜单。
                    scroll_out_and_clear();
                }
                Route::Leave => return Outcome::silent(last_code),
            },
            // Esc / Ctrl-C:与 quit 同一个出口。
            Ok(None) => return Outcome::silent(last_code),
            Err(_) => return Outcome::silent(EXIT_ERROR),
        }
    }
}

/// 命令跑完之后停一下:结果留在屏上,按键才回菜单。返回 `false` = 用户按了
/// `q`,连菜单都不必回,直接退出。
///
/// 没有这一停,`run_menu` 会在命令返回的同一瞬间清屏重绘菜单,用户的阅读
/// 窗口是 0 毫秒——而 duster 的全部价值就在那几行结果里,下一步很可能还是
/// `clean` / `prune`。mole 在同一个位置也停(`Press Enter to return to the
/// app list, press q to exit`),这不是客套,是「先看清楚再动手」的同一条规矩。
///
/// 只认两个出口(Enter / Esc / 空格 回菜单,`q` 退出),其余键忽略:手滑
/// 不该改变去向。`read_key` 出错(stdin 没了、被信号打断)当退出处理,别
/// 在这里空转。
fn pause_for_menu() -> bool {
    let term = Term::stderr();
    if !term.is_term() {
        return true;
    }
    eprintln!();
    eprint!("  {}", style("Enter back to the menu · q quit").dim());
    let back = loop {
        match term.read_key() {
            Ok(Key::Enter | Key::Escape | Key::Char(' ')) => break true,
            Ok(Key::Char('q' | 'Q')) => break false,
            Ok(_) => continue,
            Err(_) => break false,
        }
    };
    // 提示行擦掉再走:它是问句,不是结果,不该跟着结果一起进 scrollback。
    let _ = term.clear_line();
    back
}

/// 菜单屏的清屏 + 归位:直接擦。走 stderr,非 TTY 直接跳过。
///
/// 用在「屏上只有菜单」的两个位置(执行命令前、从子菜单回来)。菜单是可再生
/// 的画面,进 scrollback 只是噪音,擦掉最干净。
///
/// # 为什么是 2J,不是 22J
///
/// `CSI 22 J`(擦屏但把内容存进 scrollback)是 kitty 扩展,而 ghostty 的 ED
/// 文档写死了「`n` 只有 0/1/2/3 有效,其它值**整条不执行**」——发过去等于
/// 没清屏,菜单会直接画在上一轮画面上。`CSI 2 J` 是 VT100 就有的,任何终端、
/// tmux / screen 都认。要保内容的地方另有办法,见 [`scroll_out_and_clear`]。
fn clear_menu_screen() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    // console 的 clear_screen 是 `\r ESC[2J \r ESC[H`:前导 CR 只是把光标
    // 挪回本行行首,对整屏擦除没有影响,与 mole 的 `\033[2J\033[H` 同语义。
    let _ = Term::stderr().clear_screen();
}

/// 结果屏的清屏 + 归位:**先把这一屏滚进 scrollback**,再擦、再回左上角。
///
/// `ESC[2J` 擦掉的内容在 ghostty / kitty 上不会进 scrollback,上滚也找不回来
/// (ghostty#905;iTerm2 / Terminal.app 不丢)。命令结果那一屏要是这么擦,
/// 用户刚跑完的 `scan` 末尾那几行「共回收多少」就永久没了——菜单顶对齐得
/// 再准也没意义。
///
/// 保内容的标准答案是 kitty 的 `CSI 22 J`,但 ghostty 的 ED 文档明确不认
/// (见 [`clear_menu_screen`]),tmux 之类更不认。所以用最土也最通用的办法:
/// 吐 `rows` 个换行。光标在第 r 行时,前 `rows - r` 个 LF 只是把光标往下挪,
/// 后 r 个才真的滚屏——滚出去的行按终端的常规规则进 scrollback(scrollback
/// 本来就是这么攒出来的),既不多推一行空白,也不依赖任何扩展。末尾补一次
/// `2J`:正常情况下屏已经空了,这一下是空转;万一命令把光标留在了内容中间,
/// 它负责收尾。
fn scroll_out_and_clear() {
    let term = Term::stderr();
    if !term.is_term() {
        return;
    }
    let _ = term.write_str(&scroll_out_sequence(term.size().0));
}

/// [`scroll_out_and_clear`] 发出去的那串字节。单独一个函数是为了能测:
/// 「LF 的条数等于屏高」是这套做法唯一的承重点,少一条就少滚一行,
/// 那一行就是被销毁的那一行。
fn scroll_out_sequence(rows: u16) -> String {
    let rows = usize::from(rows.max(1));
    let mut out = String::with_capacity(rows + 8);
    for _ in 0..rows {
        out.push('\n');
    }
    out.push_str("\x1b[2J\x1b[H");
    out
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
    menu_pick_reserving(prompt, items, start, 0)
}

/// 同 [`menu_pick`],外加一句「调用方在我上面已经印了 `above` 行」。
///
/// 菜单屏的头部就是这么申报进来的:页高只能由知道自己印了几行的人来定,
/// 而 banner 时代那句「头部滚出屏幕无害」在清屏顶对齐之后不再成立——头部
/// 现在和菜单同处一屏,不申报进 [`CHECKLIST_RESERVED`],控件就会照着整个
/// 终端高度铺开,把自己的尾巴顶出屏幕,退场时 `clear_last_lines` 也擦不回来。
fn menu_pick_reserving(
    prompt: &str,
    items: &[String],
    start: usize,
    above: usize,
) -> Result<Option<usize>, ()> {
    let theme = menu_theme();
    let picked = prompt_pick(
        &theme,
        Picker {
            prompt,
            header: None,
            items,
            page: crate::browse::viewport(CHECKLIST_RESERVED + above),
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

/// 招牌:`duster` 的点阵块字。`░` 是 25% 网点,做「尘」;`█▀▄` 是实心笔画,
/// 做「字」——两者分色([`wordmark_line`]),招牌本身就是这工具在干的事:
/// 把字从尘里擦出来。
///
/// 写死成三行字面量而不是运行时排版:字形是设计资产,不是算出来的东西。
const WORDMARK: [&str; 3] = [
    "░█▀▄░█░█░█▀▀░▀█▀░█▀▀░█▀▄",
    "░█░█░█░█░▀▀█░░█░░█▀▀░█▀▄",
    "░▀▀░░▀▀▀░▀▀▀░░▀░░▀▀▀░▀░▀",
];

/// 招牌以下要留给菜单的余地:低于这个高度就不挂招牌,退回一行字。
/// 招牌是锦上添花,不能把菜单本身挤到强制翻页。
const WORDMARK_MIN_ROWS: usize = 20;

const TAGLINE: &str = "See and clean up what your AI coding agents leave on disk";
const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// 后台扫描还没跑完时头部多印的那一行。**静态一行,不是动画**,理由见
/// [`BackgroundScan`]:第二个线程会和阻塞在 `read_key` 上的选择控件抢光标。
///
/// 短是刻意的:装不下的行会被 [`header_lines`] 整行丢掉(折行就毁了
/// 「行数 = 高度」),而这一行在 12 列的终端上也得留得住。
const SCANNING_LINE: &str = "indexing…";

/// 招牌的一行:`░`(尘)压暗,`█▀▄`(字)走 cyan bold。
///
/// 按「是不是 ░」分段上色,一段一次转义序列;逐字符上色会把 24 个字符的
/// 一行撑成十几倍长,还会在不支持颜色的地方留下满地噪音。
fn wordmark_line(row: &str) -> String {
    let dust = Style::new().dim();
    let ink = Style::new().cyan().bold();
    let mut out = String::new();
    let mut run = String::new();
    let mut run_is_dust = None;
    for ch in row.chars() {
        let is_dust = ch == '░';
        if run_is_dust != Some(is_dust) {
            if let Some(prev) = run_is_dust {
                let style = if prev { &dust } else { &ink };
                out.push_str(&style.apply_to(&run).to_string());
                run.clear();
            }
            run_is_dust = Some(is_dust);
        }
        run.push(ch);
    }
    if let Some(prev) = run_is_dust {
        let style = if prev { &dust } else { &ink };
        out.push_str(&style.apply_to(&run).to_string());
    }
    out
}

/// 键位提示。词表与顺序全菜单统一(move · page · Enter <动词> · Esc <去向>);
/// 顶层菜单的 Esc 是退出程序,所以写 quit 而不是 back——提示行必须说实话。
/// 组标记(`›`)随二层菜单一起撤了,这一行不再有条件分支。
const KEYLINE: &str = "↑/↓ move · ←/→ page · Enter run · Esc quit";

/// 头部那一整块,每个元素是已经上好色的一行。**行数就是它的高度**:画的人
/// 与算页高的人读同一个 `Vec`,对不上是不可能的。
///
/// 两档,按屏幕给得起多少行、多少列往下退:
/// - 屏幕撑得住:招牌三行(版本贴在基线右侧)+ 一句话 + 键位;
/// - 屏幕矮或窄:退回一行 `duster v0.1.0`。
/// (面包屑那一档随二层菜单一起撤了:菜单只剩顶层这一屏。)
///
/// `scanning` = 后台索引扫描还没跑完,键位那一行下面多印一行 [`SCANNING_LINE`]。
/// 它和别的行一样算进这个 `Vec` 的长度,于是自动被申报进页高——扫描中的菜单比
/// 扫完的矮一行,尾巴不会被顶出屏幕。
///
/// 任何一行都不许宽过终端:折行会让「行数 = 高度」这条等式失效,页高一算错,
/// `clear_last_lines` 就擦不干净(browse.rs 的 `browse_一屏不溢出` 记着这笔账)。
/// 装不下的行直接不印——标语和键位是锦上添花,招牌那一行另有短版可退。
fn header_lines(rows: usize, cols: usize, scanning: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(8);
    let mut add = |plain: &str, styled: String| {
        if plain.is_empty() || console::measure_text_width(plain) <= cols {
            out.push(styled);
        }
    };
    add("", String::new());
    if rows >= WORDMARK_MIN_ROWS
        && console::measure_text_width(&format!("  {}  {VERSION}", WORDMARK[0])) <= cols
    {
        for (i, row) in WORDMARK.iter().enumerate() {
            // 版本号贴在最后一行右侧:像 logotype 的注册标记落在基线上,
            // 而不是浮在招牌顶上抢第一眼。
            let tail = if i + 1 == WORDMARK.len() {
                format!("  {VERSION}")
            } else {
                String::new()
            };
            add(
                &format!("  {row}{tail}"),
                format!(
                    "  {}{}",
                    wordmark_line(row),
                    if tail.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", style(VERSION).dim())
                    }
                ),
            );
        }
        add("", String::new());
    } else {
        add(
            &format!("  duster {VERSION}"),
            format!(
                "  {} {}",
                style("duster").cyan().bold(),
                style(VERSION).dim()
            ),
        );
    }
    add(
        &format!("  {TAGLINE}"),
        format!("  {}", style(TAGLINE).dim()),
    );
    add(
        &format!("  {KEYLINE}"),
        format!("  {}", style(KEYLINE).dim()),
    );
    if scanning {
        add(
            &format!("  {SCANNING_LINE}"),
            format!("  {}", style(SCANNING_LINE).dim()),
        );
    }
    add("", String::new());
    out
}

/// 把头部画到 stderr,交回它占了几行——调用方要把这个数字申报给页高。
///
/// `scanning` 由 [`BackgroundScan::running`] 现场问出来:每一轮循环重画一次,
/// 扫描一结束,下一次重画时那一行就没了。
fn draw_header(scanning: bool) -> usize {
    let (rows, cols) = Term::stderr().size();
    let lines = header_lines(usize::from(rows), usize::from(cols), scanning);
    for line in &lines {
        eprintln!("{line}");
    }
    lines.len()
}

/// 菜单主题:条目自带颜色,把选中态的整行覆盖色关掉(否则会盖住条目色),
/// 只留高亮箭头前缀标记当前行。
fn menu_theme() -> ColorfulTheme {
    ColorfulTheme {
        active_item_style: Style::new(),
        ..ColorfulTheme::default()
    }
}

/// 预渲染菜单行:命令名各自着色、对齐,说明置灰、按余量截断。
///
/// **说明要截断**:菜单是全项目唯一一张不走 [`Table`] 的列表,行宽没人算过。
/// 而它和别的列表一样由 `prompt_pick` 画、按逻辑行计数、按物理行擦——行宽
/// 碰到终端宽就折成两个物理行,残影每按一次翻一倍(那段病史见 [`Prefix`])。
/// 早先 `skill` 那条说明 94 字符、整行 108 列,80 列终端上必折;靠人写文案时
/// 自觉数字数是守不住的,所以在这里按 [`Prefix::Cursor`] 的预算兜住。
fn rendered_items(items: &[MenuItem]) -> Vec<String> {
    rendered_items_at(items, term_width())
}

/// 宽度算法本体;`cols` 由测试直接给,不必 mock 终端。
///
/// 预算与 [`Table::rows_at`] 同一把尺子:`cols - prefix - 1`,末尾那 1 列是
/// 不许折行的余量。名字列一格不让(它是用户要认的东西),被截的永远是说明。
fn rendered_items_at(items: &[MenuItem], cols: usize) -> Vec<String> {
    let width = items
        .iter()
        .map(|m| display_width(m.name))
        .max()
        .unwrap_or(0);
    // 名字列 + 两空格,说明从这里开始(组标记那一列随二层菜单撤了)。
    let head = width + 2;
    let desc_w = cols
        .saturating_sub(Prefix::Cursor.width() + 1 + head)
        .max(1);
    items
        .iter()
        .map(|m| {
            let name = (m.color)(&Style::new().bold()).apply_to(format!("{:<width$}", m.name));
            // 先截后着色:反过来会剪断 ANSI 序列(与 `Table::render_line` 同理)。
            let desc = truncate_width(m.desc, desc_w);
            format!("{name}  {}", style(desc).dim())
        })
        .collect()
}

/// 顶层的 `status`:表 + 足迹,打完就回菜单。
///
/// 体检已随上一轮整体撤下,这一屏没有下钻可给:以前那行问题摘要连同它的
/// 动作菜单(`see the N issues`)一起没了——status 不再报发现,退出码也只留
/// 0/失败,「看那 N 条」这个动作没有对象了。剩下的是纯转发:菜单入口一律
/// 叫 `run_*`,路由表里一眼能数清顶层能跑到哪几个地方。
fn run_status(mode: OutputMode, index: Option<&Path>) -> Outcome {
    Outcome::shown(cmd_status(mode, index))
}

/// 顶层的 `doctor`:自检 duster 自己(索引库、adapter 清单、目录权限、版本)。
///
/// 命令已经不收任何旗标——他检那六项连同它们的开关(`--no-secrets` /
/// `--ping` / `--agent` / `--check`)一起搬去了 `status`,菜单也就没什么可问的,
/// 选中即跑。这一层现在是纯转发,留着它是因为菜单入口一律叫 `run_*` /
/// `prompt_*`:路由表里一眼能数清顶层能跑到哪几个地方。
fn run_doctor(mode: OutputMode, index: Option<&Path>) -> Outcome {
    Outcome::shown(cmd_doctor(mode, index))
}

// ---------------------------------------------------------------------------
// 统一列表流:列表 → 详情 / 勾选批量 → 回列表
// ---------------------------------------------------------------------------

/// 一趟下钻/批量动作的收场:退出码 + 这一屏的数据有没有被改过。
///
/// `changed` 只在一个动作**真的删了行**时为 true(详情里的 delete、勾选
/// 批量里的 delete each)。数据变了,列表就必须在下一轮重取——列表不许
/// 继续列着已删的行,那和指着空座位让用户入座没有区别。
#[derive(Clone, Copy)]
struct DrillOutcome {
    code: i32,
    changed: bool,
}

impl From<i32> for DrillOutcome {
    fn from(code: i32) -> Self {
        Self {
            code,
            changed: false,
        }
    }
}

/// 统一列表流的循环骨架:浏览一张表 → 下钻看一条(或对勾选集合动手)→
/// 回到原来那一行接着看。
///
/// 早先每张列表都是一次性的:下钻一条,详情打完,直接弹回顶层菜单。想看
/// 第二条就得从头再来一遍——重新出表、重新翻到那一页。而「挨个看看」
/// 恰恰是列表类命令唯一的用法(`mcp list` 十几个服务器,用户是来逐个核对
/// 声明的,不是来选一个就走的)。
///
/// 四件事由这里统一,不许各写各的:
/// - **光标带回来**。`browse` 持有调用方的下标,回列表时还停在那一行。
/// - **勾选批量**。Space 勾、Enter 进批量动作菜单;`act` 交 `None` 表示
///   用户在动作菜单上按了 Esc(勾选原样留着,回列表),交 `Some(code)`
///   表示动作跑完(勾选已被消费,这里清空——下一次 Enter 不该再对着
///   旧集合动手)。
/// - **退出码累积**。`base` 是列表自己那一档(warning 就是 `EXIT_PARTIAL`),
///   每趟下钻/批量的结果用 [`worse`] 并进去:看了五条,有一条报错,整趟
///   就不是 0。累积用 `worse` 而不是 `max`——退出码数值本身无序(3「部分
///   成功」比 4「什么都没做」更坏),取大会把错误吞成「没做」。
/// - **删完重取**。`detail` / `act` 交回 [`DrillOutcome`] 时带 `changed`,
///   这一轮结束时用 `fetch` 重取整张表(换掉 `value`),下一轮浏览的就是
///   新数据。`fetch` 失败不打断浏览:旧列表照常能翻,删成的结果不该被
///   一句重取报错盖掉。
/// - **Esc 只退一层**。过滤态的 Esc 先清过滤(browse 内部的梯子),列表上
///   的 Esc 才回菜单,详情里的 Esc 由 `detail` 自己处理成「回列表」。
///
/// 非 TTY 时 `browse` 直接给 Quit,原样返回 `base`——浏览始终是 TTY 上的
/// 增益而不是新契约。
///
/// 列表命令共用的外壳(错误、空表)也在这里:错误与空表都算「印了结果」——
/// 那一两行说明就是这趟的全部产出,不拦一道按键它就会在 0 毫秒内被菜单
/// 盖掉;浏览退场是 silent——屏上的一切都是用户自己翻出来的,Esc 一次回
/// 菜单,不再多按。
fn list_drill<T, IsEmpty, Empty, Rows, Detail, Act, Fetch>(
    mode: OutputMode,
    command: &str,
    result: anyhow::Result<T>,
    base: i32,
    is_empty: IsEmpty,
    empty: Empty,
    rows: Rows,
    prompt: &str,
    detail: Detail,
    act: Act,
    fetch: Fetch,
) -> Outcome
where
    IsEmpty: Fn(&T) -> bool,
    Empty: Fn(&T),
    Rows: Fn(&T) -> (String, Vec<String>),
    Detail: FnMut(&T, usize) -> DrillOutcome,
    Act: FnMut(&T, &[usize]) -> Option<DrillOutcome>,
    Fetch: FnMut() -> anyhow::Result<T>,
{
    let mut value = match result {
        Ok(value) => value,
        Err(error) => return Outcome::shown(fail(mode, command, &error)),
    };
    if is_empty(&value) {
        empty(&value);
        return Outcome::shown(base);
    }
    let mut code = base;
    let mut cursor = 0;
    let mut checked: Vec<bool> = Vec::new();
    let mut filter: Option<String> = None;
    let mut detail = detail;
    let mut act = act;
    let mut fetch = fetch;
    loop {
        let (header, labels) = rows(&value);
        if checked.len() != labels.len() {
            // 数据被换过(删过行):勾选对着旧行打的,一律清空——一张藏着
            // 勾选的表是张说谎的表。删完重取的 `value` 也可能让列表空掉,
            // 那就说一句空表话、收场,不留在空屏上。
            checked = vec![false; labels.len()];
        }
        match browse(prompt, &header, &labels, &mut cursor, &mut checked, &mut filter) {
            Browsed::Open(i) => {
                let o = detail(&value, i);
                code = worse(code, o.code);
                if o.changed {
                    match fetch() {
                        Ok(v) => {
                            value = v;
                            cursor = cursor.min(labels.len().saturating_sub(1));
                            if is_empty(&value) {
                                empty(&value);
                                return Outcome::silent(code);
                            }
                        }
                        Err(_) => {}
                    }
                }
            }
            Browsed::Act(sel) => {
                if let Some(o) = act(&value, &sel) {
                    code = worse(code, o.code);
                    if o.changed {
                        match fetch() {
                            Ok(v) => {
                                value = v;
                                cursor = cursor.min(labels.len().saturating_sub(1));
                                if is_empty(&value) {
                                    empty(&value);
                                    return Outcome::silent(code);
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    checked.iter_mut().for_each(|b| *b = false);
                }
            }
            Browsed::Quit => return Outcome::silent(code),
        }
    }
}

// ---------------------------------------------------------------------------
// session / memory / mcp / skill:四组名词,统一列表流
// ---------------------------------------------------------------------------

// `session show` / `mcp ping` 的默认值与命令行同一个常量,不再照抄。
// 抄错的代价是菜单里印的行数与命令行不一样,不会更危险,但一样是谎。
// `session list` 不在此列:菜单要全表(理由见下面 docstring),命令行那份
// 20 是它自己的领地。

/// `session list` → 进组直接列表:全部会话直接开浏览,不再先问范围——
/// `/` 过滤就是范围问句的替身,子串既能滤 agent 也能滤项目路径。选中即
/// [`session_detail`] 下钻;勾选后 Enter 进批量动作(逐条导出,见
/// [`session_bulk`])。
fn session_list(mode: OutputMode, index: Option<&Path>) -> Outcome {
    // fetch 闭包是删除后的重取来源:删完一行,列表不许继续列着它。
    let filter = SessionFilter {
        agents: Vec::new(),
        project: None,
        older_than_days: None,
        min_bytes: None,
        limit: 0,
        now_ms: None,
    };
    let list = match session::list(index, &filter) {
        Ok(l) => l,
        Err(error) => return Outcome::shown(fail(mode, "session-list", &error)),
    };
    // 源库打不开就明说,列表照常展示能读的——与 memory list 同一条规矩。
    let base = if list.warnings.is_empty() {
        EXIT_OK
    } else {
        EXIT_PARTIAL
    };
    render_warnings(&list.warnings);
    let rows = list.rows;
    list_drill(
        mode,
        "session-list",
        Ok(rows),
        base,
        |rows| rows.is_empty(),
        |_| {
            println!();
            println!(
                "  {}",
                muted().apply_to(
                    "No conversations matched. If that looks wrong, run `duster scan` first."
                )
            );
        },
        |rows| session_list_rows(rows),
        "Which conversation",
        |rows, i| session_detail(mode, index, &rows[i]),
        |rows, sel| session_bulk(mode, index, rows, sel),
        || session::list(index, &filter).map(|l| l.rows),
    )
}

/// 勾选集合的批量动作:逐条导出或逐条删除。菜单只发真实存在的参数组合——
/// 每一条都是一次真实的 `session export` / `session rm`,循环只是替用户
/// 少敲几遍。
///
/// `None` = 在动作菜单上按了 Esc、或 delete 的确认被拒绝(勾选保留,回列表);
/// `Some(code)` = 动作跑完(勾选由 [`list_drill`] 清掉)。delete 跑完时
/// 交回 `changed`,让列表重取——不许继续列着已删的行。
fn session_bulk(
    mode: OutputMode,
    index: Option<&Path>,
    rows: &[SessionRow],
    sel: &[usize],
) -> Option<DrillOutcome> {
    let actions = [
        "export each as markdown".to_string(),
        "export each as json".to_string(),
        "delete each (a readable copy is archived first)".to_string(),
    ];
    let pick = match menu_pick(&format!("{}: what next", crate::plural(sel.len(), "conversation")), &actions, 0) {
        Ok(Some(i)) => i,
        // Esc:回列表,勾选留着。
        _ => return None,
    };
    if pick < 2 {
        // 导出:out 恒为 None(写到 stdout),与详情动作同一个口径。
        let format = if pick == 0 {
            cmd::session::Format::Markdown
        } else {
            cmd::session::Format::Json
        };
        let mut code = EXIT_OK;
        for &i in sel {
            code = worse(
                code,
                cmd::session::run(
                    mode,
                    index,
                    cmd::session::SessionCmd::Export {
                        rid: rows[i].rid,
                        format,
                        out: None,
                    },
                ),
            );
        }
        return Some(DrillOutcome::from(code));
    }

    // delete each:多选时确认句必须报条数与合计字节——用户同意的是一件
    // 具体大小的事,不是一句含糊的「删掉这些」。
    let total: u64 = sel.iter().map(|&i| rows[i].bytes).sum();
    match prompt_yes_no(
        &format!(
            "Delete {} ({}), archiving a readable copy of each into \
             ~/agent-duster-exports/ first?",
            crate::plural(sel.len(), "conversation"),
            human_bytes(total)
        ),
        false,
    ) {
        Ok(Some(true)) => {
            let mut code = EXIT_OK;
            let mut changed = false;
            for &i in sel {
                let c = cmd::session::run(
                    mode,
                    index,
                    cmd::session::SessionCmd::Rm {
                        id: rows[i].rid.to_string(),
                        no_archive: false,
                        dry_run: false,
                    },
                );
                code = worse(code, c);
                if c != EXIT_ERROR {
                    changed = true;
                }
            }
            Some(DrillOutcome { code, changed })
        }
        // n / Esc / 终端拿不到:算了,不动盘。勾选留着,回列表。
        _ => None,
    }
}

/// `session list` 的浏览行:列与 `cmd::session::render_list` 一致(ID / AGENT /
/// PROJECT / TURNS / SIZE / LAST USED),单元格口径直接复用那两份
/// ([`cmd::session::project_cell`] / [`cmd::session::size_cell`])——同一段
/// 代码抄两遍、靠注释维持一致不是一致,这里只留 `cmd/session.rs` 那一份。
fn session_list_rows(rows: &[SessionRow]) -> (String, Vec<String>) {
    session_list_rows_at(rows, term_width())
}

/// 宽度算法本体;`cols` 由测试直接给(60 / 80 / 200),不必 mock 终端。
/// 前缀预算是 [`Prefix::Checkbox`]:统一列表流的浏览表每行画 `❯ [x] `。
fn session_list_rows_at(rows: &[SessionRow], cols: usize) -> (String, Vec<String>) {
    let mut table = Table::new(vec!["ID", "AGENT", "PROJECT", "TURNS", "SIZE", "LAST USED"]);
    table.right_align(&[0, 3, 4]);
    // LAST USED 吃余量:它天然就短(「3d ago」这个量级),平时原样给,
    // 只有整行逼近终端宽时才截它——不设总宽上限的话,没有人算过整行,
    // PROJECT 截到 28 也只是把问题挪到别处。地板 4 与旧版手算同数。
    table.flex_col(5);
    for r in rows {
        table.push_row(vec![
            // ID 带 `s` 前缀(会话 id,与 search 的 `t<tid>` 区分开),与
            // `cmd::session::render_list` 同一口径——两个入口印同一场会话,
            // id 的写法也必须同一种。
            format!("s{}", r.rid),
            r.agent_id.clone(),
            cmd::session::project_cell(r.cwd.as_deref()),
            r.turns.to_string(),
            cmd::session::size_cell(r.bytes, r.compressed),
            crate::relative_time(r.last_turn_ms),
        ]);
    }
    table.rows_at(Prefix::Checkbox, cols)
}

/// 一场会话的下钻:先折叠视图(与命令行默认同档)扫一遍,再在同一行上
/// 给上下文动作(全文 / 两种导出 / migrate / 两种 delete)。
///
/// `--full` 是菜单以前唯一够不着又最痛的能力——折叠视图是**扫**一场会话的
/// 形态,而用户扫完往往正是要读全文,让他退出菜单去敲一条带 rid 的命令是
/// 最没道理的收场。这里把全文、两种导出都摆在同一个菜单里,动作跑完回
/// 列表接着看下一条(`list_drill` 的下一轮),退出码与折叠视图那趟用 [`worse`]
/// 并起来——看了五条有一条报错,整趟就不是 0。
///
/// Esc 回列表,一次一层(back 条目已撤)。默认光标落第 0 格(全文,只读):
/// 写类动作(导出灌 stdout、migrate 动别人家的盘、两条 delete 删数据)排
/// 在后面,而且两条 delete 各有一句 y/N 确认(默认 No),回车不会一击写盘。
///
/// 导出 `out` 恒为 None(写到 stdout):在提示符后面拼文件路径是 shell 的活,
/// 菜单不抢。全文那档 `limit: 0` + `full: true`:`limit` 0 = 全部轮次、
/// `--full` = 工具轮不折叠、正文不截 40 行,两条都是 `SessionCmd::Show`
/// 里「要全部」的表达(见 `cmd::session::run_show`)。
///
/// 返回 [`DrillOutcome`]:delete 真删了时带 `changed`,让列表重取。
fn session_detail(mode: OutputMode, index: Option<&Path>, row: &SessionRow) -> DrillOutcome {
    let rid = row.rid;
    let mut code = cmd::session::run(
        mode,
        index,
        cmd::session::SessionCmd::Show {
            rid,
            limit: cmd::session::DEFAULT_SHOW_LIMIT,
            full: false,
        },
    );
    let actions = session_detail_actions();
    match menu_pick(&format!("Conversation {rid}: what next"), &actions, 0) {
        Ok(Some(i)) => {
            match SESSION_DETAIL_ACTIONS[i] {
                SessionDetailAction::Full => code = worse(
                    code,
                    cmd::session::run(
                        mode,
                        index,
                        cmd::session::SessionCmd::Show {
                            rid,
                            limit: 0,
                            full: true,
                        },
                    ),
                ),
                SessionDetailAction::ExportMarkdown => code = worse(
                    code,
                    cmd::session::run(
                        mode,
                        index,
                        cmd::session::SessionCmd::Export {
                            rid,
                            format: cmd::session::Format::Markdown,
                            out: None,
                        },
                    ),
                ),
                SessionDetailAction::ExportJson => code = worse(
                    code,
                    cmd::session::run(
                        mode,
                        index,
                        cmd::session::SessionCmd::Export {
                            rid,
                            format: cmd::session::Format::Json,
                            out: None,
                        },
                    ),
                ),
                SessionDetailAction::Migrate => {
                    code = worse(code, session_migrate_action(mode, index, rid));
                }
                // 两条 delete 共用同一个 helper,唯一的差别是 `no_archive`:
                // 菜单不留第二套业务逻辑。
                SessionDetailAction::DeleteArchived | SessionDetailAction::DeleteForGood => {
                    let no_archive =
                        matches!(SESSION_DETAIL_ACTIONS[i], SessionDetailAction::DeleteForGood);
                    let (c, deleted) = session_delete_action(mode, index, row, no_archive);
                    code = worse(code, c);
                    if deleted {
                        // 真删了:列表下一轮必须重取,不许继续列着这一行。
                        return DrillOutcome { code, changed: true };
                    }
                }
            }
        }
        // Esc:回列表,折叠视图那趟的退出码原样带走。
        _ => {}
    }
    DrillOutcome::from(code)
}

/// 详情动作里的 delete:菜单把「删」拆成两条并列动作——归档后删与真删,
/// 唯一的差别是 `no_archive`。会话是不可再生的用户内容,删之前默认留一份
/// 可读副本是对的;但用户明确知道自己在扔什么的时候,工具不该强塞一个他
/// 还得自己去清的包——所以两条并列,业务逻辑只有一份。两条都走
/// `cmd::session::run` 的同一条 `Rm` 通路,回执、退出码、报错文案与命令行
/// 逐字相同。
///
/// 确认句由 `no_archive` 决定:归档删亮出路径、体积与归档去处;真删点明
/// 没有退路。两句默认答案都是 No。
///
/// 返回 (退出码, 是否真删了)。真删了才让列表重取;拒绝 / Esc / 终端拿
/// 不到 = 不动盘,不是错误,也不触发重取。
fn session_delete_action(
    mode: OutputMode,
    index: Option<&Path>,
    row: &SessionRow,
    no_archive: bool,
) -> (i32, bool) {
    let prompt = if no_archive {
        format!(
            "Delete this conversation ({path} · {size}) for good? No archive is made — \
             the conversation is gone",
            path = display_tilde(Path::new(&row.path)),
            size = human_bytes(row.bytes)
        )
    } else {
        format!(
            "Delete this conversation ({path} · {size})? A readable copy is archived into \
             ~/agent-duster-exports/ first",
            path = display_tilde(Path::new(&row.path)),
            size = human_bytes(row.bytes)
        )
    };
    match prompt_yes_no(&prompt, false) {
        Ok(Some(true)) => {
            let code = cmd::session::run(
                mode,
                index,
                cmd::session::SessionCmd::Rm {
                    id: row.rid.to_string(),
                    no_archive,
                    dry_run: false,
                },
            );
            (code, code != EXIT_ERROR)
        }
        // n / Esc / 终端拿不到,都算「算了」:不动盘不是错误。
        _ => (EXIT_OK, false),
    }
}

/// 详情屏的动作。菜单行文本与动作一一对应;back 没有条目——Esc 就是
/// 返回,一屏一次,菜单里再放一条 back 等于给同一扇门装两个把手。
/// migrate 的实现在 [`session_migrate_action`],delete 的实现在
/// [`session_delete_action`]。
///
/// 为什么只有会话(和记忆,见 `memory_detail_actions`)配得上「两条
/// delete」:它们是不可再生的用户内容,删之前默认留一份归档是对的;但
/// 用户明确知道自己在扔什么的时候,工具不该强塞一个他还得自己去清的包。
/// 两条并列,唯一的差别是 `no_archive`,业务逻辑只有一份。
#[derive(Clone, Copy)]
enum SessionDetailAction {
    Full,
    ExportMarkdown,
    ExportJson,
    Migrate,
    DeleteArchived,
    DeleteForGood,
}

impl SessionDetailAction {
    /// 菜单行文本,顺序即菜单顺序。
    fn label(self) -> &'static str {
        match self {
            SessionDetailAction::Full => "read it in full (every turn, nothing truncated)",
            SessionDetailAction::ExportMarkdown => "export as markdown",
            SessionDetailAction::ExportJson => "export as json",
            SessionDetailAction::Migrate => {
                "plant a text-only copy into another agent (tool calls are not carried over)"
            }
            SessionDetailAction::DeleteArchived => {
                "delete it (a readable copy is archived into ~/agent-duster-exports/ first)"
            }
            SessionDetailAction::DeleteForGood => {
                "delete it for good (no archive — the conversation is gone)"
            }
        }
    }
}

/// 顺序即菜单顺序;[`session_detail`] 按下标回查这一份。
const SESSION_DETAIL_ACTIONS: [SessionDetailAction; 6] = [
    SessionDetailAction::Full,
    SessionDetailAction::ExportMarkdown,
    SessionDetailAction::ExportJson,
    SessionDetailAction::Migrate,
    SessionDetailAction::DeleteArchived,
    SessionDetailAction::DeleteForGood,
];

/// 下钻动作的行文。单独成函数而不是 inline:这是对用户可见的稳定契约
/// (数量、措辞),测试要逐字核对,inline 就没有抓手。
fn session_detail_actions() -> Vec<String> {
    SESSION_DETAIL_ACTIONS
        .iter()
        .map(|a| a.label().to_string())
        .collect()
}

/// 详情动作里的 migrate:先挑目标 agent,再一句 y/N 确认,然后走
/// `cmd::session::run` 的同一条 `Migrate` 通路——菜单不留第二套业务逻辑,
/// 回执、退出码、报错文案与命令行逐字相同。
///
/// 目标名单硬编码三家,与 `duster_core::session_migrate` 的封闭词汇表是
/// 同一份事实:菜单只该把能成功的选项摆出来,让用户选一个注定报
/// `stats-only` 错的 agent 不是自由是陷阱。Esc / 拒绝确认 = 不动盘,
/// EXIT_OK 回详情。
fn session_migrate_action(mode: OutputMode, index: Option<&Path>, rid: i64) -> i32 {
    let targets: Vec<String> = ["claude-code", "codex", "omp"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let Ok(Some(i)) = menu_pick("Plant this conversation into which agent", &targets, 0) else {
        return EXIT_OK; // Esc:回详情,不动盘。
    };
    let to = &targets[i];
    // 往别人家目录写文件前的最后一道门。文案点明「有损」:工具调用不带过去,
    // 用户此刻不知情,拿副本续聊时就会被上下文缺口吓一跳。
    match prompt_yes_no(
        &format!("Plant a text-only copy into {to} (tool calls are not carried over)?"),
        false,
    ) {
        Ok(Some(true)) => cmd::session::run(
            mode,
            index,
            cmd::session::SessionCmd::Migrate {
                id: rid.to_string(),
                to: to.clone(),
                dry_run: false,
            },
        ),
        // n / Esc / 终端拿不到,都算「算了」:不动盘不是错误。
        _ => EXIT_OK,
    }
}


/// `memory list` → 进组直接列表:全部记忆直接开浏览,不再先问范围——
/// `/` 过滤就是范围问句的替身(子串滤 agent、标题、路径都行)。选中即
/// [`memory_detail`] 下钻;勾选后 Enter 进批量动作(逐条打印,见
/// [`memory_bulk`])。
fn memory_list(mode: OutputMode, index: Option<&Path>) -> Outcome {
    let all = match memory::list(index, None) {
        Ok(l) => l,
        Err(e) => return Outcome::shown(fail(mode, "memory-list", &e)),
    };
    let entries: Vec<&MemoryEntry> = all.entries.iter().collect();
    let base = if all.warnings.is_empty() { EXIT_OK } else { EXIT_PARTIAL };
    render_warnings(&all.warnings);
    let keys = cmd::memory::browse_rows(&entries).2;
    list_drill(
        mode,
        "memory-list",
        Ok(entries),
        base,
        |entries| entries.is_empty(),
        |_| {
            println!();
            println!(
                "  {}",
                muted().apply_to(cmd::memory::empty_list_message(&[], &all.entries))
            );
        },
        |entries| {
            let (header, labels, _) = cmd::memory::browse_rows(entries);
            (header, labels)
        },
        "Which memory",
        |entries, i| memory_detail(mode, index, entries[i], &keys[i]).into(),
        |_entries, sel| memory_bulk(mode, index, &keys, sel).map(DrillOutcome::from),
        || Ok(all.entries.iter().collect::<Vec<&MemoryEntry>>()),
    )
}

/// 一条记忆的详情:先 `memory show` 全文(key 是行尾那一串,原样递,不经
/// 过用户的手抄),再挂上下文动作(migrate / 两条 delete)。Esc 回列表,一次
/// 一层。
///
/// 默认光标落第 0 格:migrate 自己还有目标单选 + y/N 两道门,两条 delete
/// 各有一句 y/N 确认(默认 No),回车不会一击写盘。
fn memory_detail(mode: OutputMode, index: Option<&Path>, entry: &MemoryEntry, key: &str) -> i32 {
    // 这一屏的下一步是 migrate / delete——用户决定要不要删这条记忆,
    // 路径、体积、上次修改三个事实得先摆出来,不能回列表去数。
    println!("\n  {}", muted().apply_to(memory_meta_line(entry)));
    let mut code = cmd::memory::run(
        mode,
        index,
        &cmd::memory::MemoryCmd::Show {
            key: key.to_string(),
        },
    );
    let actions = memory_detail_actions();
    if let Ok(Some(i)) = menu_pick(&format!("{}: what next", entry.title), &actions, 0) {
        code = worse(
            code,
            match i {
                0 => memory_migrate_action(mode, index, entry),
                1 => memory_delete_action(mode, index, entry, false),
                2 => memory_delete_action(mode, index, entry, true),
                // menu_pick 的下标不会越出 actions 的长度;枚举穷尽只是
                // 把「再加一条就得接线」变成编译期提醒。
                _ => unreachable!("memory_detail_actions 只有 3 项,下标 {i} 不会出现"),
            },
        );
    }
    // Esc:回列表,show 那趟的退出码原样带走。
    code
}

/// 详情屏正文之前的元信息行:`路径 · 体积 · 上次修改`。
///
/// 单独成函数而不是 inline:这一行是用户可见的稳定契约,测试要逐字核对。
/// 路径折 `~`(与列表的 PATH 列同口径);「上次修改」与 LAST USED 列共用
/// [`relative_time`](crate::relative_time)——索引行的 mtime,没有证据印
/// `never`,绝不编 1970 年出来。
fn memory_meta_line(entry: &MemoryEntry) -> String {
    format!(
        "{} · {} · {}",
        display_tilde(&entry.path),
        human_bytes(entry.bytes),
        crate::relative_time(entry.last_used_ms),
    )
}

/// 下钻动作的行文。单独成函数而不是 inline:数量与措辞是用户可见的稳定
/// 契约,测试要逐字核对。back 没有条目——Esc 就是返回。
///
/// 为什么记忆配得上「两条 delete」(和会话同一理由,见
/// [`SessionDetailAction`]):记忆是不可再生的用户内容,删之前默认留一份
/// 归档是对的;但用户明确知道自己在扔什么的时候,工具不该强塞一个他还得
/// 自己去清的包。两条并列,唯一的差别是 `no_archive`,业务逻辑只有一份。
fn memory_detail_actions() -> Vec<String> {
    vec![
        "migrate it into another agent's memory".to_string(),
        "delete it (a readable copy is archived into ~/agent-duster-exports/ first)".to_string(),
        "delete it for good (no archive — the memory is gone)".to_string(),
    ]
}

/// 勾选集合的批量动作:逐条打印全文。每一条都是一次真实的 `memory show`,
/// 循环只是替用户少敲几遍——想把三条记忆放在一屏里对着读,不必来回下钻。
///
/// `None` = 在动作菜单上按了 Esc(勾选保留,回列表);`Some(code)` =
/// 动作跑完(勾选由 [`drill`] 清掉)。
fn memory_bulk(
    mode: OutputMode,
    index: Option<&Path>,
    keys: &[String],
    sel: &[usize],
) -> Option<i32> {
    let actions = [
        "print each in full".to_string(),
        "delete selected".to_string(),
    ];
    match menu_pick(&format!("{}: what next", crate::plural(sel.len(), "memory")), &actions, 0) {
        Ok(Some(0)) => {
            let mut code = EXIT_OK;
            for &i in sel {
                code = worse(
                    code,
                    cmd::memory::run(
                        mode,
                        index,
                        &cmd::memory::MemoryCmd::Show {
                            key: keys[i].clone(),
                        },
                    ),
                );
            }
            Some(code)
        }
        Ok(Some(1)) => memory_bulk_delete(mode, index, keys, sel),
        // Esc:回列表,勾选留着。
        _ => None,
    }
}

/// 勾选集合的批量删除。先逐条勘察归类,确认句按「duster 块 / duster 建
/// 的文件 / 你手写的文件」分开报数并报合计字节;批量里只要含手写文件
/// 就要过两道门,全是 duster 内容就一道。执行走 `memory::remove_many`:
/// 整批只打一个归档包(逐条归档会在同一秒撞名,而归档包拒绝被覆盖)。
///
/// 多块文件没法在集合级指名哪一块,跳过并说明——逐块指名是详情屏的事。
fn memory_bulk_delete(
    mode: OutputMode,
    index: Option<&Path>,
    keys: &[String],
    sel: &[usize],
) -> Option<i32> {
    let requests: Vec<RemoveRequest> = sel
        .iter()
        .map(|&i| RemoveRequest {
            key: expand_home_key(&keys[i]),
            from: None,
        })
        .collect();
    let plan = match memory::plan_batch(index, None, &requests) {
        Ok(p) => p,
        Err(e) => return Some(fail(mode, "memory-rm", &e)),
    };
    let skip_reasons: Vec<String> = plan
        .skipped
        .iter()
        .map(|s| {
            format!(
                "skipped {} — {}",
                display_tilde(std::path::Path::new(&s.key)),
                s.reason
            )
        })
        .collect();
    for line in &skip_reasons {
        eprintln!("  {}", muted().apply_to(line));
    }

    // 分开报数 + 合计字节。
    let (mut duster_blocks, mut duster_files, mut user_files) = (0usize, 0usize, 0usize);
    let mut total_bytes = 0u64;
    for item in &plan.items {
        let bytes = std::fs::metadata(item.target.path())
            .map(|m| m.len())
            .unwrap_or(0);
        total_bytes += bytes;
        match item.target {
            RemoveTarget::CutBlock { .. } => duster_blocks += 1,
            RemoveTarget::DusterFile { .. } => duster_files += 1,
            RemoveTarget::UserFile { .. } => user_files += 1,
            RemoveTarget::AmbiguousBlocks { .. } => {}
        }
    }
    if plan.items.is_empty() {
        return Some(EXIT_OK);
    }
    let mut parts: Vec<String> = Vec::new();
    if duster_blocks > 0 {
        parts.push(plural("duster block", duster_blocks));
    }
    if duster_files > 0 {
        parts.push(plural("duster-created file", duster_files));
    }
    if user_files > 0 {
        parts.push(if user_files == 1 {
            "1 file you wrote yourself".to_string()
        } else {
            format!("{user_files} files you wrote yourself")
        });
    }
    let what = join_parts(&parts);
    let count = plan.items.len();
    let noun = if count == 1 { "memory" } else { "memories" };

    // 第一道门:条数 + 归类分报 + 合计字节 + 归档去处。
    match prompt_yes_no(
        &format!(
            "Delete {count} selected {noun}? {what} · {} · will be archived to {}.",
            human_bytes(total_bytes),
            memory_archive_dir()
        ),
        false,
    ) {
        Ok(Some(true)) => {}
        _ => return Some(EXIT_OK),
    }
    // 第二道门:只要含手写文件就要过——「这是我自己写的」值得单独确认。
    if user_files > 0 {
        match prompt_yes_no(
            &format!(
                "Some of these are your own writing, not something duster put there. \
                 Delete them for good? Archived to {}.",
                memory_archive_dir()
            ),
            false,
        ) {
            Ok(Some(true)) => {}
            _ => return Some(EXIT_OK),
        }
    }

    let report = match memory::remove_many(index, None, &requests, true, false) {
        Ok(r) => r,
        Err(e) => return Some(fail(mode, "memory-rm", &e)),
    };
    let mut code = EXIT_OK;
    for r in &report.reports {
        cmd::memory::render_remove_line(r);
        code = worse(code, if r.warnings.is_empty() { EXIT_OK } else { EXIT_PARTIAL });
    }
    if let Some(a) = &report.archived {
        println!("  {}", muted().apply_to(format!("archived to {}", display_tilde(a))));
    }
    // 每份报告的 warnings 是同一趟扫描的同一个提示,去重后打一次。
    let mut seen = Vec::new();
    for r in &report.reports {
        for w in &r.warnings {
            if !seen.contains(w) {
                seen.push(w.clone());
            }
        }
    }
    if !seen.is_empty() {
        code = worse(code, EXIT_PARTIAL);
    }
    render_warnings(&seen);
    Some(code)
}

/// 带复数的小工具:1 个原样,多个加 s。只在批量确认句里用。
fn plural(singular: &str, n: usize) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

/// 逗号 + and 的英文并列:「a, b and c」。
fn join_parts(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        _ => {
            let (last, rest) = parts.split_last().unwrap();
            format!("{} and {last}", rest.join(", "))
        }
    }
}

/// 折叠的 key(带 `~/`)展开成绝对路径。批量里 `keys` 来自 browse_rows 的
/// 折叠列,而删除要的是磁盘上的真实路径。
fn expand_home_key(key: &str) -> String {
    duster_fs::path::expand_tilde(key).display().to_string()
}

/// memory 详情动作里的 migrate:先挑目标 agent,再一句 y/N 确认(亮出
/// 目标路径),然后走 `cmd::memory::migrate_execute` 的同一条通路——
/// 菜单不留第二套业务逻辑,回执、退出码、报错文案与命令行逐字相同。
///
/// 目标名单来自 `memory::migrate_targets`(清单里有非 stats-only memory
/// 资源的 agent),源 agent 自己除外:菜单只该把能成功的选项摆出来,
/// 让用户选一个注定报 stats-only 错的 agent(qoder 这类)不是自由是陷阱。
/// 目录型落点在行尾标出来:那里写的是新文件,不是往清单路径里追加。
/// Esc / 拒绝确认 = 不动盘,EXIT_OK 回详情。
fn memory_migrate_action(mode: OutputMode, index: Option<&Path>, entry: &MemoryEntry) -> i32 {
    let targets: Vec<_> = match memory::migrate_targets(None) {
        Ok(t) => t,
        Err(e) => return fail(mode, "memory-migrate", &e),
    }
    .into_iter()
    .filter(|t| t.agent_id != entry.agent_id)
    .collect();
    if targets.is_empty() {
        // 没有可选项的列表是死胡同:说一句,回详情。
        eprintln!(
            "  {}",
            muted().apply_to(
                "no other agent declares a writable memory file · nothing to migrate into"
            )
        );
        return EXIT_OK;
    }
    let rows: Vec<String> = targets
        .iter()
        .map(|t| {
            format!(
                "{} — {}{}",
                t.agent_id,
                display_tilde(&t.path),
                if t.dir { " (new file inside)" } else { "" }
            )
        })
        .collect();
    let Ok(Some(i)) = menu_pick("Migrate this memory into which agent", &rows, 0) else {
        return EXIT_OK; // Esc:回详情,不动盘。
    };
    let target = &targets[i];
    // 往别人家记忆文件写字前的最后一道门。文案点明块语义:重跑是替换,
    // 不是越堆越多——用户此刻不知道哨兵块,该在按下 y 之前知道。
    match prompt_yes_no(
        &format!(
            "Copy this memory into {} (a marked block; re-running replaces it)?",
            display_tilde(&target.path)
        ),
        false,
    ) {
        Ok(Some(true)) => cmd::memory::migrate_execute(
            mode,
            index,
            &entry.path.display().to_string(),
            &target.agent_id,
            false,
        ),
        // n / Esc / 终端拿不到,都算「算了」:不动盘不是错误。
        _ => EXIT_OK,
    }
}

/// memory 详情动作里的 delete:菜单把「删」拆成两条并列动作——归档后删与
/// 真删,唯一的差别是 `no_archive`。记忆是不可再生的用户内容(与会话同一
/// 理由,见 [`SessionDetailAction`]):删之前默认留一份归档是对的;但用户
/// 明确知道自己在扔什么的时候,工具不该强塞一个他还得自己去清的包——
/// 所以两条并列,业务逻辑只有一份。
///
/// 先勘察这份文件属于谁(duster 块 / duster 建的整份文件 / 用户手写),按
/// 归类走不同道数的确认,然后走 `cmd::memory::run` 的同一条 `Rm` 通路——
/// 菜单不留第二套业务逻辑,回执、退出码、报错文案与命令行逐字相同。
/// 确认句由 `no_archive` 决定:归档删亮出归档落点,真删点明没有退路。
///
/// 文件里有多个 duster 块时先摆子菜单挑哪一块;整删永远两道 y/N(文件
/// 同时还有 N 个 duster 块时,确认句照亮出块数)。用户手写文件归档强制
/// 是核心层的硬规则(见 duster-core 的 `memory::remove`):真删在那一支上
/// 会被核心拒绝,菜单不绕过——报错文案就是命令行那套。Esc / 拒绝确认 =
/// 不动盘,EXIT_OK 回详情。
fn memory_delete_action(
    mode: OutputMode,
    index: Option<&Path>,
    entry: &MemoryEntry,
    no_archive: bool,
) -> i32 {
    let path = entry.path.display().to_string();
    let inspection = match memory::inspect_remove(index, None, &path) {
        Ok(i) => i,
        Err(e) => return fail(mode, "memory-rm", &e),
    };

    // duster 建的整份文件(目录型 from-<agent>-*.md,内容全是 duster 块):
    // 整删一道门。
    if inspection.duster_file {
        let prompt = format!(
            "Delete the duster-created file {}? {}",
            display_tilde(&inspection.file),
            memory_archive_tail(no_archive)
        );
        return match prompt_yes_no(&prompt, false) {
            Ok(Some(true)) => cmd::memory::run(
                mode,
                index,
                &cmd::memory::MemoryCmd::Rm {
                    key: path.clone(),
                    from: None,
                    whole_file: false,
                    no_archive,
                    dry_run: false,
                },
            ),
            _ => EXIT_OK,
        };
    }

    // 没有 duster 块 → 用户自己手写的文件:两道门 + 强制归档。
    if inspection.block_count() == 0 {
        return memory_delete_user_file(mode, index, &path, &inspection, no_archive);
    }

    // 只有一个完整块:直切。菜单里选中这份文件就是选中了唯一的那个块,
    // 不存在需要点名的歧义;确认句仍亮出 from= 与归档去处。
    if inspection.blocks.len() == 1 && inspection.broken.is_empty() {
        let from = &inspection.blocks[0];
        let prompt = format!(
            "Cut the duster block from={from} out of {}? Your own text is untouched. {}",
            display_tilde(&inspection.file),
            memory_archive_tail(no_archive)
        );
        return match prompt_yes_no(&prompt, false) {
            Ok(Some(true)) => cmd::memory::run(
                mode,
                index,
                &cmd::memory::MemoryCmd::Rm {
                    key: path.clone(),
                    from: Some(from.clone()),
                    whole_file: false,
                    no_archive,
                    dry_run: false,
                },
            ),
            _ => EXIT_OK,
        };
    }

    // 多个块(或含残块):先挑切哪一块,或整删。残块没法切(报错而不是
    // 猜),只在问话里点名提醒。
    let mut rows: Vec<String> = inspection
        .blocks
        .iter()
        .map(|a| format!("cut the duster block from={a}"))
        .collect();
    rows.push("delete the whole file (two confirmations)".to_string());
    let mut prompt = "Delete which part".to_string();
    if !inspection.broken.is_empty() {
        prompt.push_str(&format!(
            " — note: the block from={} is missing its end marker (hand-edited)",
            inspection.broken.join(", ")
        ));
    }
    let Ok(Some(i)) = menu_pick(&prompt, &rows, 0) else {
        return EXIT_OK; // Esc:回详情,不动盘。
    };
    if i < inspection.blocks.len() {
        let from = inspection.blocks[i].clone();
        let prompt = format!(
            "Cut the duster block from={from} out of {}? Your own text is untouched. {}",
            display_tilde(&inspection.file),
            memory_archive_tail(no_archive)
        );
        match prompt_yes_no(&prompt, false) {
            Ok(Some(true)) => cmd::memory::run(
                mode,
                index,
                &cmd::memory::MemoryCmd::Rm {
                    key: path.clone(),
                    from: Some(from),
                    whole_file: false,
                    no_archive,
                    dry_run: false,
                },
            ),
            _ => EXIT_OK,
        }
    } else {
        memory_delete_user_file(mode, index, &path, &inspection, no_archive)
    }
}

/// 用户手写文件的删除:两道 y/N。第一道报路径与字节(文件同时还有
/// duster 块时一并亮出块数);第二道亮出所有权与归档去处——这道门是给
/// 「这是我自己写的、不是 duster 放的」这句事实一个单独的确认机会。
/// 核心层对用户手写文件强制归档(`--no-archive` 拒绝生效),真删走到这支
/// 也一样被拒——这里只按 `no_archive` 措辞确认句,拒绝是核心层的事,
/// 菜单不自己实现第二套规则。
fn memory_delete_user_file(
    mode: OutputMode,
    index: Option<&Path>,
    path: &str,
    inspection: &RemoveInspection,
    no_archive: bool,
) -> i32 {
    let size = std::fs::metadata(&inspection.file)
        .map(|m| m.len())
        .unwrap_or(0);
    let blocks_note = if inspection.block_count() > 0 {
        format!(
            " It also holds {} duster block(s).",
            inspection.block_count()
        )
    } else {
        String::new()
    };
    // 第一道门:报路径与字节。
    match prompt_yes_no(
        &format!(
            "Delete {} ({} bytes)?{}",
            display_tilde(&inspection.file),
            human_bytes(size),
            blocks_note,
        ),
        false,
    ) {
        Ok(Some(true)) => {}
        _ => return EXIT_OK,
    }
    // 第二道门:亮出所有权与归档去处。
    match prompt_yes_no(
        &format!(
            "This is your own writing, not something duster put there. \
             Delete it for good? {}",
            memory_archive_tail(no_archive)
        ),
        false,
    ) {
        Ok(Some(true)) => cmd::memory::run(
            mode,
            index,
            &cmd::memory::MemoryCmd::Rm {
                key: path.to_string(),
                from: None,
                whole_file: false,
                no_archive,
                dry_run: false,
            },
        ),
        _ => EXIT_OK,
    }
}

/// 确认文案里的归档落点目录。core 给的就是目录(包名带秒级时间戳,预告一个
/// 保证会变的精确文件名是假信息);真跑之后的报告里有实际路径。
fn memory_archive_dir() -> String {
    memory::remove_archive_dest(None)
        .map(|p| display_tilde(&p))
        .unwrap_or_else(|_| "~/agent-duster-exports".to_string())
}

/// 确认句的归档尾注。菜单把「删」拆成归档删 / 真删两条,唯一的差别是
/// `no_archive`,尾注要把各自的结果讲透:归档删亮出落点目录,真删点明
/// 没有退路(用户手写文件那一支核心层还会拒绝,见 [`memory_delete_user_file`])。
fn memory_archive_tail(no_archive: bool) -> String {
    if no_archive {
        "No archive is made — the original is gone.".to_string()
    } else {
        format!("It will be archived to {}.", memory_archive_dir())
    }
}

/// `mcp list` → 进组直接列表。**一行一条声明**(铺平,与 `skill list` 同构,
/// 见 [`cmd::mcp::browse_rows`]):选中即 [`mcp_detail`] 下钻,下钻目标永远是
/// 「这个名字」的合并视图——同一组的几行进同一个详情;勾选后 Enter 进
/// 批量动作(逐个 ping,见 [`mcp_bulk`])。
fn mcp_list(mode: OutputMode, index: Option<&Path>) -> Outcome {
    let list = match mcp::list(index) {
        Ok(l) => l,
        Err(e) => return Outcome::shown(fail(mode, "mcp-list", &e)),
    };
    let base = if list.warnings.is_empty() { EXIT_OK } else { EXIT_PARTIAL };
    render_warnings(&list.warnings);
    // 铺平后每一行是一条声明,而勾选批量里两类动作要的不是同一个对象:
    // delete 按「声明」(名字 + agent + 路径),ping 按「server」(去重)。
    // 两个映射在这里一次算好,动作菜单按需取,各动作不必自己重排。
    let mut row_to_server: Vec<usize> = Vec::new();
    let mut row_to_decl: Vec<(usize, usize)> = Vec::new();
    for (si, s) in list.servers.iter().enumerate() {
        for di in 0..s.declared_in.len() {
            row_to_server.push(si);
            row_to_decl.push((si, di));
        }
    }
    list_drill(
        mode,
        "mcp-list",
        Ok(&list),
        base,
        |list| list.servers.is_empty(),
        |_| {
            println!();
            println!(
                "  {}",
                muted().apply_to(
                    "No MCP server is indexed. Run `duster scan` first — if you just added one, run it again."
                )
            );
        },
        |list| cmd::mcp::browse_rows(list),
        "Which server",
        |list, i| mcp_detail(mode, index, &list.servers[row_to_server[i]]).into(),
        |list, sel| {
            mcp_bulk(mode, index, &list.servers, &row_to_server, &row_to_decl, sel)
                .map(DrillOutcome::from)
        },
        // 占位重取:返回同一份借用的 list。删过行后的真正重取(owned T +
        // 重排 row 映射)由 mcp 一片接上,见 mcp_list 的文档。
        || Ok(&list),
    )
}

/// 勾选集合的批量动作:逐个 ping 或批量删除。每一条 ping 都是一次真实的
/// `mcp ping <name>`(默认超时),循环只是替用户少敲几遍——「这五个还活着
/// 吗」不必来回下钻。delete 是勾选批量里最重的一档,见 [`mcp_bulk_delete`]。
///
/// 勾选的是**声明行**,ping 的是**server**:同一家的几条声明(同名同哈希
/// 合并成一组)去重后只 ping 一次——把同一个进程问三遍不是用户想要的。
/// delete 正好相反:**一行 = 一条声明**,勾选的就是删除目标(这正是铺平的
/// 理由),不按 server 去重。
///
/// `None` = 在动作菜单上按了 Esc(勾选保留,回列表);`Some(code)` =
/// 动作跑完(勾选由 [`drill`] 清掉)。
fn mcp_bulk(
    mode: OutputMode,
    index: Option<&Path>,
    servers: &[MergedServer],
    row_to_server: &[usize],
    row_to_decl: &[(usize, usize)],
    sel: &[usize],
) -> Option<i32> {
    let actions = [
        "ping each".to_string(),
        "delete each declaration from its agent's config".to_string(),
    ];
    match menu_pick(
        &format!("{}: what next", crate::plural(sel.len(), "declaration")),
        &actions,
        0,
    ) {
        Ok(Some(0)) => {
            let mut code = EXIT_OK;
            let mut seen: BTreeSet<usize> = BTreeSet::new();
            for &i in sel {
                let si = row_to_server[i];
                if !seen.insert(si) {
                    continue;
                }
                code = worse(
                    code,
                    cmd::mcp::run(
                        mode,
                        index,
                        cmd::mcp::McpCmd::Ping {
                            name: Some(servers[si].name.clone()),
                            timeout_ms: cmd::mcp::DEFAULT_PING_TIMEOUT_MS,
                        },
                    ),
                );
            }
            Some(code)
        }
        Ok(Some(1)) => mcp_bulk_delete(mode, index, servers, row_to_decl, sel),
        // Esc:回列表,勾选留着。
        _ => None,
    }
}

/// 勾选批量里的 delete:一行 = 一条声明,勾选的就是删除目标(这正是铺平
/// 的理由——合并行表达不了「删哪一家」)。先跑一轮 dry-run 把「会删几条、
/// 动几个文件、合计释放多少字节」算出来摆进确认句,再一道 y/N;点头后
/// 逐条走 `duster mcp rm` 的同一条通路。
///
/// 确认句里的数字来自 dry-run,不是拍脑袋:多选时「条数 + 合计字节」是
/// 用户按下 y 之前唯一能核对的事实。预检任一目标报错(已经被摘走、guard
/// 拒读)就当场说破并停在确认前——确认句里不能给一个执行时必然对不上的数。
fn mcp_bulk_delete(
    mode: OutputMode,
    index: Option<&Path>,
    servers: &[MergedServer],
    row_to_decl: &[(usize, usize)],
    sel: &[usize],
) -> Option<i32> {
    // 按 (name, agent) 去重:同一行只会出现一次,但保险起见不重复删。
    let mut targets: Vec<(String, String)> = Vec::with_capacity(sel.len());
    let mut files: BTreeSet<&Path> = BTreeSet::new();
    for &i in sel {
        let (si, di) = row_to_decl[i];
        let name = servers[si].name.clone();
        let agent = servers[si].declared_in[di].agent_id.clone();
        if targets.iter().any(|(n, a)| *n == name && *a == agent) {
            continue;
        }
        files.insert(servers[si].declared_in[di].path.as_path());
        targets.push((name, agent));
    }

    // 预检:dry-run 只算不写,合计释放字节就是确认句里的那个数。
    // 任一目标报错(已经被摘走、guard 拒读)就当场说破并停在确认前——
    // 确认句里不能给一个执行时必然对不上的数。报错按 Esc 处理:回列表,
    // 勾选留着,用户能取消掉坏的那几条再删。
    let mut freed = 0u64;
    for (name, agent) in &targets {
        match mcp::remove(&mcp::RemoveOptions {
            index_path: index.map(Path::to_path_buf),
            home: None,
            dry_run: true,
            name: name.clone(),
            agent: Some(agent.clone()),
            all_agents: false,
        }) {
            Ok(r) => freed = freed.saturating_add(r.freed_bytes),
            Err(e) => {
                eprintln!(
                    "  {}",
                    muted().apply_to(format!("cannot delete `{name}` from `{agent}`: {e:#}"))
                );
                return None;
            }
        }
    }

    // 一道 y/N:亮出条数、文件数与合计字节,外加改写前的快照落点。这是摘键前
    // 的最后一道门,默认 No。
    match prompt_yes_no(
        &format!(
            "Delete {} from {} — about {} freed? The whole files are snapshotted to ~/.agent-duster/snapshots first.",
            crate::plural(targets.len(), "declaration"),
            crate::plural(files.len(), "file"),
            human_bytes(freed)
        ),
        false,
    ) {
        Ok(Some(true)) => {
            let mut code = EXIT_OK;
            for (name, agent) in &targets {
                code = worse(
                    code,
                    cmd::mcp::run(
                        mode,
                        index,
                        cmd::mcp::McpCmd::Rm {
                            name: name.clone(),
                            agent: Some(agent.clone()),
                            all_agents: false,
                            dry_run: false,
                        },
                    ),
                );
            }
            Some(code)
        }
        // n / Esc:回列表,勾选留着。
        _ => None,
    }
}

/// 详情屏的动作。菜单行文本与动作一一对应。back 没有条目——Esc 就是返回。
#[derive(Clone, Copy)]
enum McpDetailAction {
    Copy,
    Ping,
    Delete,
}

impl McpDetailAction {
    /// 菜单行文本,顺序即菜单顺序。
    fn label(self) -> &'static str {
        match self {
            McpDetailAction::Copy => "copy it into other agents",
            McpDetailAction::Ping => "ping it",
            McpDetailAction::Delete => "delete a declaration from one agent's config file",
        }
    }
}

/// 一个 server 的详情:先 `mcp show`,再出上下文动作。动作跑完回列表
/// (drill 的下一轮),不是回顶层菜单;Esc 回列表,一次一层。
///
/// 铺平表已经把「几家声明各长什么样」摆在列表里,这一屏只回答「这条
/// 声明还能怎么处置」;compare 动作随 `mcp diff` 一起撤下——差异在列表
/// 的 STATE 列与各行的 command / url 里一眼可见,再摆一条「对比两家」
/// 是旧路线的残留。
///
/// 默认光标落第 0 格:copy 后面还有目标勾选表(默认全不勾)+ 写入确认
/// 两道门,回车不会一击写盘。delete 沉底且自带 y/N 确认,同样不会一击写盘。
fn mcp_detail(mode: OutputMode, index: Option<&Path>, server: &MergedServer) -> i32 {
    let mut code = cmd::mcp::run(
        mode,
        index,
        cmd::mcp::McpCmd::Show {
            name: server.name.clone(),
        },
    );

    let actions = [McpDetailAction::Copy, McpDetailAction::Ping, McpDetailAction::Delete];
    let labels: Vec<String> = actions.iter().map(|a| a.label().to_string()).collect();
    let Ok(Some(i)) = menu_pick(&format!("{}: what next", server.name), &labels, 0)
    else {
        // Esc:回列表,什么都没发生。
        return code;
    };
    match actions[i] {
        McpDetailAction::Copy => {
            // 名字不再问第二遍:这一屏的上下文就是它。
            code = worse(code, mcp_sync_named(mode, index, server.name.clone()));
        }
        McpDetailAction::Ping => {
            code = worse(
                code,
                cmd::mcp::run(
                    mode,
                    index,
                    cmd::mcp::McpCmd::Ping {
                        name: Some(server.name.clone()),
                        timeout_ms: cmd::mcp::DEFAULT_PING_TIMEOUT_MS,
                    },
                ),
            );
        }
        McpDetailAction::Delete => {
            code = worse(code, mcp_delete_decl(mode, index, server));
        }
    }
    code
}

/// 详情屏的 delete 动作:先挑「哪一条声明」(同一个 server 可能被多家声明,
/// 删哪家是删除的目标本身,不许默认替用户挑),再一道 y/N 确认——文案亮出
/// 将删的路径与快照落点——然后走 `duster mcp rm` 的同一条通路,回执、
/// 退出码、报错文案与命令行逐字相同。
///
/// Esc / 拒绝确认 = 不动盘,EXIT_OK 回详情。
fn mcp_delete_decl(mode: OutputMode, index: Option<&Path>, server: &MergedServer) -> i32 {
    if server.declared_in.is_empty() {
        eprintln!(
            "  {}",
            muted().apply_to("no declaration is indexed for this server")
        );
        return EXIT_OK;
    }
    let chosen = if server.declared_in.len() == 1 {
        0
    } else {
        // 声明者多于一个时先选一个:行文本把「哪个 agent 的哪个文件」摆出来,
        // 用户按掉的才是删除目标。
        let rows: Vec<String> = server
            .declared_in
            .iter()
            .map(|d| format!("{} — {}", d.agent_id, display_tilde(&d.path)))
            .collect();
        match menu_pick("Remove which declaration", &rows, 0) {
            Ok(Some(i)) => i,
            _ => return EXIT_OK, // Esc:回详情,不动盘。
        }
    };
    let d = &server.declared_in[chosen];
    // 往别人家主配置里摘键前的最后一道门。文案点明路径与快照落点:
    // 摘错了一条声明,用户要从快照里把整份配置捞回来。
    match prompt_yes_no(
        &format!(
            "Remove `{}` from {}? The whole file is snapshotted to ~/.agent-duster/snapshots first, so it can be restored from there.",
            server.name,
            display_tilde(&d.path)
        ),
        false,
    ) {
        Ok(Some(true)) => cmd::mcp::run(
            mode,
            index,
            cmd::mcp::McpCmd::Rm {
                name: server.name.clone(),
                agent: Some(d.agent_id.clone()),
                all_agents: false,
                dry_run: false,
            },
        ),
        // n / Esc / 终端拿不到,都算「算了」:不动盘不是错误。
        _ => EXIT_OK,
    }
}

/// 默认一个都不勾——方向与 [`prompt_agent`] 相反,理由见 [`sync_target_agents`]。
fn mcp_sync_named(mode: OutputMode, index: Option<&Path>, name: String) -> i32 {
    let from = match prompt_optional("Copy from which agent (leave empty to let duster pick)") {
        Ok(LineOutcome::Value(v)) => Some(v),
        Ok(LineOutcome::Empty) => None,
        Ok(LineOutcome::Esc) => return EXIT_OK,
        Err(()) => return EXIT_ERROR,
    };
    let to = match sync_target_agents(index, from.as_deref()) {
        Ok(Some(v)) => v,
        Ok(None) => return EXIT_OK, // Esc:什么都没发生,与其余问句一致
        Err(()) => return EXIT_ERROR,
    };
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
            prompt: "Write these · Space toggle · a all/none · Enter sync the checked · Esc back",
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

/// 目标勾选表的行:`agent · action · path`。三列都走 [`Table`] 的量法
/// (按 `display_width` 定宽,宽字符不会顶歪下一列),路径列吃余量、截断
/// 给出——整行宽有上限,路径不截的话 80 列的终端上一行就是 90 列,必然
/// 折行。列间分隔由 Table 统一给两空格;旧版手拼的 ` · ` 各写各的,
/// 不再保留。
fn sync_target_rows(plan: &[SyncOutcome]) -> Vec<String> {
    sync_target_rows_at(plan, term_width())
}

/// 宽度算法本体;`cols` 由测试直接给(60 / 80 / 200),不必 mock 终端。
fn sync_target_rows_at(plan: &[SyncOutcome], cols: usize) -> Vec<String> {
    let mut table = Table::new(vec!["", "", ""]);
    // 路径列吃余量。旧版手写过一个 16 列的地板,但地板会让整行越过预算、
    // 折行、翻残影;预算是硬约束,窄终端下宁可把路径截狠(见 Table 的 flex)。
    table.flex_col(2);
    for o in plan {
        table.push_row(vec![
            o.agent_id.clone(),
            o.action.clone(),
            display_tilde(Path::new(&o.path)),
        ]);
    }
    // 表头三格全空:这张勾选表没有表头,消费方只取行,丢头留行。
    table.rows_at(Prefix::Checkbox, cols).1
}

/// 目标名单的勾选表:一行一个 agent,排除 `from` 点名的那个。默认一个
/// 都不勾。
///
/// 行文本只印 agent id,不照抄 [`agent_choice_rows`] 那张带体积的表:
/// 那两个数(总量 / 可回收量)是给 clean / prune 做「清谁」决策用的;
/// 这里的问题是「往谁家写」,家当多大不改变答案,印出来只会把行顶宽、
/// 把屏幕占掉,而这一屏的候选常常就是三五个名字。
///
/// **默认一个都不勾**——与 [`prompt_agent`] 那张表相反。理由:`--agent`
/// 不带就是「全部」,所以那屏默认全勾;而 `--to` 在 clap 里是
/// `required = true`,命令行**没有**「全部」这个默认,菜单替用户勾满
/// 等于凭空发明一个 CLI 表达不出的默认值,而它的后果是往每一个 agent
/// 的配置文件里写字。
///
/// 索引缺失或一个 agent 都没有时退回逗号输入:一屏没有任何可选项的
/// 菜单是死胡同,而自由输入总能给出一条路。排除 `from` 后一个都不剩
/// 同样退回——勾选表连一行都没有,不是「用户勾了零个」,是根本没得选。
fn sync_target_agents(
    index: Option<&Path>,
    from: Option<&str>,
) -> Result<Option<Vec<String>>, ()> {
    let agents = match agent_choices(index) {
        Some(a) => a,
        None => return fallback_targets(),
    };
    let items: Vec<String> = agents
        .iter()
        .map(|a| a.agent_id.clone())
        .filter(|id| Some(id.as_str()) != from)
        .collect();
    if items.is_empty() {
        return fallback_targets();
    }
    let picked = match prompt_checklist(
        &menu_theme(),
        Checklist {
            // 这一屏只是圈定候选,一个字节都还没写——计划在它之后才出。
            // 措辞不能借用下一屏那句「Enter sync the checked」:那是同意
            // 门,这里按下回车只会让 duster 去算一份计划给你看。
            prompt: "Copy into which agents · Space toggle · a all/none · Enter confirm · Esc back",
            header: None,
            items: &items,
            checked: initial_checked(items.len(), false),
            select_all: false,
            page: crate::browse::viewport(CHECKLIST_RESERVED),
        },
    ) {
        Ok(Some(p)) => p,
        Ok(None) => return Ok(None), // Esc:回上一屏,什么都没发生
        Err(_) => return Err(()),
    };
    Ok(Some(
        picked.iter().map(|&i| items[i].clone()).collect(),
    ))
}

/// 逗号输入的降级:与 clap 的 `value_delimiter = ','` 同一把切法,
/// 空串与空白段一律滤掉。
fn fallback_targets() -> Result<Option<Vec<String>>, ()> {
    let raw = match prompt_required("Copy into which agents, comma separated") {
        Ok(Some(v)) => v,
        Ok(None) => return Ok(None),
        Err(()) => return Err(()),
    };
    Ok(Some(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    ))
}

/// 勾选表的初始勾选向量。两个入口方向相反:
/// - [`prompt_agent`] 默认全勾——命令行不带 `--agent` 就是「全部」,
///   所以「全部」才是那屏的默认答案;
/// - sync 的目标表默认全不勾——`--to` 没有「全部」这个默认(见
///   [`sync_target_agents`]),替用户勾满等于发明一个 CLI 表达不出的
///   默认值。
///
/// 抽成纯函数,让测试能把这两个方向各自钉死。
fn initial_checked(n: usize, default: bool) -> Vec<bool> {
    vec![default; n]
}


// ---------------------------------------------------------------------------
// skill list:组列表 → 组详情(副本表 + link)→ 回组列表
// ---------------------------------------------------------------------------

/// `skill list` → 进组直接列表,选中进组详情;勾选后 Enter 进批量动作
/// (逐个 link 进同一个目标,见 [`skill_bulk`])。Esc 回菜单,一次一层。
///
/// 列的是**全部** skill,不只装在多处的:单份组是常态(本机 35 个里 23 个
/// 只在一个 agent 里),把它们藏起来等于让用户以为那些 skill 不存在。装在
/// 多处的组在 AGENTS 列一眼可见,单份组进详情也还能 link 出去。
fn skill_list(mode: OutputMode, index: Option<&Path>) -> Outcome {
    let groups = match skill_ops::list(index) {
        Ok(g) => g,
        Err(e) => return Outcome::shown(fail(mode, "skill-list", &e)),
    };
    // 行 -> (组, 组内副本) 的映射:表铺平成一行一份副本,而下钻的目标是
    // 「这个 skill」(整组一屏),批量的目标是「这一份」。两种粒度都要,
    // 所以映射建一次两处共用(与 `mcp_list` 的 row_to_server / row_to_decl 同法)。
    let row_to: Vec<(usize, usize)> = groups
        .iter()
        .enumerate()
        .flat_map(|(gi, g)| (0..g.copies.len()).map(move |ci| (gi, ci)))
        .collect();
    list_drill(
        mode,
        "skill-list",
        Ok(groups),
        EXIT_OK,
        |groups| groups.is_empty(),
        |_| {
            println!();
            println!(
                "  {}",
                muted().apply_to("No skills found on disk — nothing to list.")
            );
        },
        |groups| skill_copy_rows(groups),
        "Which skill",
        |groups, i| skill_group_detail(mode, index, &groups[row_to[i].0]),
        |groups, sel| skill_bulk(mode, index, groups, &row_to, sel),
        || skill_ops::list(index),
    )
}

/// 勾选集合的批量动作:把勾中的每一份副本 link 进同一个目标 agent,或逐份删掉。
///
/// 表铺平成一行一份之后,这里**不再追问「哪一份」**:勾选的行就是那一份。
/// 旧版一行一组,link 的源与 delete 的目标都得逐组再问一遍,多份组尤其烦;
/// 铺平把那两轮问句整个消掉了——这是铺平除了「删除目标明确」之外的第二个收益。
/// link 的目标 agent 仍只问一次(它是这一批共同的去处)。
///
/// 每一步都是一次真实的 `skill link` / `skill rm`,失败照常报告并继续——
/// 五个成功一个失败,那五个不该陪葬。
///
/// `None` = 动作没定下来就退出来(勾选保留,回列表);`Some(code)` =
/// 动作跑完(勾选由 [`list_drill`] 清掉)。delete 跑完时交回 `changed`,
/// 让列表重取——不许继续列着已删的行。
fn skill_bulk(
    mode: OutputMode,
    index: Option<&Path>,
    groups: &[SkillGroup],
    row_to: &[(usize, usize)],
    sel: &[usize],
) -> Option<DrillOutcome> {
    let actions = [
        "link each into another agent (one copy on disk)".to_string(),
        "delete each (deleted outright — no archive, nothing to restore from)".to_string(),
    ];
    let pick = match menu_pick(&format!("{}: what next", crate::plural(sel.len(), "copy")), &actions, 0) {
        Ok(Some(i)) => i,
        // Esc:回列表,勾选留着。
        _ => return None,
    };
    if pick == 0 {
        // 不排除任何候选:各行的源 agent 不一样,这里排谁都排错。目标撞上
        // 某一行的源时由 `skill link` 自己报错,那一行失败,其余照常。
        let to = match pick_target_agent(index, "") {
            Ok(Some(v)) => v,
            Ok(None) => return None,
            Err(()) => return Some(EXIT_ERROR.into()),
        };
        let mut code = EXIT_OK;
        for &i in sel {
            let (gi, ci) = row_to[i];
            let g = &groups[gi];
            // 勾中的行就是要共享出去的那一份,不再追问。
            let from = g.copies[ci].agent_id.clone();
            code = worse(code, cmd_skill_link(mode, index, &g.name, &from, &to, false));
        }
        return Some(DrillOutcome::from(code));
    }

    // delete each:勾中的行就是要删的副本,直接凑名单再一道 y/N——多选时
    // 确认句必须报条数与合计字节,用户同意的是一件具体大小的事,不是一句
    // 含糊的「删掉这些」。
    let mut plan: Vec<(&SkillGroup, usize)> = Vec::new();
    let mut total = 0u64;
    for &i in sel {
        let (gi, ci) = row_to[i];
        total += groups[gi].copies[ci].bytes;
        plan.push((&groups[gi], ci));
    }
    if plan.is_empty() {
        return None;
    }
    match prompt_yes_no(
        &format!(
            "Delete {} ({}) outright? Skill deletion makes no archive — nothing to \
             restore from.",
            crate::plural(plan.len(), "copy"),
            human_bytes(total)
        ),
        false,
    ) {
        Ok(Some(true)) => {
            let mut code = EXIT_OK;
            let mut changed = false;
            for (g, ci) in plan {
                let c = cmd_skill_rm(
                    mode,
                    index,
                    None,
                    &g.name,
                    Some(&g.copies[ci].agent_id),
                    // 勾选的是具体那一份,路径必须传下去:只给 agent 名会把
                    // 那家的同名多份一起删掉(本机 open-gstack-browser 在
                    // claude-code 下就有两份),而确认句说的是「这一份」。
                    Some(&g.copies[ci].path.display().to_string()),
                    false,
                );
                code = worse(code, c);
                if c != EXIT_ERROR {
                    changed = true;
                }
            }
            Some(DrillOutcome { code, changed })
        }
        // n / Esc / 终端拿不到:算了,不动盘。勾选留着,回列表。
        _ => None,
    }
}

/// 单选一份副本,返回它在 `copies` 里的下标。行文本带路径——同一 agent
/// 名下同名两份(不同目录)是真实存在的,只给 agent 名根本分不开。
/// Esc = None(调用方跳过这一组)。
fn pick_copy_index(g: &SkillGroup, prompt: &str) -> Option<usize> {
    let rows: Vec<String> = g
        .copies
        .iter()
        .map(|c| format!("{} · {}", c.agent_id, display_tilde(&c.path)))
        .collect();
    match menu_pick(prompt, &rows, 0) {
        Ok(Some(i)) => Some(i),
        Ok(None) | Err(()) => None,
    }
}

/// 副本列表的浏览行:**一行一份副本**(铺平,与 `mcp` 列表同构)。
///
/// 不是「一行一组、AGENTS 单元格里塞 `claude-code ×2, codex`」——那种合并行
/// 有两个毛病:同一 agent 装了两份时单元格分不开它们(本机
/// `open-gstack-browser` 在 claude-code 下就是两份,目录不同、内容可以不同),
/// 而勾选一行到底选中了哪一份也说不清。铺平之后一行 = 一个明确的删除目标,
/// 正好对上 `skill rm` 要 `--path` 的契约。
///
/// skill 名每行都印:这张表会翻页,一组的几份可能被切到下一页,而每行都是
/// 可勾选的删除目标——空名字的行既认不出属于谁又能被选中删掉。
fn skill_copy_rows(groups: &[SkillGroup]) -> (String, Vec<String>) {
    skill_copy_rows_at(groups, term_width())
}

/// 宽度算法本体;`cols` 由测试直接给(60 / 80 / 200),不必 mock 终端。
/// 前缀预算是 [`Prefix::Checkbox`]:统一列表流的浏览表每行画 `❯ [x] `。
fn skill_copy_rows_at(groups: &[SkillGroup], cols: usize) -> (String, Vec<String>) {
    let mut table = Table::new(vec!["SKILL", "STATE", "AGENT", "CONTENT", "LAST USED", "PATH"]);
    table.right_align(&[3]);
    // LAST USED 是短列(「3d ago」/「never」这个量级),按内容宽放、不做
    // flex;PATH 吃余量:它天然最长,窄终端里按余量截断。名字 / 状态 /
    // agent 三列一格不让——「哪个 skill、什么状态、谁装的」是这一屏存在
    // 的理由。
    table.flex_col(5);
    for g in groups {
        for c in &g.copies {
            table.push_row(vec![
                g.name.clone(),
                crate::dup_state_label(c.state).to_string(),
                c.agent_id.clone(),
                human_bytes(c.bytes),
                crate::relative_time(c.last_used_ms),
                // 折 `~`:副本住在 home 下,`/Users/<你>/` 那 13 列零信息量。
                display_tilde(&c.path),
            ]);
        }
    }
    table.rows_at(Prefix::Checkbox, cols)
}

/// 一组 skill 的详情:副本表(agent / 内容 / install / 路径)+ 软链去向
/// 与坏链修法 + 动作菜单(link)。
///
/// 「对比两份副本」已随顶层 `diff` 一起撤下:通用文件对比交给系统 `diff`,
/// 两条 PATH 就印在这张表里;`mcp` 详情屏的 compare 动作也随 `mcp diff`
/// 撤了——菜单里 diff 引擎不再有落脚点,要比就把 PATH 拿去喂 `duster diff`。
///
/// Esc 回 `skill_list` 的组列表,一次一层,不弹回顶层——这一屏是 drill 的
/// 详情,动作打完用户多半还要看下一组,回列表且光标停在原行,比重新进一遍
/// 省事。
fn skill_group_detail(mode: OutputMode, index: Option<&Path>, g: &SkillGroup) -> DrillOutcome {
    // INSTALLED 只在真有编译产物时出一列——与 `render_skill_groups` 同一条
    // 规矩:一整列 0 B 既占掉 PATH 要的宽度,又让人以为那里有意义可读。
    let show_install = g.copies.iter().any(|c| c.install_bytes > 0);
    let mut head = vec!["AGENT", "CONTENT"];
    if show_install {
        head.push("INSTALLED");
    }
    // LAST USED 在 PATH 前:这一屏是用户决定删哪一份的地方,「哪一份早
    // 就不用了」正是判据;PATH 留到行尾吃终端余量。
    head.push("LAST USED");
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
        row.push(crate::relative_time(c.last_used_ms));
        // 折 `~`,与列表那一屏同口径:同一条动线里一处折一处不折,用户会
        // 以为是两个不同的路径。
        row.push(display_tilde(&c.path));
        t.push_row(row);
    }
    // 路径列吃终端余量:这一屏是 TTY 上的阅读屏,超宽就截;要完整路径走
    // `duster skill list --json`,那里永不截断。
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
    // 软链副本的去向与坏链的修法:CONTENT 比较把 Linked/Broken 排除在外
    // (skill_ops 的口径),这两种行在表里只有一个 0 B,不交代去向等于让
    // 用户自己去 readlink。坏链给一条能直接抄的修法——目标已经没了,
    // 这个 symlink 本身就是垃圾。
    for c in &g.copies {
        match c.state {
            DupState::Linked => {
                if let Some(t) = &c.link_target {
                    println!(
                        "  {}",
                        muted().apply_to(format!(
                            "{} is a symlink → {}",
                            c.agent_id,
                            display_tilde(t)
                        ))
                    );
                }
            }
            DupState::Broken => {
                let gone = c
                    .link_target
                    .as_ref()
                    .map(|t| display_tilde(t))
                    .unwrap_or_else(|| "?".to_string());
                println!(
                    "  {} {}",
                    warn_mark(),
                    format!(
                        "{} is a symlink → {} which is gone · fix: rm {} (dangling symlink)",
                        c.agent_id,
                        gone,
                        display_tilde(&c.path)
                    )
                );
            }
            _ => {}
        }
    }

    // 动作菜单只剩 link。Esc 回组列表;link 自己还有源/目标两问,
    // 回车不会一击写盘。
    let actions = skill_group_actions(g);
    match menu_pick(&format!("{}: what next", g.name), &actions, 0) {
        Ok(Some(0)) => {
            let code = skill_link_action(mode, index, g);
            DrillOutcome::from(code)
        }
        Ok(Some(1)) => {
            let (code, deleted) = skill_delete_action(mode, index, g);
            if deleted {
                // 真删了:列表下一轮必须重取,不许继续列着这一行。
                DrillOutcome { code, changed: true }
            } else {
                DrillOutcome::from(code)
            }
        }
        // Esc:回组列表(详情是 drill 的下钻,回列表是回到原行接着看,
        // 不是弹回顶层)。
        _ => DrillOutcome::from(EXIT_OK),
    }
}

/// 详情动作里的 delete:多份副本时先挑一份(单份免问),一句 y/N 确认
/// (亮出将删的路径、体积与形态对应的实话),然后走 `cmd_skill_rm` 的
/// 同一条通路——菜单不留第二套业务逻辑,回执、退出码、报错文案与命令行
/// 逐字相同。
///
/// 软链副本(Linked / Broken)的确认句必须说实话:只删链接,指向的内容
/// 原样留在别处。真目录也不再说「先归档」——skill 删除不归档(见
/// [`duster_core::skill_ops::remove`] 的文档),确认句照实说「直接删、不留
/// 副本」。
///
/// 返回 (退出码, 是否真删了)。真删了才让列表重取;拒绝 / Esc / 终端拿
/// 不到 = 不动盘,不是错误,也不触发重取。
fn skill_delete_action(mode: OutputMode, index: Option<&Path>, g: &SkillGroup) -> (i32, bool) {
    let ci = if g.copies.len() == 1 {
        0
    } else {
        match pick_copy_index(g, &format!("{}: which copy to delete", g.name)) {
            Some(v) => v,
            // Esc:回详情,不动盘。
            None => return (EXIT_OK, false),
        }
    };
    let c = &g.copies[ci];
    let note = match c.state {
        DupState::Linked | DupState::Broken => {
            "only the link is removed — the content it points to stays"
        }
        _ => "deleted outright — no archive, nothing to restore from",
    };
    let prompt = format!(
        "Delete this copy of {} ({path} · {size})? {note}",
        g.name,
        path = display_tilde(&c.path),
        size = human_bytes(c.bytes)
    );
    match prompt_yes_no(&prompt, false) {
        Ok(Some(true)) => {
            let code = cmd_skill_rm(
                mode,
                index,
                None,
                &g.name,
                Some(&c.agent_id),
                // 同上:确认句写的是「this copy」,删的就得是这一份。
                Some(&c.path.display().to_string()),
                false,
            );
            (code, code != EXIT_ERROR)
        }
        // n / Esc / 终端拿不到,都算「算了」:不动盘不是错误。
        _ => (EXIT_OK, false),
    }
}

/// 组详情下钻的动作行文。单独成函数而不是 inline:数量与措辞是用户可见
/// 的稳定契约,测试要逐字核对。back 没有条目——Esc 就是返回。
///
/// link 把「只在 claude-code 里的 skill 共享给 codex」——单份 skill 最
/// 有用的动作,列表现在把单份组也摊开了,这一格就是它们的出口;
/// delete 是反方向:删掉一份副本。「对比两份副本」已撤(见
/// [`skill_group_detail`])。
///
/// delete 那句的后半段跟着副本形态走，不能写死「先归档」：skill 删除
/// **不归档**（见 [`duster_core::skill_ops::remove`] 的文档）——真目录整棵
/// 删、软链副本（Linked / Broken）删的只是链接本身。原来的三档（真目录
/// 说先归档 / 软链说只摘链接 / 混着不承诺）并成两档：全是软链就说只摘链接；
/// 其余（含混着）都说「直接删、不留副本」——这句对每一份副本都是实话，
/// 软链那份的「内容留在别处」留给 [`skill_delete_action`] 挑完副本后逐份
/// 补充。
fn skill_group_actions(g: &SkillGroup) -> Vec<String> {
    let link_like = |c: &duster_core::skill_ops::SkillCopy| {
        matches!(c.state, DupState::Linked | DupState::Broken)
    };
    let delete = if g.copies.iter().all(link_like) {
        "delete a copy (only the link is removed — the content it points to stays)"
    } else {
        "delete a copy (deleted outright — no archive, nothing to restore from)"
    };
    vec![
        "link this skill into another agent (one copy on disk)".to_string(),
        delete.to_string(),
    ]
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


/// 交互式 clean:只需要范围,清单打完当场确认。范围问句 Esc = 不 pause
/// 的取消(什么都没印);跑过 `cmd_clean` 就是 shown——哪怕用户在勾选表
/// 上全不勾,计划本身也已经印在屏上了。
fn prompt_clean(mode: OutputMode, index: Option<&Path>) -> Outcome {
    match prompt_agent(index) {
        Ok(Some(agents)) => Outcome::shown(cmd_clean(mode, index, agents, Consent::Ask)),
        Ok(None) => Outcome::silent(EXIT_OK),
        Err(()) => Outcome::silent(EXIT_ERROR),
    }
}

/// 交互式 prune:阈值问句见 [`prompt_age`],范围见 [`prompt_agent`]。
///
/// 归档在这里没有开关:prune 恒归档,凡删必先打包;预估超阈值不拦路,
/// 体积随计划报告给用户过目,不再当场表态。
fn prompt_prune(mode: OutputMode, index: Option<&Path>) -> Outcome {
    let age = match prompt_age() {
        Ok(Some(a)) => a,
        Ok(None) => return Outcome::silent(EXIT_OK),
        Err(()) => return Outcome::silent(EXIT_ERROR),
    };
    let agents = match prompt_agent(index) {
        Ok(Some(v)) => v,
        Ok(None) => return Outcome::silent(EXIT_OK),
        Err(()) => return Outcome::silent(EXIT_ERROR),
    };
    // 代际超编是「按数量冗余」而不是「放久了」:不看 --older-than,所以
    // 单独一问,并把会删什么讲成人话——数据库备份这类保留最新 N 份的
    // 资源,第 N+1 份起会被一并清掉。默认关,与命令行旗标默认一致。
    let keep_generations = match prompt_yes_no(
        "Also prune surplus backups — resources that keep only the newest N copies, like database backups?",
        false,
    ) {
        Ok(Some(b)) => b,
        Ok(None) => return Outcome::silent(EXIT_OK),
        Err(()) => return Outcome::silent(EXIT_ERROR),
    };
    Outcome::shown(cmd_prune(
        mode,
        index,
        agents,
        Some(age),
        Consent::Ask,
        keep_generations,
    ))
}

/// 交互式 uninstall:单选 agent → 勾一张两格的范围表([`uninstall_scope`])
/// → 一次默认 No 的 y/N,过了才动手。
///
/// 不用 checklist 选 agent 而用单选([`prompt_pick`]):卸载是最重的动作,
/// 一屏勾多个 agent 全删的批量语义太危险,产品上单选。范围那一张才是勾选
/// 表——它问的不是「删几个 agent」,而是「这一个 agent 的哪两样东西」。
///
/// 只留一道 y/N 而不是两道:范围表本身已经是一次逐项过目,后面再叠一句
/// 「你真的确定吗」只会训练用户闭眼按 y。该说的都压进那一句里——数字来自
/// 索引统计([`uninstall_summary`]),外加「什么都不归档、不可撤销」。
///
/// 范围表或确认句按 Esc / 选 No 都退回列表重新挑,一次 Esc 一层;过了才把
/// 确认串交给 [`cmd_uninstall`]——与命令行 `--confirm <agent>` 走同一条执行
/// 门,授权凭据不再需要逐字输入。
///
/// 收场的停顿口径([`Outcome`]):Esc 一路取消 = silent(交互屏自己擦干
/// 净了,屏上没有没读过的东西);「索引里没 agent」的那一句与执行后的
/// 报告 = shown——不停一下,它们会在 0 毫秒内被菜单盖掉。
fn prompt_uninstall(mode: OutputMode, index: Option<&Path>) -> Outcome {
    let Some(agents) = agent_choices(index) else {
        // 索引缺失或一个 agent 都没有:没有可选项的列表是死胡同,而卸载
        // 不接受自由输入(见模块文档)——说一句然后回菜单。
        eprintln!(
            "  {}",
            muted().apply_to("no agents in the index · nothing to remove")
        );
        return Outcome::shown(EXIT_OK);
    };
    loop {
        let rows = agent_choice_rows(&agents);
        let picked = match prompt_pick(
            &menu_theme(),
            Picker {
                prompt: "Uninstall which agent",
                header: None,
                items: &rows,
                page: crate::browse::viewport(CHECKLIST_RESERVED),
                start: 0,
            },
        ) {
            Ok(Some(i)) => i,
            Ok(None) => return Outcome::silent(EXIT_OK), // Esc:回菜单,什么都没发生
            Err(_) => return Outcome::silent(EXIT_ERROR),
        };
        let agent = &agents[picked];
        eprintln!(
            "  {} {}",
            ok_mark(),
            muted().apply_to(format!("agent · {}", agent.agent_id))
        );
        // 范围勾选:卸载要动的是两件不同性质的东西,让用户各自表态,
        // 而不是替他把「删数据」和「代跑包管理器」捆成一个 y。
        let Some(scope) = uninstall_scope(agent) else {
            continue;
        };
        // 一次 y/N 收口。勾选表本身已经是一次逐项过目,再叠第二道
        // 「你真的确定吗」只会训练用户闭眼按 y——把该说的数字与
        // 不可逆一次说全,那才是同意门。
        match ask(&ColorfulTheme::default(), &scope.confirm_line(agent)) {
            Approval::Yes => {}
            // Esc 与 No 同路:退回列表重新挑。
            Approval::No => continue,
            Approval::Aborted => return Outcome::silent(EXIT_ERROR),
        }
        return Outcome::shown(cmd_uninstall(
            mode,
            index,
            UninstallArgs {
                agent: agent.agent_id.clone(),
                // 菜单用户加不上旗标,所以给他完整的那个卸载:清单打全(包括要动
                // 别人家哪一个键)。留一条指向已删程序的 MCP 声明不叫卸载干净。
                data_only: false,
                // 勾选表 + 确认句就是授权凭据:确认串直接给 agent id,core 会再
                // 校验一次,与命令行 `--confirm <agent>` 走同一道执行门。
                confirm: Some(agent.agent_id.clone()),
                // `export_first` 在 core 里的含义是「sessions / memory 进删除
                // 计划」(反义是 `--keep` 留在原地),不是「一定打包」;真正决定
                // 打不打包的是 `archive`。菜单要的是卸载干净,所以两者搭成
                // 「进计划 + 显式接受永久丢失」。
                export_first: true,
                keep: Vec::new(),
                // 归档在这条路上是关掉的:用户是来卸载的,不是来换个目录囤
                // 同一批字节的——留一个他还得自己去清的包,不叫卸载干净。
                // 想要退路的人走命令行 `--export-first`(默认开)。
                archive: Some(false),
                run_package_manager: scope.software,
            },
            Consent::Granted,
        ));
    }
}

/// 卸载范围:两个复选框各自代表什么。
///
/// 拆成两格而不是一句「全删吗」,因为它们的性质根本不同:一格是**删本机
/// 文件**(duster 自己动手,可预期),另一格是**代跑包管理器**(把控制权
/// 交给 npm / brew,输出与后果都不在 duster 手里)。把后者塞进前者的 y
/// 里,等于替用户签了一份他没读过的字。
struct UninstallScope {
    /// 删 agent 的 owns 树;二进制若也在树里,这一项连它一起删除。
    data: bool,
    /// 连软件本体一起卸:`--run-package-manager` 的菜单入口。
    software: bool,
    /// 二进制已属于 owns 树时,确认句明确说出这一点。
    binary_inside_owns: bool,
}

impl UninstallScope {
    /// 同意门那一句。数字与不可逆一次说全:说不清丢什么的确认句
    /// 换不来真正的同意。
    fn confirm_line(&self, a: &AgentStatus) -> String {
        let tail = if self.binary_inside_owns {
            " · the binary itself is inside the data tree"
        } else if self.software {
            " · the software itself is uninstalled too"
        } else {
            ""
        };
        format!(
            "Delete {} for good - {} · nothing is archived, this cannot be undone{}",
            a.agent_id,
            uninstall_summary(a),
            tail
        )
    }
}

/// 问一张两格的范围表。`None` = Esc / 勾了个不成立的组合,调用方退回
/// agent 列表重挑,一个字节都没动。
///
/// 数据那一格默认勾上、软件本体默认不勾:前者是这个动词的定义,后者会
/// 把控制权交给包管理器,默认替人做主是错的。
fn uninstall_scope(a: &AgentStatus) -> Option<UninstallScope> {
    // 清单/环境探测失败只保守地保留旧两格,不能让展示问题阻断卸载。
    let binary_inside_owns = binary_inside_owns(&a.agent_id).unwrap_or(false);
    if binary_inside_owns {
        // 只有一件真实选择: owns 树同时包含配置、数据和二进制。再画一格
        // 「软件本体」会暗示它能独立保留,所以直接进入同意句而不画假勾选表。
        return Some(UninstallScope {
            data: true,
            software: false,
            binary_inside_owns: true,
        });
    }
    let items = vec![
        format!(
            "config & data · ~/.{} — skills, MCP declarations, conversations, memory",
            a.agent_id
        ),
        "the software itself · runs its package-manager uninstall".to_string(),
    ];
    let checked = match prompt_checklist(
        &menu_theme(),
        Checklist {
            prompt: "What to remove · Space toggle · Enter confirm · Esc back",
            header: None,
            items: &items,
            checked: vec![true, false],
            select_all: false,
            page: crate::browse::viewport(CHECKLIST_RESERVED),
        },
    ) {
        Ok(Some(v)) => v,
        _ => return None,
    };
    let scope = UninstallScope {
        data: checked.contains(&0),
        software: checked.contains(&1),
        binary_inside_owns: false,
    };
    if !scope.data {
        // 只卸软件本体、留着配置,不是 uninstall 能表达的事:它删的就是
        // 那些文件。与其假装支持,不如说清楚该去哪做——那条卸载命令本来
        // 就会印在正常一趟的结尾。
        eprintln!(
            "  {}",
            muted().apply_to(
                "uninstall is the file deletion — unchecking it leaves nothing to do; \
                 to remove only the software, run its package-manager command (a normal \
                 run prints it)"
            )
        );
        return None;
    }
    Some(scope)
}

/// 同意门那一句里的数字摘要:`SIZE, N conversations, N skills, N memories`。
///
/// 计数来自索引的 `kind_counts`——某类资源一行都没有时键不存在,省略那一
/// 项而不是印「0 conversations」:没有会话的 agent 不该被这句话暗示有。
fn uninstall_summary(a: &AgentStatus) -> String {
    let mut parts = vec![human_bytes(a.bytes)];
    for (kind, word) in [
        ("session", "conversations"),
        ("skill", "skills"),
        ("memory", "memories"),
    ] {
        if let Some(n) = a.kind_counts.get(kind) {
            parts.push(format!("{n} {word}"));
        }
    }
    parts.join(", ")
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
    // 默认全勾:方向与 sync 的目标表相反,见 [`initial_checked`]。
    let checked = initial_checked(items.len(), true);
    let picked = match prompt_checklist(
        &menu_theme(),
        Checklist {
            prompt: "Which agents · Space toggle · a all/none · Enter confirm · Esc back",
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
/// 列宽按**本批**数据量,不写死——Table 按 `display_width` 量,宽字符
/// 不会顶歪下一列。第三列(可回收量)吃余量:前两列把行顶到终端宽附近时
/// 先截它,整行宽不越过终端预算。
fn agent_choice_rows(agents: &[AgentStatus]) -> Vec<String> {
    agent_choice_rows_at(agents, term_width())
}

/// 宽度算法本体;`cols` 由测试直接给(60 / 80 / 200),不必 mock 终端。
fn agent_choice_rows_at(agents: &[AgentStatus], cols: usize) -> Vec<String> {
    let mut table = Table::new(vec!["", "", "", ""]);
    table.right_align(&[1, 2]);
    // 第三列(可回收量)吃余量,地板 8 与旧版手算同数。
    //
    // 尾巴那个固定后缀「 reclaimable」安置成第四列而不是拼进第三列:
    // flex 列按余量截断,拼进去会让截断吃掉后缀——旧版只截数字、后缀
    // 永远整词印出,换成独立列后同一契约由列宽算法守住。
    table.flex_col(2);
    for a in agents {
        table.push_row(vec![
            a.agent_id.clone(),
            human_bytes(a.bytes),
            human_bytes(a.clean_bytes.values().sum()),
            "reclaimable".to_string(),
        ]);
    }
    // 表头四格全空:这张表没有表头,消费方只取行,丢头留行。
    table.rows_at(Prefix::Checkbox, cols).1
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
    /// 祈使原形(小写),用在提示行 `Enter {} the checked`——与其他键位
    /// 提示(move / page / toggle)同一词形。
    fn word(self) -> &'static str {
        match self {
            Verb::Clean => "clean",
            Verb::Prune => "prune",
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
                "Space toggle · a all/none · Enter {} the checked · Esc back",
                verb.word()
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
///
/// 行宽预算 = `cols - 前缀宽 - 1`,前缀宽度由 [`Prefix::width`] 一家给、
/// 不许调用方手数。旧版在这里手算过一笔账:把 `❯ [x] ` 按 4 列算,
/// 少的 2 列全进了路径列,行宽恰好 = 终端宽 + 1,每一行都折——而控件
/// 按逻辑行计数、`clear_last_lines` 按物理行擦,折一行残影每按一次翻
/// 一倍,这就是交互式 clean 残影翻倍的根源。这笔账现在由 [`Table::rows`]
/// 的预算算法统一守。
fn checklist_rows_at(items: &[&PlanItem], now: i64, cols: usize) -> (String, Vec<String>) {
    let mut table = Table::new(vec!["AGENT", "KIND", "FREES", "IDLE", "PATH"]);
    table.right_align(&[2, 3]);
    // 路径列吃余量。旧版手写过一个 16 列的地板,已删——理由同上:预算不许
    // 被任何旋钮顶破,那正是这段注释记着的残影病史的成因。
    table.flex_col(4);
    for i in items {
        table.push_row(vec![
            i.agent_id.clone(),
            row_kind(i).to_string(),
            human_bytes(i.bytes),
            idle_days(i.last_used_ms, now),
            display_tilde(&i.path),
        ]);
    }
    table.rows_at(Prefix::Checkbox, cols)
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

/// 一句 y/N。默认 No —— 回车不该等于同意。方向键 / `y` / `n` 切换,
/// Enter 确认,`Esc` 与选 No 同路(都是「什么都没动」)。
fn ask(theme: &ColorfulTheme, prompt: &str) -> Approval {
    match prompt_confirm(theme, prompt, false) {
        Ok(Some(true)) => Approval::Yes,
        Ok(Some(false)) | Ok(None) => Approval::No,
        Err(_) => Approval::Aborted,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 菜单只剩顶层一屏(名词直接进列表,见模块文档)。这个助手早年沿
    /// `Route::Group` 收集全部子屏;组撤了它只剩 TOP——留着它,是让下面
    /// 的守卫在有人再加一屏时改一处就能全覆盖。
    fn all_menus() -> Vec<&'static Menu> {
        vec![&TOP]
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

    /// 回菜单前的清屏必须先把这一屏滚进 scrollback:`ESC[2J` 擦掉的内容在
    /// ghostty / kitty 上是连同历史一起没的(ghostty#905),命令结果那一屏
    /// 直接擦就等于把用户刚跑出来的答案销毁。承重点只有一个——LF 的条数等于
    /// 屏高,少一条就少滚一行,那一行正是留在屏上被 `2J` 擦掉的那一行。
    #[test]
    fn 回菜单的清屏先把整屏滚进历史() {
        for rows in [1_u16, 12, 24, 30, 200] {
            let seq = scroll_out_sequence(rows);
            assert_eq!(
                seq.chars().filter(|c| *c == '\n').count(),
                usize::from(rows),
                "rows={rows}"
            );
            assert!(seq.ends_with("\x1b[2J\x1b[H"), "rows={rows}: {seq:?}");
        }
        // 拿不到屏高时(size 返回 0)也得滚一行,不能一行不滚就擦。
        assert_eq!(scroll_out_sequence(0), "\n\x1b[2J\x1b[H");
    }

    /// 头部的行数必须真的等于它占的屏幕行数——任何一行宽过终端就会折行,
    /// 「行数 = 高度」一旦失效,申报给页高的数字就是错的。招牌与面包屑都要
    /// 在窄屏上自己退场,不能靠终端替它折;扫描中那一行同理。
    #[test]
    fn 头部任何一行都不折行() {
        for cols in [12_usize, 20, 26, 33, 34, 40, 60, 80, 200] {
            for rows in [6_usize, 12, 19, 20, 24, 30, 60] {
                for scanning in [false, true] {
                    for line in header_lines(rows, cols, scanning) {
                        assert!(
                            console::measure_text_width(&line) <= cols,
                            "{rows}x{cols} scanning={scanning}:这一行会折:{line:?}"
                        );
                    }
                }
            }
        }
    }

    /// 头部 + 菜单必须一屏装得下,算式抄 `browse.rs` 的 `browse_一屏不溢出`:
    /// 控件画问句 1 行、条目若干行、多于一页时页脚 1 行,画完光标还要占 1 行。
    ///
    /// 这条是招牌的守门人:头部只要不申报进 [`CHECKLIST_RESERVED`](或者
    /// 招牌在矮屏上不肯退场),它立刻变红——那正是 banner 时代留下的旧账,
    /// 而代价是退场时 `clear_last_lines` 擦不到滚出屏幕的行,屏幕上留半张菜单。
    ///
    /// 扫描态一起验:后台扫描那一行也占一行屏幕,不跟着算进页高的话,这笔
    /// 旧账就会在每次冷启动的头几秒原样重演。
    #[test]
    fn 头部加菜单一屏不溢出() {
        for rows in 4..=60usize {
            for menu in all_menus() {
                for scanning in [false, true] {
                    let above = header_lines(rows, 100, scanning).len();
                    let len = menu.items.len();
                    let avail = crate::browse::viewport_of(rows, CHECKLIST_RESERVED + above);
                    let body = crate::prompt::body_page(avail, false, len);
                    let footer = usize::from(body < len);
                    let drawn = above + 1 + body.min(len) + footer;
                    // 控件自己的地板:问句 1 + 保底 1 行条目 + 页脚,加上光标那一行
                    // 还放不下,就不是页高能救的组合(与 browse 的同名断言同口径)。
                    let min_drawn = above + 1 + 1 + footer;
                    if min_drawn + 1 > rows {
                        continue;
                    }
                    assert!(
                        drawn < rows,
                        "{rows} 行终端 / 「{}」{len} 条 / 头部 {above} 行 / scanning={scanning}:画了 {drawn} 行,溢出",
                        menu.prompt
                    );
                }
            }
        }
    }

    /// 扫描中的头部只比扫完多一行,不多不少——多出来的正是 [`SCANNING_LINE`]。
    ///
    /// 这一行必须跟着 `Vec` 的长度一起被申报进页高(`头部加菜单一屏不溢出`
    /// 连扫描态一起验),少算它,矮终端里菜单尾巴就会被顶出屏幕,而退场时
    /// `clear_last_lines` 擦不回滚出去的行。
    ///
    /// 列宽从 12 起:那一行连缩进一共 11 列,再窄就装不下,`header_lines`
    /// 会整行丢掉它——丢掉是对的,折行才会毁掉「行数 = 高度」。
    #[test]
    fn 扫描中的头部恰好多一行() {
        for cols in [12_usize, 20, 40, 60, 80, 200] {
            for rows in [6_usize, 12, 19, 20, 24, 30, 60] {
                let idle = header_lines(rows, cols, false);
                let busy = header_lines(rows, cols, true);
                assert_eq!(
                    busy.len(),
                    idle.len() + 1,
                    "{rows}x{cols}:扫描中该恰好多一行"
                );
                assert!(
                    busy.iter().any(|l| l.contains(SCANNING_LINE)),
                    "{rows}x{cols}:多出来的那行该是 {SCANNING_LINE}"
                );
                assert!(
                    idle.iter().all(|l| !l.contains(SCANNING_LINE)),
                    "{rows}x{cols}:扫完了就该不见"
                );
            }
        }
    }

    /// 造一个结局已定的后台扫描手柄。三条降级路径都得在不碰真实索引的前提下
    /// 走一遍——真去扫一次盘,这几条断言就成了 4 秒一跑的集成测试。
    fn finished_scan(outcome: anyhow::Result<Freshness>) -> BackgroundScan {
        BackgroundScan(Some(std::thread::spawn(move || outcome)))
    }

    /// 收账只收一次:`join` 之后手柄被 `take` 掉,后续每一轮循环都直接过,
    /// 不会再等一次已经等过的扫描,头部也不会再印那一行。
    #[test]
    fn 后台扫描只收一次账() {
        let mut scan = finished_scan(Ok(Freshness::UpToDate));
        scan.join();
        assert!(!scan.running(), "join 过之后不该再算「还在扫」");
        // 手柄已经没了:这一次必须直接返回,而不是 panic。
        scan.join();
        assert!(!scan.running());
    }

    /// 扫描没成功不阻断菜单。两条降级路径都得走得通:报错咽成一行 warning,
    /// 撞上另一个 duster 实例的写锁([`Freshness::SkippedLocked`])连 warning
    /// 都不印——读路径全是 `open_readonly`,别人写的时候照样看得见。
    #[test]
    fn 扫描失败与撞锁都不阻断菜单() {
        let mut broken = finished_scan(Err(anyhow::anyhow!("index is on fire")));
        broken.join();
        assert!(!broken.running());

        let mut locked = finished_scan(Ok(Freshness::SkippedLocked));
        locked.join();
        assert!(!locked.running());
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

    /// 顶层菜单是用户可见的稳定契约:条目数与顺序变动都会改变用户的选择。
    /// `search` 已撤(命令行保留),back 条目全菜单绝迹——Esc 是唯一的
    /// 返回通道;doctor 也不应暴露已经删除的旧旗标。
    #[test]
    fn 顶层菜单条目稳定_无search_无back() {
        let names = |items: &[MenuItem]| items.iter().map(|item| item.name).collect::<Vec<_>>();
        assert_eq!(names(&TOP_ITEMS), vec![
            "clean", "prune", "uninstall", "status", "skill", "mcp", "session", "memory",
            "doctor", "quit",
        ]);
        for menu in all_menus() {
            assert!(menu.items.iter().all(|item| item.name != "back"));
            assert!(menu.items.iter().all(|item| item.name != "search"));
            assert!(menu.items.iter().all(|item| !item.name.contains("--secrets")));
        }
    }

    /// M2 的三组必须在菜单上找得到。`--help` 里有而菜单里没有,对只会
    /// 敲一个 `duster` 的用户来说就等于不存在——这正是这次要修的那个洞。
    ///
    /// `diff` 不在此列:它是通用文件/文件夹对比,从顶层刻意撤下(见顶层
    /// 菜单的注释),菜单里不再有它的落脚点——`mcp` 的 compare 动作已随
    /// `mcp diff` 撤下,铺平表的 STATE 列直接回答「几家声明一样吗」。
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

    /// `scan` / `scan --full` 从菜单撤下而留在命令行:`ensure_fresh` 上线后
    /// 用户动手时索引永远是新的,「去扫描」只剩门面价值(理由见顶层菜单的
    /// 注释)。`duster scan` / `duster scan --full` 在 clap 里原样保留,这里
    /// 只钉菜单。
    #[test]
    fn 顶层菜单没有_scan() {
        assert!(
            !TOP_ITEMS
                .iter()
                .any(|i| i.name == "scan" || i.name == "scan --full"),
            "scan 应从顶层菜单撤下,命令行 `duster scan` 原样保留"
        );
    }

    /// 菜单行加上 `❯ ` 前缀后不得碰到终端宽——它是全项目唯一一张不走
    /// [`Table`] 的列表,却和别的列表一样由 `prompt_pick` 画、按逻辑行计数、
    /// 按物理行擦:折一行,残影就每按一次翻一倍。
    ///
    /// 60 列是这条守卫的真正战场:80 列下只有超长文案才折,而 `skill` 那条
    /// 曾经 94 字符、整行 108 列,连 100 列的终端都装不下。
    #[test]
    fn 菜单行加前缀严格小于终端宽() {
        for menu in all_menus() {
            for cols in [60usize, 80, 200] {
                for line in rendered_items_at(menu.items, cols) {
                    let plain = console::strip_ansi_codes(&line);
                    let w = Prefix::Cursor.width() + display_width(&plain);
                    assert!(w < cols, "{cols} 列终端:行宽 {w} 超限: {plain}");
                }
            }
        }
    }

    /// 说明文案本身就该放得进 80 列,不该靠截断兜底——截断丢掉的正是
    /// 「这条命令干什么」的后半句。守卫在 [`rendered_items_at`] 里,
    /// 这条测试守的是文案作者。
    #[test]
    fn 菜单说明在80列下不被截断() {
        for menu in all_menus() {
            for (line, item) in rendered_items_at(menu.items, 80).iter().zip(menu.items) {
                let plain = console::strip_ansi_codes(line);
                assert!(
                    plain.trim_end().ends_with(item.desc),
                    "`{}` 的说明在 80 列下被截断了,把文案改短: {}",
                    item.name,
                    item.desc
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

    /// 两张勾选表的默认方向必须相反:范围表(`prompt_agent`,clean / prune
    /// 的 `--agent`)默认全勾,因为命令行不带 `--agent` 就是「全部」;sync
    /// 的目标表默认全不勾,因为 `--to` 是 `required = true`,命令行没有
    /// 「全部」这个默认——替用户勾满等于发明一个 CLI 表达不出的默认值,
    /// 而它的后果是往每个 agent 的配置文件里写字。
    #[test]
    fn 勾选表默认方向相反() {
        assert!(
            initial_checked(4, true).iter().all(|c| *c),
            "范围表默认全勾"
        );
        assert!(
            initial_checked(4, false).iter().all(|c| !*c),
            "sync 目标表默认全不勾"
        );
        assert_eq!(initial_checked(4, false).len(), 4, "一行一个勾选框");
    }


    /// 目标勾选表在 80 列终端下必须放得下那条出了名的长路径:路径列不截
    /// 的话,`claude-desktop  create  ~/Library/Application Support/…` 一行
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


    /// 下钻菜单的条目是用户可见的稳定契约:数量与措辞逐字钉死。back 没有
    /// 条目——Esc 就是返回(全菜单同一条导航契约),这里顺带守住「不许
    /// 有人把 back 加回来」。两条 delete 的先后顺序同样是契约:归档删在
    /// 前(默认留退路),真删在后。
    #[test]
    fn 会话下钻动作_数量措辞稳定且无back() {
        assert_eq!(
            session_detail_actions(),
            [
                "read it in full (every turn, nothing truncated)",
                "export as markdown",
                "export as json",
                "plant a text-only copy into another agent (tool calls are not carried over)",
                "delete it (a readable copy is archived into ~/agent-duster-exports/ first)",
                "delete it for good (no archive — the conversation is gone)",
            ]
        );
        let a = session_detail_actions();
        assert_eq!(
            a[4],
            "delete it (a readable copy is archived into ~/agent-duster-exports/ first)"
        );
        assert_eq!(a[5], "delete it for good (no archive — the conversation is gone)");
        assert!(session_detail_actions().iter().all(|a| a != "back"));
    }

    /// memory 详情动作:migrate + 两条 delete,back 没有条目——Esc 就是
    /// 返回。两条 delete 的先后顺序同样是契约:归档删在前(默认留退路),
    /// 真删在后。
    #[test]
    fn 记忆下钻动作_数量措辞稳定且无back() {
        assert_eq!(
            memory_detail_actions(),
            [
                "migrate it into another agent's memory",
                "delete it (a readable copy is archived into ~/agent-duster-exports/ first)",
                "delete it for good (no archive — the memory is gone)",
            ]
        );
        let a = memory_detail_actions();
        assert_eq!(
            a[1],
            "delete it (a readable copy is archived into ~/agent-duster-exports/ first)"
        );
        assert_eq!(a[2], "delete it for good (no archive — the memory is gone)");
    }

    /// 详情屏正文前的元信息行:`路径 · 体积 · 上次修改`,逐字钉死。
    /// 路径不在 home 下,`display_tilde` 原样返回(不依赖测试机的 $HOME);
    /// 「上次修改」与 LAST USED 列同一口径,没有证据印 `never`(绝不编
    /// 1970 年出来),有证据印相对时间。
    #[test]
    fn 记忆详情元信息行_路径体积与上次修改() {
        let e = MemoryEntry {
            agent_id: "codex".into(),
            project: None,
            title: "AGENTS.md".into(),
            category: None,
            store: duster_core::memory::MemoryStore::Markdown,
            path: std::path::PathBuf::from("/tmp/mem/AGENTS.md"),
            bytes: 2048,
            mtime_ms: 0,
            last_used_ms: None,
        };
        assert_eq!(memory_meta_line(&e), "/tmp/mem/AGENTS.md · 2 KB · never");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let stale = MemoryEntry {
            // 恰好三天前：`3 days ago`，与墙钟无关。
            last_used_ms: Some(now - 3 * 86_400_000),
            ..e
        };
        assert_eq!(
            memory_meta_line(&stale),
            "/tmp/mem/AGENTS.md · 2 KB · 3 days ago"
        );
    }

    /// 批量确认句的英文并列与复数:1 个 / 多个 / 三类混排各有一个形状。
    #[test]
    fn 批量删除确认句_复数与并列稳定() {
        assert_eq!(plural("duster block", 1), "1 duster block");
        assert_eq!(plural("duster block", 2), "2 duster blocks");
        assert_eq!(plural("duster-created file", 2), "2 duster-created files");
        assert_eq!(join_parts(&[]), "");
        assert_eq!(join_parts(&["a".to_string()]), "a");
        assert_eq!(join_parts(&["a".to_string(), "b".to_string()]), "a and b");
        assert_eq!(
            join_parts(&[
                "2 duster blocks".to_string(),
                "1 duster-created file".to_string(),
                "1 file you wrote yourself".to_string(),
            ]),
            "2 duster blocks, 1 duster-created file and 1 file you wrote yourself"
        );
    }

    /// 一行测试数据:能造出「ID / AGENT / PROJECT / TURNS / SIZE」五列
    /// 都顶满的表;`last_turn_ms` 恒 None,LAST USED 列因此恒为
    /// `relative_time(None)` = `never`,与墙钟无关。
    fn session_row(rid: i64, agent: &str, cwd: Option<&str>, bytes: u64, turns: u64) -> SessionRow {
        SessionRow {
            rid,
            agent_id: agent.into(),
            key: "k".into(),
            path: "/x.jsonl".into(),
            cwd: cwd.map(Into::into),
            bytes,
            turns,
            last_turn_ms: None,
            compressed: false,
        }
    }

    /// 会话表的每一行(含表头)加上控件前缀 `❯ [x] `(6 列)后,显示宽度
    /// 必须严格小于终端宽度——行宽碰到终端宽就会折成两个物理行,而控件按
    /// 逻辑行计数、`clear_last_lines` 按物理行擦,残影每按一次翻一倍。数据把
    /// AGENT / PROJECT 两列顶满,60 列档恰好把 LAST USED 逼到地板。
    #[test]
    fn session_list_rows_行宽不超终端() {
        let rows = [
            session_row(500, "claude-code", Some("/Users/me/Code/agent-duster"), 2048, 42),
            session_row(7, "pi", Some("/Users/me/Code/duster-core"), 1048576, 2),
            session_row(50000, "gpt-oss-120b", None, 0, 0),
        ];
        for cols in [60usize, 80, 200] {
            let (header, lines) = session_list_rows_at(&rows, cols);
            for line in std::iter::once(&header).chain(lines.iter()) {
                let w = Prefix::Checkbox.width() + display_width(line);
                assert!(
                    w < cols,
                    "{cols} 列终端:行宽 {w} 超限: {line}"
                );
            }
        }
    }

    /// 会话表列名 / 顺序 / 对齐 / 单元格文案的逐字契约,按字节切片钉死
    /// (数据全 ASCII,字节 == 列)。ID 带 `s` 前缀(与 `cmd::session::
    /// render_list` 同口径)、右对齐;AGENT / PROJECT / LAST USED 左对齐,
    /// TURNS / SIZE 右对齐;LAST USED 恒 `never`(墙钟无关),PROJECT 列是
    /// [`cmd::session::project_cell`] 的最后两段口径。
    #[test]
    fn session_list_rows_列名顺序对齐与单元格文案照旧() {
        let rows = [
            session_row(500, "claude-code", Some("/Users/me/Code/agent-duster"), 2048, 42),
            session_row(7, "pi", Some("/Users/me/Code/duster-core"), 1048576, 2),
            session_row(123456, "gpt-oss-120b", None, 0, 0),
        ];
        let (header, lines) = session_list_rows_at(&rows, 80);
        // 列宽按本批数据取:ID 7(`s123456`)/ AGENT 12(`gpt-oss-120b`)/
        // PROJECT 17(`Code/agent-duster`)/ TURNS 5(表头)/ SIZE 4(表头)/
        // LAST USED 9(表头),列间两空格。表头与数据行共用同一套宽度,
        // 所以每一列的起始字节在两者里必须相同——下面逐列钉的就是这件事。
        assert_eq!(&header[0..7], "     ID");
        assert_eq!(&header[9..14], "AGENT");
        assert_eq!(&header[23..30], "PROJECT");
        assert_eq!(&header[42..47], "TURNS");
        assert_eq!(&header[49..53], "SIZE");
        assert_eq!(&header[55..64], "LAST USED");

        assert_eq!(&lines[0][0..7], "   s500");
        assert_eq!(&lines[0][9..20], "claude-code");
        assert_eq!(&lines[0][23..40], "Code/agent-duster");
        assert_eq!(&lines[0][42..47], "   42");
        assert_eq!(&lines[0][49..53], "2 KB");
        assert_eq!(&lines[0][55..], "never");

        assert_eq!(&lines[1][0..7], "     s7");
        assert_eq!(&lines[1][9..21], format!("{:<12}", "pi"));
        assert_eq!(&lines[1][23..40], format!("{:<17}", "Code/duster-core"));
        assert_eq!(&lines[1][42..47], "    2");
        assert_eq!(&lines[1][49..53], "1 MB");
        assert_eq!(&lines[1][55..], "never");

        // cwd 取不到印 `-`(绝不从会话文件名反推),TURNS / SIZE 右对齐。
        assert_eq!(&lines[2][0..7], "s123456");
        assert_eq!(&lines[2][9..21], "gpt-oss-120b");
        assert_eq!(&lines[2][23..40], format!("{:<17}", "-"));
        assert_eq!(&lines[2][42..47], "    0");
        assert_eq!(&lines[2][49..53], " 0 B");
        assert_eq!(&lines[2][55..], "never");
    }

    /// 一行测试副本。`path` 可给:铺平后 PATH 是吃余量的那一列,行宽测试
    /// 要靠长路径把余量吃干。`last_used_ms` 由调用方给:夹具里 None 与真值
    /// 两种都要有(None 印 `never`,有值印相对时间),LAST USED 列才不是
    /// 一行死数据,两种渲染路径才都被行宽测试压过。
    fn copy_at(
        agent: &str,
        path: &str,
        last_used_ms: Option<i64>,
    ) -> duster_core::skill_ops::SkillCopy {
        duster_core::skill_ops::SkillCopy {
            agent_id: agent.into(),
            path: PathBuf::from(path),
            state: duster_core::skill_ops::DupState::Identical,
            link_target: None,
            tree_hash: "abc123".into(),
            bytes: 0,
            install_bytes: 0,
            last_used_ms,
        }
    }

    /// 铺平后的副本表:每一行(含表头)加上控件前缀 `❯ [x] `(6 列)后不得
    /// 折行——行宽碰到终端宽就折成两个物理行,而控件按逻辑行计数、按物理行
    /// 擦,残影每按一次翻一倍。夹具用长 skill 名 + 长路径把 60 列档逼到底。
    #[test]
    fn skill_copy_rows_行宽不超终端() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let groups = [
            SkillGroup {
                name: "the-longest-skill-name-anyone-ever-wrote".into(),
                state: DupState::Drifted,
                copies: vec![
                    copy_at("claude-code", "/Users/me/.claude/skills/connect-chrome", None),
                    copy_at("gpt-oss-120b", "/Users/me/.config/opencode/skills/x", Some(now_ms)),
                ],
                diff: None,
                warnings: vec![],
            },
            SkillGroup {
                name: "frontend".into(),
                state: DupState::Single,
                copies: vec![copy_at("codex", "/Users/me/.codex/skills/frontend", None)],
                diff: None,
                warnings: vec![],
            },
        ];
        for cols in [60usize, 80, 200] {
            let (header, lines) = skill_copy_rows_at(&groups, cols);
            for line in std::iter::once(&header).chain(lines.iter()) {
                let w = Prefix::Checkbox.width() + display_width(line);
                assert!(w < cols, "{cols} 列终端:行宽 {w} 超限: {line}");
            }
        }
    }

    /// 铺平的两条硬契约:**一行一份副本**(不是一行一组),且 skill 名
    /// **每行都印**——表会翻页,一组的几份可能被切到下一页,而每行都是可
    /// 勾选的删除目标,空名字的行既认不出属于谁又能被选中删掉。
    #[test]
    fn skill_copy_rows_一行一份且组名每行都印() {
        let groups = [SkillGroup {
            name: "dup".into(),
            state: DupState::Drifted,
            copies: vec![
                copy_at("claude-code", "/x/a", None),
                copy_at("claude-code", "/x/b", Some(now_ms())),
            ],
            diff: None,
            warnings: vec![],
        }];
        let (header, lines) = skill_copy_rows_at(&groups, 200);
        assert!(header.starts_with("SKILL"), "{header}");
        assert_eq!(lines.len(), 2, "两份副本就是两行: {lines:?}");
        for line in &lines {
            assert!(line.starts_with("dup"), "组名每行都要印: {line}");
        }
        assert!(lines[0].contains("/x/a") && lines[1].contains("/x/b"), "{lines:?}");
    }

    /// LAST USED 列与 CLI 打印表同口径:None → `never`(没有时间戳证据,
    /// 不是「1970 年用过」),有值 → 相对时间。表头恒在 SKILL 表头之后、
    /// PATH 之前——PATH 留作吃余量的尾列。
    #[test]
    fn skill_copy_rows_last_used_列_never_与相对时间() {
        let groups = [SkillGroup {
            name: "dup".into(),
            state: DupState::Drifted,
            copies: vec![
                copy_at("claude-code", "/x/a", None),
                copy_at("codex", "/x/b", Some(now_ms())),
            ],
            diff: None,
            warnings: vec![],
        }];
        let (header, lines) = skill_copy_rows_at(&groups, 200);
        let last_used_at = header.find("LAST USED").expect("LAST USED 表头要在: {header}");
        let path_at = header.find("PATH").expect("PATH 表头要在: {header}");
        assert!(last_used_at < path_at, "{header}");
        assert!(lines[0].contains("never"), "{lines:?}");
        assert!(lines[1].contains("just now"), "{lines:?}");
    }

    /// 墙钟无关的「刚刚」:行宽与渲染测试都要一个真实的时间戳,取当前
    /// 时刻造一个,`relative_time` 正好落进 `just now` 档。
    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    /// 组详情下钻的动作表:link + delete(「对比两份副本」已随统一列表
    /// 流撤下,要比就拿详情表里的两条 PATH 去喂系统 diff);back 没有条目
    /// ——Esc 就是返回。数量与措辞逐字钉死。
    /// delete 那句必须跟着副本形态走。skill 删除不归档(见 core 的
    /// [`duster_core::skill_ops::remove`] 文档):真目录整棵删、软链副本
    /// 删的只是链接本身。所以只有两档——全是软链(含悬空)说只摘链接,
    /// 其余(含混着)都说「直接删、不留副本」,这句对每一份副本都是实话。
    #[test]
    fn skill_group_actions_只有link和delete且无back() {
        let group = |copies: Vec<duster_core::skill_ops::SkillCopy>| SkillGroup {
            name: "s".into(),
            state: DupState::Drifted,
            copies,
            diff: None,
            warnings: vec![],
        };
        let with_state = |st: DupState| {
            let mut c = copy_at("claude-code", "/x/a", None);
            c.state = st;
            c
        };

        // 真目录副本:直接删、不留副本,不再提归档。
        assert_eq!(
            skill_group_actions(&group(vec![with_state(DupState::Identical)])),
            [
                "link this skill into another agent (one copy on disk)",
                "delete a copy (deleted outright — no archive, nothing to restore from)",
            ]
        );

        // 全是软链(含悬空):只摘链接,一个字都不许提归档。
        for st in [DupState::Linked, DupState::Broken] {
            let actions = skill_group_actions(&group(vec![with_state(st)]));
            assert_eq!(
                actions[1], "delete a copy (only the link is removed — the content it points to stays)",
                "{st:?}"
            );
        }

        // 混着:跟真目录同档——「直接删、不留副本」对每一份都是实话,
        // 软链那份的「内容留在别处」留给逐份确认那一句。
        let mixed = group(vec![
            with_state(DupState::Identical),
            with_state(DupState::Broken),
        ]);
        assert_eq!(
            skill_group_actions(&mixed)[1],
            "delete a copy (deleted outright — no archive, nothing to restore from)"
        );
    }

    /// 多选行(agent / 总量 / 可回收量 / reclaimable)加上 `❯ [x] ` 前缀
    /// (6 列)后不得折行。这张表没有表头,`agent_choice_rows` 只交数据行。
    #[test]
    fn agent_choice_rows_行宽不超终端() {
        let agent = |id: &str, bytes: u64, clean: u64| AgentStatus {
            agent_id: id.into(),
            display_name: None,
            last_scan_ms: None,
            bytes,
            kind_counts: std::collections::BTreeMap::new(),
            kind_bytes: std::collections::BTreeMap::new(),
            clean_bytes: [("l1".to_string(), clean)].into_iter().collect(),
        };
        let agents = [
            agent("claude-code", 12_884_901_888, 2_147_483_648),
            agent("gpt-oss-120b", 5_368_709_120, 0),
        ];
        for cols in [60usize, 80, 200] {
            for line in agent_choice_rows_at(&agents, cols) {
                let w = 6 + display_width(&line);
                assert!(
                    w < cols,
                    "{cols} 列终端:行宽 {w} 超限: {line}"
                );
            }
        }
    }

    /// 卸载同意门那一句必须自己说清三件事:删谁、丢什么(带数字)、
    /// **什么都不归档且不可撤销**。
    ///
    /// 这是这条流程唯一的一道门——范围表勾完就只剩它了,所以它漏一个字
    /// 都是用户没被告知。旧版把「an archive goes to ~/agent-duster-exports
    /// first」印在这里,那是句谎话:菜单这条路 `archive: Some(false)`,
    /// 一个包都不打。这条测试防的就是有人把那句安慰话加回来。
    #[test]
    fn 卸载确认句_说清不归档且不可撤销() {
        let mut counts = std::collections::BTreeMap::new();
        counts.insert("session".to_string(), 340u64);
        counts.insert("skill".to_string(), 12u64);
        let a = AgentStatus {
            agent_id: "qoder".into(),
            display_name: None,
            last_scan_ms: None,
            bytes: 1_288_490_188,
            kind_counts: counts,
            kind_bytes: std::collections::BTreeMap::new(),
            clean_bytes: std::collections::BTreeMap::new(),
        };

        let only_data = UninstallScope { data: true, software: false, binary_inside_owns: false }.confirm_line(&a);
        assert!(only_data.contains("Delete qoder for good"), "{only_data}");
        assert!(only_data.contains("340 conversations"), "{only_data}");
        assert!(only_data.contains("12 skills"), "{only_data}");
        assert!(
            only_data.contains("nothing is archived, this cannot be undone"),
            "{only_data}"
        );
        // 没勾软件本体就不许暗示会去动它。
        assert!(!only_data.contains("software"), "{only_data}");
        // 旧口径一个字都不许回来。
        assert!(!only_data.contains("agent-duster-exports"), "{only_data}");

        // 勾了软件本体:代跑包管理器这件事必须写在同一句里。
        let with_sw = UninstallScope { data: true, software: true, binary_inside_owns: false }.confirm_line(&a);
        assert!(
            with_sw.contains("the software itself is uninstalled too"),
            "{with_sw}"
        );
    }
    /// 二进制落在 owns 树内时,确认句必须明说第一格连 binary 一起删除;
    /// 这是跳过单项勾选表后的等价断言。
    #[test]
    fn 卸载确认句_说明数据树包含二进制() {
        let a = AgentStatus {
            agent_id: "opencode".into(),
            display_name: None,
            last_scan_ms: None,
            bytes: 1,
            kind_counts: std::collections::BTreeMap::new(),
            kind_bytes: std::collections::BTreeMap::new(),
            clean_bytes: std::collections::BTreeMap::new(),
        };
        let line = UninstallScope { data: true, software: false, binary_inside_owns: true }.confirm_line(&a);
        assert!(line.contains("binary itself"), "{line}");
    }

    /// 尾巴那个固定后缀「 reclaimable」是文案不是数据:flex 列按余量截断,
    /// 它可以吃掉可回收量这个数字,不许吃掉后缀——旧版只截数字、后缀永远
    /// 整词印出,换成第四列后同一契约由列宽算法守住。用长 agent 名把 40 列
    /// 逼到极限,后缀必须仍然整词在行尾。
    #[test]
    fn agent_choice_rows_固定后缀永远整词印出() {
        let agent = AgentStatus {
            agent_id: "a-very-long-agent-id".into(),
            display_name: None,
            last_scan_ms: None,
            bytes: 12_884_901_888,
            kind_counts: std::collections::BTreeMap::new(),
            kind_bytes: std::collections::BTreeMap::new(),
            clean_bytes: [("l1".to_string(), 2_147_483_648)].into_iter().collect(),
        };
        for cols in [40usize, 60, 80, 200] {
            for line in agent_choice_rows_at(std::slice::from_ref(&agent), cols) {
                assert!(
                    line.ends_with("reclaimable"),
                    "{cols} 列终端:后缀被截断或挪位: {line}"
                );
            }
        }
    }

    /// 第一道确认的摘要:数字来自索引的 `kind_counts`,取不到的项整段省略——
    /// 没有会话的 agent 不该被印成「0 conversations」。顺序固定:
    /// SIZE 打头,conversations / skills / memories 依次,与契约文案一致。
    #[test]
    fn uninstall_summary_数字来自统计且取不到的项省略() {
        let agent = |counts: &[(&str, u64)], bytes: u64| AgentStatus {
            agent_id: "fixture".into(),
            display_name: None,
            last_scan_ms: None,
            bytes,
            kind_counts: counts
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect(),
            kind_bytes: std::collections::BTreeMap::new(),
            clean_bytes: std::collections::BTreeMap::new(),
        };
        assert_eq!(
            uninstall_summary(&agent(
                &[("session", 40), ("skill", 3), ("memory", 2)],
                12_884_901_888
            )),
            "12 GB, 40 conversations, 3 skills, 2 memories"
        );
        // 只有会话:skills / memories 两段整段省略,不留「0 skills」。
        assert_eq!(
            uninstall_summary(&agent(&[("session", 7)], 536_870_912)),
            "512 MB, 7 conversations"
        );
        // 一种资源都没有:只剩体积。
        assert_eq!(
            uninstall_summary(&agent(&[], 1_073_741_824)),
            "1 GB"
        );
    }

    /// CJK 宽字符不能顶歪下一列:agent 名与路径都含双宽字时,路径列在表头
    /// 与每一行都从同一个**显示**列开始(按 `display_width` 量,不是字节数),
    /// 且整行 + 前缀仍不越线。
    #[test]
    fn cjk_宽字符不顶歪下一列() {
        let now = 1_000 * 86_400_000;
        let a = item(
            "Claudeコード",
            "/tmp/缓存/cache",
            300 * 1024,
            Some(now - 3 * 86_400_000),
        );
        let b = item("pi", "/tmp/x/logs", 2, None);
        for cols in [60usize, 80, 200] {
            let (header, rows) = checklist_rows_at(&[&a, &b], now, cols);
            // 与 `勾选表五列对齐且不含依据长句` 同一量法,但按显示宽度量——
            // 字节数在 CJK 面前是谎话。
            let at = |s: &str| {
                let i = s.find("/tmp").or_else(|| s.find("PATH")).unwrap();
                display_width(&s[..i])
            };
            assert_eq!(
                at(&header),
                at(&rows[0]),
                "{cols} 列:表头与数据行的路径列起点不一致"
            );
            assert_eq!(
                at(&rows[0]),
                at(&rows[1]),
                "{cols} 列:两行的路径列起点不一致"
            );
            for line in std::iter::once(&header).chain(rows.iter()) {
                let w = 6 + display_width(line);
                assert!(
                    w < cols,
                    "{cols} 列终端:行宽 {w} 超限: {line}"
                );
            }
        }
    }
    /// 列表骨架的空表收场:基线退出码原样保留,而且是 shown——那一两行
    /// 「空表说明」是打给人看的,回菜单前必须停一下,否则 0 毫秒就被菜单
    /// 盖掉。空表不建行、不下钻、没有批量动作、不重取。
    #[test]
    fn 列表骨架空结果保留基线退出码() {
        let empty_called = std::cell::Cell::new(false);
        let out = list_drill(
            OutputMode::Human,
            "test-list",
            Ok(Vec::<u8>::new()),
            EXIT_PARTIAL,
            |rows| rows.is_empty(),
            |_| empty_called.set(true),
            |_| panic!("空表不应建浏览行"),
            "Which item",
            |_, _| panic!("空表不应下钻"),
            |_, _| panic!("空表不应有批量动作"),
            || panic!("空表不应重取"),
        );
        assert_eq!(out.code, EXIT_PARTIAL);
        assert!(out.pause, "空表说明是打给人看的,回菜单前必须停一下");
        assert!(empty_called.get());
    }
}
