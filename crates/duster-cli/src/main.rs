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

use duster_core::scan::{ScanOptions, ScanReport, scan};
use duster_core::search::{SearchFilter, SearchHit, TurnDetail, open_turn, search};
use duster_core::status::{StatusReport, status};
use duster_model::CleanLevel;

mod interactive;
mod output;
use output::{
    EXIT_ERROR, EXIT_LOCKED, EXIT_OK, EXIT_PARTIAL, OutputMode, Table, accent, display_width,
    emit_json, emit_json_error, human_bytes, human_ms, is_json_mode, muted, ok_mark,
    truncate_width, warn_mark,
};

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
        /// Only search one agent, e.g. --agent claude-code
        #[arg(long)]
        agent: Option<String>,
        /// How many results to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print one whole conversation turn (the tid comes from `duster search`)
    Open {
        /// Turn id shown as `#42` in search results
        tid: i64,
    },
}

fn main() {
    let cli = Cli::parse();
    let mode = is_json_mode(cli.json);
    let index = cli.index.as_deref();
    let code = match cli.command {
        Some(Command::Scan { full }) => cmd_scan(mode, index, full),
        Some(Command::Status) => cmd_status(mode, index),
        Some(Command::Search {
            ref query,
            ref agent,
            limit,
        }) => cmd_search(mode, index, query, agent.clone(), limit),
        Some(Command::Open { tid }) => cmd_open(mode, index, tid),
        // 裸 `duster`:交互式启动菜单(TTY),否则打印帮助。
        None => interactive::run(mode, index),
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// 错误与退出码
// ---------------------------------------------------------------------------

/// 从错误链推断退出码。
///
/// 锁冲突的具体类型是 `duster_index::db::LockBusy`,但分层铁律禁止 CLI
/// import duster-index,downcast 不可达;退而匹配其 `Display` 的稳定文案
/// (「locked by another duster instance」),该文案由错误类型固定输出,
/// 视作跨层契约。匹配不上就落一般错误,不做更多特判。
fn exit_code_for(err: &anyhow::Error) -> i32 {
    if err
        .chain()
        .any(|e| e.to_string().contains("locked by another duster instance"))
    {
        EXIT_LOCKED
    } else {
        EXIT_ERROR
    }
}

/// 退出码对应的机器可读短码(进 JSON 信封的 error.code)。
fn error_code_name(code: i32) -> &'static str {
    match code {
        EXIT_LOCKED => "locked",
        EXIT_PARTIAL => "partial",
        _ => "error",
    }
}

/// 统一错误出口:人话链落 stderr;--json 时信封走 emit_json_error。
fn fail(mode: OutputMode, command: &str, err: &anyhow::Error) -> i32 {
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
    let mut headers: Vec<String> = vec!["AGENT".into(), "SIZE".into()];
    headers.extend(kind_headers());
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
        let mut row = vec![a.agent_id.clone(), human_bytes(a.bytes)];
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

    if !report.unclassified.is_empty() {
        let mut t = Table::new(vec!["FOLDER".to_string(), "SIZE".to_string()]);
        t.color_col(0, muted());
        t.right_align(&[1]);
        for u in &report.unclassified {
            t.push_row(vec![u.path.clone(), human_bytes(u.bytes)]);
        }
        println!();
        println!("  {}", Style::new().bold().apply_to("Not supported yet"));
        println!(
            "  {}",
            muted().apply_to("These folders look like agent data but have no adapter — size only, nothing is managed.")
        );
        println!("{}", t.render());
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

/// 体积列的资源类:条数不可行动,「占了 1.1 GB」才是决策依据。
/// 两列刻意分开——它们的可操作性完全相反:
/// `artifact` 是 clean 的全部作用域,`install` 是 clean 永不触碰的部分。
///
/// 列里给的是**占用量**;能拿回多少看摘要行,两者对 l0 并不相等。
const BYTE_KINDS: [(&str, &str); 2] = [("artifact", "CLEANABLE"), ("install", "INSTALLED")];

/// 资源类表头,顺序即列顺序。
fn kind_headers() -> Vec<String> {
    let mut h: Vec<String> = COUNT_KINDS.iter().map(|k| k.to_uppercase()).collect();
    h.extend(BYTE_KINDS.iter().map(|(_, label)| (*label).to_string()));
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
            muted().apply_to("installed software — clean never touches it")
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
    let mut headers: Vec<String> = vec!["AGENT".into(), "SIZE".into()];
    headers.extend(kind_headers());
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
        let mut row = vec![a.agent_id.clone(), human_bytes(a.bytes)];
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
    match delta_s {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} minutes ago", delta_s / 60),
        3600..86400 => format!("{} hours ago", delta_s / 3600),
        _ => format!("{} days ago", delta_s / 86400),
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

fn cmd_search(
    mode: OutputMode,
    index: Option<&Path>,
    query: &str,
    agent: Option<String>,
    limit: usize,
) -> i32 {
    let filter = SearchFilter { agent, limit };
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

fn cmd_open(mode: OutputMode, index: Option<&Path>, tid: i64) -> i32 {
    let detail: TurnDetail = match open_turn(index, tid) {
        Ok(d) => d,
        Err(e) => return fail(mode, "open", &e),
    };
    match mode {
        OutputMode::Json => emit_json("open", &detail, &[]),
        OutputMode::Human => {
            let dot = muted().apply_to("·").to_string();
            println!();
            println!(
                "  {} {dot} {} {dot} {}",
                accent().bold().apply_to(&detail.agent_id),
                format_args!("turn {}", detail.seq),
                style(&detail.role).magenta()
            );
            println!("  {}", muted().apply_to(&detail.resource_path));
            // 横线与路径行同宽(封顶 72 列),把元信息和正文隔开。
            let rule = display_width(&detail.resource_path).clamp(16, 72);
            println!("  {}", muted().apply_to("─".repeat(rule)));
            println!();
            println!("{}", detail.text);
        }
    }
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
