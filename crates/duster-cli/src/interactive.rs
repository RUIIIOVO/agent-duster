//! 交互式启动菜单:裸 `duster`(无子命令)时进入。
//!
//! - TTY 下:彩色 banner + 上下箭头选择命令(dialoguer),search/open 补交互输入。
//! - 非 TTY 或 `--json`:交互无意义,退化为打印帮助。
//! - banner 与菜单全部走 stderr(dialoguer 默认),stdout 仍只放命令结果,
//!   与 output.rs 的 stdout/stderr 契约一致。

use std::io::IsTerminal;
use std::path::Path;

use clap::CommandFactory;
use console::{Style, style};
use dialoguer::{Input, Select, theme::ColorfulTheme};

use crate::output::{EXIT_ERROR, EXIT_OK, OutputMode};
use crate::{Cli, cmd_open, cmd_scan, cmd_search, cmd_status};

/// 菜单条目:命令名、一句话说明、名字的颜色(逐条不同,即「多彩」)。
struct MenuItem {
    name: &'static str,
    desc: &'static str,
    color: fn(&Style) -> Style,
}

const MENU: [MenuItem; 6] = [
    MenuItem {
        name: "scan",
        desc: "Find your agents and index what they store",
        color: |s| s.clone().green(),
    },
    MenuItem {
        name: "scan --full",
        desc: "Same, but re-read every file from scratch",
        color: |s| s.clone().yellow(),
    },
    MenuItem {
        name: "status",
        desc: "See which agent uses how much disk",
        color: |s| s.clone().blue(),
    },
    MenuItem {
        name: "search",
        desc: "Search the text of past conversations",
        color: |s| s.clone().magenta(),
    },
    MenuItem {
        name: "open",
        desc: "Read one whole turn found by search",
        color: |s| s.clone().cyan(),
    },
    MenuItem {
        name: "quit",
        desc: "Leave",
        color: |s| s.clone().red(),
    },
];

/// 裸 `duster` 入口:TTY 进菜单循环,选中即执行对应命令,quit/Esc 退出。
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
    let theme = menu_theme();
    let items = rendered_items();

    let mut last_code = EXIT_OK;
    loop {
        let picked = Select::with_theme(&theme)
            .with_prompt("Pick a command")
            .items(&items)
            .default(0)
            .interact_opt();
        match picked {
            Ok(Some(i)) if MENU[i].name != "quit" => {
                eprintln!();
                last_code = dispatch(i, mode, index);
                eprintln!();
            }
            // quit / Esc / Ctrl-C:退出,带上最后一次命令的退出码。
            Ok(_) => return last_code,
            Err(_) => return EXIT_ERROR,
        }
    }
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
    eprintln!("  {}", style("↑/↓ move · Enter run · Esc quit").dim());
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

/// 预渲染菜单行:命令名按 [`MENU`] 各自着色、对齐,说明置灰。
fn rendered_items() -> Vec<String> {
    let width = MENU.iter().map(|m| m.name.len()).max().unwrap_or(0);
    MENU.iter()
        .map(|m| {
            let name = (m.color)(&Style::new().bold()).apply_to(format!("{:<width$}", m.name));
            format!("{name}  {}", style(m.desc).dim())
        })
        .collect()
}

/// 按菜单下标执行命令;search/open 先补齐交互式参数。
fn dispatch(i: usize, mode: OutputMode, index: Option<&Path>) -> i32 {
    match MENU[i].name {
        "scan" => cmd_scan(mode, index, false),
        "scan --full" => cmd_scan(mode, index, true),
        "status" => cmd_status(mode, index),
        "search" => prompt_search(mode, index),
        "open" => prompt_open(mode, index),
        _ => unreachable!("quit is handled by the menu loop"),
    }
}

/// 交互式收集 search 参数:query(≥3 字符)、可选 agent、limit(默认 20)。
fn prompt_search(mode: OutputMode, index: Option<&Path>) -> i32 {
    let theme = ColorfulTheme::default();
    let query: String = match Input::with_theme(&theme)
        .with_prompt("Search for (3 characters or more)")
        .validate_with(|s: &String| {
            if s.chars().count() >= 3 {
                Ok(())
            } else {
                Err("please type at least 3 characters")
            }
        })
        .interact_text()
    {
        Ok(q) => q,
        Err(_) => return EXIT_ERROR,
    };
    let agent: String = match Input::with_theme(&theme)
        .with_prompt("Only this agent (leave empty for all)")
        .allow_empty(true)
        .interact_text()
    {
        Ok(a) => a,
        Err(_) => return EXIT_ERROR,
    };
    let agent = (!agent.is_empty()).then_some(agent);
    cmd_search(mode, index, &query, agent, 20)
}

/// 交互式收集 open 参数:tid(来自 search 结果)。
fn prompt_open(mode: OutputMode, index: Option<&Path>) -> i32 {
    match Input::<i64>::with_theme(&ColorfulTheme::default())
        .with_prompt("Turn id (the #42 shown by search)")
        .interact_text()
    {
        Ok(tid) => cmd_open(mode, index, tid),
        Err(_) => EXIT_ERROR,
    }
}
