//! duster CLI 入口。命令结构为「名词 + 动词」,外壳零业务逻辑:
//! 解析参数 → 调 duster-core 用例 → 按 output.rs 契约渲染并映射退出码。

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use duster_core::scan::{ScanOptions, ScanReport, scan};
use duster_core::search::{SearchFilter, SearchHit, TurnDetail, open_turn, search};
use duster_core::status::{StatusReport, status};

mod output;
use output::{
    EXIT_ERROR, EXIT_LOCKED, EXIT_OK, EXIT_PARTIAL, OutputMode, Table, emit_json, emit_json_error,
    human_bytes, is_json_mode,
};

#[derive(Parser)]
#[command(name = "duster", version, about = "AI Agent 的资源管理器")]
struct Cli {
    /// 机器可读输出:stdout 一行紧凑 JSON 信封
    #[arg(long, global = true)]
    json: bool,
    /// 索引库路径(测试用;缺省 ~/.agent-duster/index.db)
    #[arg(long, global = true, hide = true, value_name = "PATH")]
    index: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 发现 agent 并建立索引(默认增量)
    Scan {
        /// 全量重扫:忽略增量指纹,所有会话重解析
        #[arg(long)]
        full: bool,
    },
    /// 总览:agent / 体积 / 各类资源计数 / 上次扫描
    Status,
    /// 全文检索会话正文(朴素子串,支持中文)
    Search {
        /// 要检索的子串(至少 3 个字符)
        query: String,
        /// 只看某个 agent
        #[arg(long)]
        agent: Option<String>,
        /// 最多返回条数
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 查看某条命中轮次的全文(tid 来自 search 结果)
    Open {
        /// 轮次 id
        tid: i64,
    },
}

fn main() {
    let cli = Cli::parse();
    let mode = is_json_mode(cli.json);
    let index = cli.index.as_deref();
    let code = match cli.command {
        Command::Scan { full } => cmd_scan(mode, index, full),
        Command::Status => cmd_status(mode, index),
        Command::Search {
            ref query,
            ref agent,
            limit,
        } => cmd_search(mode, index, query, agent.clone(), limit),
        Command::Open { tid } => cmd_open(mode, index, tid),
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
/// (「另一个 duster 实例占用」),该文案由错误类型固定输出,视作跨层契约。
/// 匹配不上就落一般错误,不做更多特判。
fn exit_code_for(err: &anyhow::Error) -> i32 {
    if err
        .chain()
        .any(|e| e.to_string().contains("另一个 duster 实例占用"))
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
        OutputMode::Human => eprintln!("错误: {err:#}"),
    }
    code
}

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

fn cmd_scan(mode: OutputMode, index: Option<&Path>, full: bool) -> i32 {
    if mode == OutputMode::Human {
        eprintln!("扫描中…(默认增量,--full 全量)");
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
    let mut headers = vec!["agent", "体积"];
    headers.extend(ALL_KINDS);
    headers.extend(["新解析", "warnings"]);
    let mut table = Table::new(headers);
    for a in &report.agents {
        if !a.installed {
            continue; // 未安装的 agent 无数据,人类模式不占版面(JSON 里仍完整)。
        }
        let mut row = vec![a.agent_id.clone(), human_bytes(a.bytes)];
        row.extend(ALL_KINDS.iter().map(|k| match a.kind_counts.get(*k) {
            Some(n) if *n > 0 => n.to_string(),
            _ => "-".to_string(),
        }));
        row.push(a.sessions_indexed.to_string());
        row.push(a.warnings.len().to_string());
        table.push_row(row);
    }
    println!("{}", table.render());

    if !report.unclassified.is_empty() {
        let mut t = Table::new(vec!["待适配目录", "体积"]);
        for u in &report.unclassified {
            t.push_row(vec![u.path.clone(), human_bytes(u.bytes)]);
        }
        println!();
        println!("{}", t.render());
        println!("(以上目录尚无适配器,仅统计体积,未纳入资源管理)");
    }

    println!();
    println!(
        "总计 {},耗时 {} ms",
        human_bytes(report.total_bytes),
        report.duration_ms
    );
    for w in warnings {
        eprintln!("警告: {w}");
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// resource 表里会出现的全部 kind,列顺序即表格列顺序。
const ALL_KINDS: [&str; 5] = ["mcp", "skill", "memory", "session", "artifact"];

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
    let mut header = vec!["agent", "体积"];
    header.extend(ALL_KINDS);
    header.push("上次扫描");
    let mut table = Table::new(header);
    for a in &report.agents {
        let mut row = vec![a.agent_id.clone(), human_bytes(a.bytes)];
        for kind in ALL_KINDS {
            row.push(
                a.kind_counts
                    .get(kind)
                    .map_or_else(|| "-".to_string(), u64::to_string),
            );
        }
        row.push(relative_time(a.last_scan_ms));
        table.push_row(row);
    }
    println!("{}", table.render());
    println!();
    println!("总计 {}", human_bytes(report.total_bytes));
}

/// Unix 毫秒 → 相对时间人话,如「3 分钟前」;None → 「从未」。
fn relative_time(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "从未".to_string();
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let delta_s = (now_ms - ms).max(0) / 1000;
    match delta_s {
        0..60 => "刚刚".to_string(),
        60..3600 => format!("{} 分钟前", delta_s / 60),
        3600..86400 => format!("{} 小时前", delta_s / 3600),
        _ => format!("{} 天前", delta_s / 86400),
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
        OutputMode::Human => render_search_human(&hits),
    }
    EXIT_OK
}

fn render_search_human(hits: &[SearchHit]) {
    if hits.is_empty() {
        eprintln!("无结果。提示:查询至少 3 个字符;可先 `duster scan` 更新索引。");
        return;
    }
    let color = use_color();
    for h in hits {
        let file = Path::new(&h.resource_path).file_name().map_or_else(
            || h.resource_path.clone(),
            |f| f.to_string_lossy().into_owned(),
        );
        println!(
            "#{}  {}  {}  seq={}  {}",
            h.tid, h.agent_id, file, h.seq, h.role
        );
        println!("    {}", highlight(&h.snippet, &h.highlights, color));
    }
}

/// 是否给命中着色:stdout 是 tty 且未设 NO_COLOR 环境变量。
fn use_color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
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
            println!("来源: {}", detail.resource_path);
            println!(
                "agent: {}  seq={}  role={}",
                detail.agent_id, detail.seq, detail.role
            );
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
        let err = anyhow::anyhow!("索引库正被另一个 duster 实例占用: database is locked");
        assert_eq!(exit_code_for(&err), EXIT_LOCKED);
        assert_eq!(exit_code_for(&anyhow::anyhow!("随便什么错")), EXIT_ERROR);
    }

    #[test]
    fn relative_time_分档() {
        assert_eq!(relative_time(None), "从未");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert_eq!(relative_time(Some(now_ms)), "刚刚");
        assert!(relative_time(Some(now_ms - 5 * 60 * 1000)).contains("分钟前"));
        assert!(relative_time(Some(now_ms - 3 * 3600 * 1000)).contains("小时前"));
        assert!(relative_time(Some(now_ms - 50 * 86400 * 1000)).contains("天前"));
    }
}
