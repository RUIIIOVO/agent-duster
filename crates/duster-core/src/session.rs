//! `duster session`：跨 agent 的会话浏览、导出与压缩存档。
//!
//! 会话是 duster 见过的最大一坨用户数据（本机 codex 一家就 352 MB + 243 MB
//! 归档），也是**最不可再生**的一坨——它是聊天记录，删了就没了。
//! 所以这里的每个动词都偏保守：`list` / `show` / `export` 只读，
//! `prune` 压缩存档而不是删除内容。
//!
//! # 与 M1 的 `duster prune` 共用一套实现
//!
//! `duster session prune` 不是第二套逻辑，是同一套的**纵向入口**：
//! 「最后使用时间」口径（轮次最大 ts，取不到视为最近用过）、确认交互、
//! 归档机制、往返校验后才删原件——全部复用 [`crate::prune`] 与
//! [`crate::plan`]。这里只多一件事：`--export-first` 联动导出。
//!
//! 写第二遍的代价不是多几百行，是两个入口的安全性慢慢分叉，
//! 而用户以为它们一样。
//!
//! # 回读一条轮次有三种源形态
//!
//! 索引里只有 `(byte_off, byte_len)`，正文一律回源取（`fts_turn` 是
//! contentless 的，库里根本没有正文）。三条路，见 [`crate::search::read_turn_body`]：
//!
//! 1. 普通文件：seek 到 `byte_off` 读 `byte_len` 字节；
//! 2. `.zst`：先整体解压再切片——偏移量描述的永远是**解压后**的逻辑内容，
//!    压缩对这一层完全透明；
//! 3. **`byte_len == 0`：`byte_off` 是源库里的行 id，不是文件偏移。**
//!    这是 SQLite 型会话（opencode / omp）全 workspace 共用的哨兵，
//!    走原生适配器的 `read_turn` 回库里查。真实轮次的正文长度永远大于 0，
//!    所以它不会和文件型会话撞车。
//!
//! 读回来的原始区间还要再按 agent 抽成对话正文（`read_turn_body` 内部做），
//! 否则印给用户的是原始 JSONL 行。
//!
//! 第 2 条不是可选的优化：少了它，`prune` 就成了变相删除。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_index::db::Index;
use duster_index::foreign;
use duster_index::query::{self, ResourceFilter, ResourceRecord, TurnRow};
use duster_model::TurnBody;

use crate::delete::{self, DeleteOptions, DeleteReport};
use crate::freshness;

/// 列表里的一行。
#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub rid: i64,
    pub agent_id: String,
    /// 会话自然键（文件名 / uuid）。
    pub key: String,
    /// 源文件路径；已压缩的指向 `.zst`。
    pub path: String,
    /// 会话所属的工作目录，从轮次里还原（Claude 的路径编码不可逆，
    /// 只能靠 jsonl 内的 `cwd`；Codex 的 `session_meta` 自带）。取不到为 None。
    pub cwd: Option<String>,
    pub bytes: u64,
    pub turns: u64,
    /// 最后一条轮次的时间（Unix 毫秒）。取不到为 None。
    pub last_turn_ms: Option<i64>,
    /// 内容是否已压缩存档。
    pub compressed: bool,
}

/// 过滤条件。全部为空 = 不过滤。
#[derive(Debug, Clone, Default)]
pub struct SessionFilter {
    /// 只看这几个 agent 的会话；空 Vec = 全部。
    pub agents: Vec<String>,
    /// 按 cwd 子串过滤（"这个项目的会话"）。
    pub project: Option<String>,
    /// 只看最后使用时间早于 N 天的。
    pub older_than_days: Option<u32>,
    /// 只看大于 N 字节的。
    pub min_bytes: Option<u64>,
    pub limit: usize,
    /// 当前时间（Unix 毫秒）注入口，测试用；None = 系统时钟。
    /// 只有 `older_than_days` 用得上它——「陈旧」是个相对时间的判断，
    /// 没有注入口就没法在不看真实时钟的前提下测这条过滤。
    pub now_ms: Option<i64>,
}

/// 一次会话的完整内容。
#[derive(Debug, Clone, Serialize)]
pub struct SessionDetail {
    pub row: SessionRow,
    pub turns: Vec<TurnView>,
}

/// 展示用的一条轮次。
#[derive(Debug, Clone, Serialize)]
pub struct TurnView {
    pub seq: i64,
    pub role: String,
    pub ts_ms: Option<i64>,
    /// 抽取后的对话正文(不是原始 JSONL 行)。
    pub text: String,
    /// 工具轮的工具名;行里带就给,不带是 None。
    pub tool: Option<String>,
    /// true = text 是原始行兜底(抽取失败,见 [`crate::search::read_turn_body`])。
    pub raw_fallback: bool,
}

/// 导出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// 人读：`## user` / `## assistant` 分节，带时间戳。
    Markdown,
    /// 机读：一个 JSON 对象，字段与 [`SessionDetail`] 一致。
    Json,
}

/// 取不到值时的占位词。整个模块只有这一个说法。
const UNKNOWN: &str = "unknown";

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// 列表结果：行 + 一路攒下的 warnings。
#[derive(Debug, Default)]
pub struct SessionList {
    pub rows: Vec<SessionRow>,
    /// 打不开的**源库**（opencode.db / history.db 这类）在这里明说，
    /// 不许静默跳过。列表照常展示能读的，退出码由调用方按它抬档。
    pub warnings: Vec<String>,
}

/// 列出会话。读索引 + `turn` 表聚合。
///
/// 唯一碰源文件的地方是 cwd：会话的工作目录只存在于**内容**里，索引没有这一列
/// （Claude 的目录名 `-Users-me-proj` 是不可逆编码，反推出来的路径有一半是错的，
/// 而这个字段会被用户拿去 `--project` 过滤）。所以每行会回读**第一条轮次**
/// 那一小段字节去找 `cwd`，找不到就留 None——**绝不猜**。
///
/// 库型会话（正文住在 agent 的 SQLite 库里，`byte_len == 0` 哨兵）没有 cwd 可
/// 还原，但它们的源库打不开是**环境事实**，列表不该静默带过——逐库只读探一次，
/// 打不开的进 [`SessionList::warnings`]（见 [`probe_db_sources`]）。
///
/// 顺序：最后使用时间倒序，时间相同再按 `(agent_id, key)`。同一份库跑两次
/// 逐字节一致，`--json` 的输出才能进 diff。
pub fn list(index_path: Option<&Path>, filter: &SessionFilter) -> Result<SessionList> {
    let idx = open_index(index_path)?;
    let conn = idx.conn();
    let now = filter.now_ms.unwrap_or_else(system_now_ms);

    let mut rows: Vec<SessionRow> = Vec::new();
    for rec in query::list_resources(
        conn,
        &ResourceFilter {
            agents: filter.agents.clone(),
            kinds: vec!["session".to_string()],
            clean_levels: Vec::new(),
        },
    )? {
        if filter.min_bytes.is_some_and(|min| rec.size < min) {
            continue;
        }
        let last_turn_ms = query::last_turn_ms(conn, rec.rid)?;
        // 陈旧口径只有一套：复用 prune 的 [`crate::plan::is_stale`]，
        // 没有时间戳一律视为最近用过。列表里给的数字必须和 prune 会动的
        // 那一批对得上，否则「看列表决定要不要 prune」这个动作就没意义。
        if filter
            .older_than_days
            .is_some_and(|days| !crate::plan::is_stale(last_turn_ms, days, now))
        {
            continue;
        }
        rows.push(SessionRow {
            rid: rec.rid,
            // cwd 留到便宜的过滤跑完再回填：能少读一个源文件是一个。
            cwd: None,
            bytes: rec.size,
            turns: query::turn_count(conn, rec.rid)?,
            last_turn_ms,
            compressed: duster_fs::zst::is_zst(Path::new(&rec.path)),
            agent_id: rec.agent_id,
            key: rec.key,
            path: rec.path,
        });
    }

    rows.sort_by(|a, b| {
        b.last_turn_ms
            .cmp(&a.last_turn_ms)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
            .then_with(|| a.key.cmp(&b.key))
    });

    // 库型会话的源库 → 归属 agent。一个库装几百场会话，按路径去重、探一次
    // 就够；BTreeMap 顺便让 warnings 的输出顺序可复现（排序稳定可 diff）。
    let mut db_sources: BTreeMap<String, String> = BTreeMap::new();
    for row in &mut rows {
        if let Some(first) = query::first_turn(conn, row.rid)? {
            if first.byte_len == 0 {
                // 库型会话：cwd 在库里的 session 行上，索引不存这一列，取不到；
                // 但源库读不读得动要探，探不动就进 warnings。
                db_sources
                    .entry(row.path.clone())
                    .or_insert_with(|| row.agent_id.clone());
            } else {
                row.cwd = recover_cwd(&row.path, &first);
            }
        }
    }
    let warnings = probe_db_sources(&db_sources);

    if let Some(project) = &filter.project {
        // cwd 取不到的会话在项目过滤下会消失：不知道它属于哪个项目，
        // 就不能声称它属于这一个。宁可漏，不可错报。
        rows.retain(|r| {
            r.cwd
                .as_deref()
                .is_some_and(|c| c.contains(project.as_str()))
        });
    }
    if filter.limit > 0 {
        rows.truncate(filter.limit);
    }
    Ok(SessionList { rows, warnings })
}

/// 库型会话的源库逐个只读探一次。
///
/// `foreign::open` 的口径是「打不开不是错误，是这一项读不到」（被独占、
/// 加密、损坏——`Ok(None)`/`Err` 都是），与 memory 那边同一个语义。能读的
/// 静默；打不开的进 warnings，格式 `<agent>: cannot read <path>: <err>`，
/// 好让用户知道「谁的会话读不出来了」。列表本身不因此失败——正文要等
/// `show` 才回读，这一趟的差事只是把环境事实摆出来。
fn probe_db_sources(sources: &BTreeMap<String, String>) -> Vec<String> {
    let mut warnings = Vec::new();
    for (path, agent) in sources {
        match foreign::open(Path::new(path)) {
            Ok(Some(_)) => {}
            Ok(None) => warnings.push(format!(
                "{agent}: cannot read {path}: not readable (locked, encrypted, or damaged)"
            )),
            Err(e) => warnings.push(format!("{agent}: cannot read {path}: {e:#}")),
        }
    }
    warnings
}

// ---------------------------------------------------------------------------
// show / export
// ---------------------------------------------------------------------------

/// 取一次会话的全文。
///
/// **必须能直接读压缩包**：`prune` 之后源文件是 `.zst`，
/// 读不出来就等于变相删除。回读走 [`crate::search::read_turn_body`]，
/// byte_off/byte_len 描述的是解压后的逻辑内容；正文还会按 agent 抽成散文
/// （原始区间是 JSONL 行，直接印给人看等于没做这个工具）。
pub fn show(index_path: Option<&Path>, rid: i64) -> Result<SessionDetail> {
    let idx = open_index(index_path)?;
    let conn = idx.conn();

    let rec = query::list_resources(
        conn,
        &ResourceFilter {
            agents: Vec::new(),
            kinds: vec!["session".to_string()],
            clean_levels: Vec::new(),
        },
    )?
    .into_iter()
    .find(|r| r.rid == rid)
    .with_context(|| {
        format!("session {rid} does not exist. Use `duster session list` to find a valid id.")
    })?;

    let turn_rows = query::list_turns(conn, rid)?;
    let mut turns = Vec::with_capacity(turn_rows.len());
    for t in &turn_rows {
        let body = read_turn_body(&rec.agent_id, &rec.path, t).with_context(|| {
            format!(
                "failed to read back turn {} of session {} from {} (the source was \
                 deleted, truncated, or rewritten outside duster; re-scanning cannot \
                 bring the text back — check that file)",
                t.seq, rec.key, rec.path
            )
        })?;
        turns.push(TurnView {
            seq: t.seq,
            role: t.role.clone(),
            ts_ms: t.ts_ms,
            text: body.text,
            tool: body.tool,
            raw_fallback: body.raw_fallback,
        });
    }

    // cwd 只存在于原始行 JSON 里,抽取后的正文是散文、挖不出来,所以这里
    // 必须单独回读原始区间(recover_cwd 内部就是读 raw span),不复用上面
    // 抽好的正文——宁可多读一次盘,也不能让 cwd 跟着正文一起变散文。
    let cwd = turn_rows
        .first()
        .filter(|t| t.byte_len != 0)
        .and_then(|t| recover_cwd(&rec.path, t));

    let row = SessionRow {
        rid: rec.rid,
        cwd,
        bytes: rec.size,
        turns: turn_rows.len() as u64,
        last_turn_ms: turn_rows.iter().filter_map(|t| t.ts_ms).max(),
        compressed: duster_fs::zst::is_zst(Path::new(&rec.path)),
        agent_id: rec.agent_id,
        key: rec.key,
        path: rec.path,
    };
    Ok(SessionDetail { row, turns })
}

/// 导出一次会话到字符串。
///
/// 两种格式都**确定性**：不遍历 HashMap，不看本地时区（时间一律 UTC，
/// 与 `duster_fs::zst::stamp` 同一套日历算术）。同一份 detail 导两次
/// 必须逐字节相同——导出文件会进 git、会被 diff。
pub fn export(detail: &SessionDetail, format: ExportFormat) -> Result<String> {
    match format {
        ExportFormat::Markdown => Ok(render_markdown(detail)),
        ExportFormat::Json => {
            serde_json::to_string_pretty(detail).context("failed to serialise session as JSON")
        }
    }
}

/// 导出并落盘，返回写下的字节数。
///
/// 正文一律来自 [`export`]，本函数一个字符都不改——导到 stdout 和导到文件
/// 必须逐字节相同，否则用户拿文件去 diff 会发现两条路径长得不一样。
///
/// 落盘走 `duster_fs::atomic::write_atomic`：半截导出比没有导出更糟，
/// 它看起来像一份完整记录。写文件属于 IO 层的活，外壳不碰盘
/// （CLI 不依赖 duster-fs，见 `duster-cli` 的 Cargo.toml 注释）。
pub fn export_to_file(detail: &SessionDetail, format: ExportFormat, out: &Path) -> Result<u64> {
    let body = export(detail, format)?;
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory: {}", parent.display()))?;
    }
    duster_fs::atomic::write_atomic(out, body.as_bytes())
        .with_context(|| format!("failed to write session export: {}", out.display()))?;
    Ok(body.len() as u64)
}

/// Markdown 形态：标题 + 一行元信息 + 按顺序的 `## <role>` 分节。
fn render_markdown(detail: &SessionDetail) -> String {
    let row = &detail.row;
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(&row.key);
    out.push_str("\n\n");
    out.push_str(&format!(
        "agent: {} | cwd: {} | turns: {} | last used: {}\n",
        row.agent_id,
        row.cwd.as_deref().unwrap_or(UNKNOWN),
        row.turns,
        row.last_turn_ms
            .map(render_date)
            .unwrap_or_else(|| UNKNOWN.to_string()),
    ));
    for turn in &detail.turns {
        out.push_str("\n## ");
        out.push_str(&turn.role);
        out.push_str(" (");
        out.push_str(
            &turn
                .ts_ms
                .map(render_time)
                .unwrap_or_else(|| UNKNOWN.to_string()),
        );
        out.push_str(")\n\n");
        out.push_str(turn.text.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// 回读
// ---------------------------------------------------------------------------

/// 回读并抽取一条轮次的可读正文。
///
/// **委托给 [`crate::search::read_turn_body`]**——那是全 workspace 唯一的
/// 回读入口。这里曾经有一份自己的实现，比 search 那份多认一种源形态
/// （SQLite 行 id），结果 `duster open` 碰到 opencode 的轮次就读出垃圾。
/// 两份实现只要有一份先长出新形态，另一份就是错的。
fn read_turn_body(agent_id: &str, path: &str, turn: &TurnRow) -> Result<TurnBody> {
    crate::search::read_turn_body(agent_id, path, turn.byte_off, turn.byte_len)
}

/// 回读第一条轮次去找 `cwd`，找不到再看它前面的头部区。任何一步失败都返回
/// None：列表少一列信息是遗憾，因为一个读不出来的会话让整条 `list` 报错是灾难。
fn recover_cwd(path: &str, first: &TurnRow) -> Option<String> {
    if first.byte_len == 0 {
        // 库里的会话：cwd 在 session 行上，不在轮次里，而索引不存这一列。
        return None;
    }
    // 走统一入口。这里 byte_len 必非 0（上面已挡掉库形态），走的是文件分支。
    let raw = crate::search::read_turn_text("", path, first.byte_off, first.byte_len).ok()?;
    if let Some(cwd) = cwd_from_turn_text(&raw) {
        return Some(cwd);
    }
    cwd_from_header(path, first.byte_off)
}

/// 头部区最多读这么多字节。
///
/// 正常文件的头部区就是一两行元信息，封顶挡的是被截断或被拼接过的病态文件：
/// 为了一个展示字段把几 MB 读进内存，这一列不值这个价。
const HEADER_SCAN_MAX: i64 = 64 * 1024;

/// 在第一条轮次**之前**的那几行里再找一次 `cwd`。
///
/// Codex 把 cwd 放在 `session_meta` 行上，而那一行不是轮次。只看第一条轮次的话，
/// 本机会话最多的那家 agent 每一场都还原不出工作目录，`--project` 会把它们整批
/// 漏掉——漏报比错报好，但两个都不是对的答案。
///
/// 只读 `[0, first_off)`，且用同一个回读入口（`.zst` 因此照样透明）。
/// 取第一条挖得出 cwd 的行：同一份文件跑两次结果逐字节一致。
/// 挖不出来仍然是 None——**绝不猜**这条规矩在这里没有例外。
fn cwd_from_header(path: &str, first_off: i64) -> Option<String> {
    let len = first_off.min(HEADER_SCAN_MAX);
    if len <= 0 {
        // 第一条轮次就在文件开头：没有头部区，也不能拿 len == 0 去调回读入口
        // （那是 SQLite 行 id 的哨兵值）。
        return None;
    }
    let head = crate::search::read_turn_text("", path, 0, len).ok()?;
    head.lines().find_map(cwd_from_turn_text)
}

/// 从一行会话 JSON 里挖 `cwd`。
///
/// 限深：会话行里真有 cwd 的都在浅层（Claude 顶层，Codex 的 `payload.cwd`），
/// 再往下挖只是在别人的 payload 里乱翻，捡到同名字段的风险大于收益。
fn cwd_from_turn_text(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    find_cwd(&value, 0)
}

const CWD_MAX_DEPTH: u32 = 3;

fn find_cwd(value: &serde_json::Value, depth: u32) -> Option<String> {
    let obj = value.as_object()?;
    if let Some(serde_json::Value::String(s)) = obj.get("cwd")
        && !s.is_empty()
    {
        return Some(s.clone());
    }
    if depth >= CWD_MAX_DEPTH {
        return None;
    }
    obj.values().find_map(|child| find_cwd(child, depth + 1))
}

// ---------------------------------------------------------------------------
// prune
// ---------------------------------------------------------------------------

/// `duster session prune` 的输入。字段刻意与
/// [`crate::prune::PruneOptions`] 对齐——它们是同一件事的两个入口。
#[derive(Debug, Clone)]
pub struct SessionPruneOptions {
    pub index_path: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// 只处理这几个 agent；空 Vec = 全部。
    pub agents: Vec<String>,
    pub older_than_days: u32,
    /// 压缩之前先把内容导出成 Markdown 到导出目录。
    /// 与压缩不冲突：压缩包是给 duster 读的，导出是给人读的。
    pub export_first: bool,
    pub export_dir: Option<PathBuf>,
    pub dry_run: bool,
    pub yes: bool,
    pub json: bool,
    pub now_ms: Option<i64>,
}

/// 压缩存档陈旧会话。**委托给 [`crate::prune`]**，本函数只做两件事：
/// 把 `SessionPruneOptions` 翻成 `PruneOptions`、在压缩前按需导出 Markdown。
/// 绝不复制粘贴一份归档/校验/确认逻辑。
pub fn prune(opts: &SessionPruneOptions) -> Result<crate::prune::PruneReport> {
    prune_filtered(opts, &crate::plan::PlanFilter::default())
}

/// 同 [`prune`]，但在出计划前并入一层 [`crate::plan::PlanFilter`]。
///
/// 「只动会话」是**结构性纵向边界**，用 `only_kind: Some("session")`
/// 表达，而不是先出一份计划、把非会话项的路径收成一份快照黑名单：
/// [`crate::prune::prune_filtered`] 会重算出 plan#2，两次计划之间新冒出来
/// 的非会话项——并发的 `duster scan` 提交了新的陈旧 skill、或某项恰好在
/// 两次 `SystemTime::now()` 之间跨过阈值——不在黑名单里，会被这个入口
/// 归档并删掉。黑名单盖不住没见过的新项，谓词盖得住：每一份重算出来的
/// 计划都自动受 `only_kind` 约束。
///
/// 调用方传入的 `skip` / `allow` / `only_kind` 全部**并入**（合并，不是
/// 替换）：用户勾选留下的会话只可能让名单变长，不可能把入口的越权边界
/// 洗掉。`only_kind` 若调用方已经给了且不是 `"session"`，那是用错了入口
/// ——直接报错而不是静静覆盖。
pub fn prune_filtered(
    opts: &SessionPruneOptions,
    filter: &crate::plan::PlanFilter,
) -> Result<crate::prune::PruneReport> {
    let home = resolve_home(opts.home.as_deref())?;
    let index_path = match &opts.index_path {
        Some(p) => p.clone(),
        None => crate::scan::default_index_path(&home),
    };

    // 先出一份只读计划，回答只有本入口知道答案的问题：合并后的过滤器会
    // 放行哪些会话（`--export-first` 要用）。plan_prune 的只读句柄开完即弃，
    // 后面 prune_filtered 会自己再开一次（写）句柄，两者不重叠。
    let plan = crate::plan::plan_prune(&crate::plan::PlanOptions {
        index_path: Some(index_path.clone()),
        home: Some(home.clone()),
        agents: opts.agents.clone(),
        older_than_days: Some(opts.older_than_days),
        keep_generations: false,
        now_ms: opts.now_ms,
    })?;

    // 本入口的纵向边界并进调用方的过滤器。`skip` / `allow` 原样并入，
    // 但要克隆一份：merged 要独立持有自己的名单，不借调用方 filter 的
    // 生命周期。
    //
    // `only_kind` 的过滤是整条剔除，**包括不可行动的 keep 行**（软件本体、
    // 最后一份 skill）：它们不会被动，但会跟着印进那份用户要逐行读的清单
    // ——`duster session prune` 印出十三行「qoder 装的软件不会被碰」，读者
    // 要先自己筛掉它们才能看见真正要过目的会话。计划清单是这个动词唯一的
    // 安全防线，稀释它就是削弱它。「软件本体有多大且不可清理」是
    // `duster prune` / `duster status` 的话题。
    let merged = crate::plan::PlanFilter {
        skip: filter.skip.clone(),
        allow: filter.allow.clone(),
        only_kind: match filter.only_kind {
            // 调用方没给（正常）或已经给了「session」：都是同一句话。
            None | Some("session") => Some("session"),
            Some(other) => {
                bail!(
                    "session prune only accepts the boundary kind \"session\", got {:?}: \
                     the vertical boundary is this entry's job, callers must not pick \
                     another kind",
                    other
                )
            }
        },
    };

    // 导出只针对**合并后的过滤器放行**、且真正会被压缩的那些（与
    // [`crate::plan::Plan::actionable`] 同一条谓词）：给一个不会被动的
    // 会话导一份 Markdown 是凭空写盘——用户勾掉的会话连压缩都轮不到，
    // 更不该凭空多出一份导出文件。直接拿合并后的过滤器问 plan#1，
    // 放行判定与执行路径共用同一个 [`crate::plan::PlanFilter::apply`]，
    // 不手搭第二套平行判定——两套判定迟早分叉，这里只留一套。
    let mut view = plan.clone();
    merged.apply(&mut view);
    let mut to_export: Vec<i64> = Vec::new();
    for item in view.items.iter() {
        if item.cleanable && item.action != crate::plan::Action::Keep {
            to_export.push(item.rid);
        }
    }
    drop(plan);

    // 导出必须发生在压缩**之前**。压完 `.zst` 照样读得出来，所以这不是
    // 数据安全问题，是承诺问题：用户要的顺序是"先给我一份纯文本，再动原件"。
    // 导出失败即中止，一个字节都不压——半份导出比没有导出更容易骗人。
    // dry-run 不导出：预览不写盘。
    if opts.export_first && !opts.dry_run && !to_export.is_empty() {
        let dir = opts
            .export_dir
            .clone()
            .unwrap_or_else(duster_fs::archive::default_export_dir);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create export directory: {}", dir.display()))?;
        for rid in to_export {
            let detail = show(Some(&index_path), rid)?;
            let body = export(&detail, ExportFormat::Markdown)?;
            let file = dir.join(export_file_name(&detail.row.agent_id, &detail.row.key));
            duster_fs::atomic::write_atomic(&file, body.as_bytes())
                .with_context(|| format!("failed to write session export: {}", file.display()))?;
        }
    }

    crate::prune::prune_filtered(
        &crate::prune::PruneOptions {
            index_path: Some(index_path),
            home: Some(home),
            agents: opts.agents.clone(),
            older_than_days: opts.older_than_days,
            keep_generations: false,
            export_dir: opts.export_dir.clone(),
            dry_run: opts.dry_run,
            yes: opts.yes,
            json: opts.json,
            now_ms: opts.now_ms,
        },
        &merged,
    )
}

/// 导出文件名：`<agent>-<key>.md`，非 `[A-Za-z0-9._-]` 一律换成 `_`。
///
/// key 是上游给的（会话文件名、uuid、库内 id），可能带 `/`，直接拼进路径
/// 就写到别的目录去了。换字符而不是报错：导出是尽量给用户留一份，
/// 不该因为一个名字里有斜杠就整批失败。
fn export_file_name(agent: &str, key: &str) -> String {
    let mut out = String::with_capacity(agent.len() + key.len() + 4);
    for ch in agent.chars().chain(std::iter::once('-')).chain(key.chars()) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out.push_str(".md");
    out
}

// ---------------------------------------------------------------------------
// rm
// ---------------------------------------------------------------------------

/// 一次 `session rm` 的目标。两种源形态（文件 / 库行）删除方式完全不同，
/// 但报告、归档、索引收尾是同一份。
struct RemoveTarget {
    rid: i64,
    /// 文件型会话（jsonl / zst）还是库型会话（源库里的行）。
    kind: SourceKind,
    /// 文件型：源文件路径；库型：源库路径。
    path: PathBuf,
    /// 库型的会话 id（opencode 的 `session.id` / omp 的 `history.session_id`）。
    key: String,
    agent: String,
}

/// 会话正文住在哪。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    /// 正文在文件里（jsonl / zst），删文件。
    File,
    /// 正文在 agent 的 SQLite 库里（`byte_len == 0` 哨兵），删库里那一行。
    Db,
}

/// `duster session rm`：归档后删除一场会话（文件型删 jsonl / zst，
/// 库型删源库里那一行），删完清索引。一次调用可以删多场（TUI 勾选批量），
/// 整个批次打一个归档包。
///
/// # 顺序铁律：先归档后删
///
/// 会话是聊天记录，删了就没了——所以删除之前必须有一条退路。`opts.archive`
/// 为 true（默认）时，先把本批次全部会话的**可读正文**导出成 Markdown、
/// 连同文件型会话的源文件一起打包进 `~/agent-duster-exports/`，打包成功
/// 才动手删；打包失败整体中止，一个字节都不删。`--no-archive` 是唯一的
/// 关闭方式，只该由脚本显式关上。
///
/// # 库型会话怎么删
///
/// 索引里 `byte_len == 0` 的轮次，`byte_off` 是源库里的行 id（见模块文档）。
/// 删除走 `duster_adapter::native::{opencode,omp}_session::delete_session`：
/// 写之前适配器先验表/列形状（schema_guard 的库形态），三条 DELETE 包在
/// 一个事务里（原子写）；这边还先留整文件快照（`~/.agent-duster/snapshots/`，
/// 与 uninstall 写别人家文件同一套规矩）——用户同意的是删一场会话，
/// 写坏了丢的是整库。库被 agent 锁住（busy_timeout 等不到）→ 这一条进
/// `warnings`，其余会话照删。
///
/// # 归档内容必须是可读的对话
///
/// 文件型会话的源文件（jsonl）本来就是对话，照归档；库型会话的"原文"是
/// 二进制库行，归档它等于归了一堆谁也读不了的东西。所以两类都先用
/// [`export_to_file`] 导出一份 Markdown（落点与 prune `--export-first`
/// 相同：`<home>/agent-duster-exports/<agent>-<key>.md`），归档里、
/// 导出目录里都有人读的副本。
///
/// # dry-run
///
/// 不导出、不归档、不删、不动索引；`removed` 列出"将删的路径"，`freed_bytes`
/// 是预估释放量。预览与真跑走同一段目标解析，预览说的就是真跑会做的。
pub fn remove(opts: &DeleteOptions, rids: &[i64]) -> Result<DeleteReport> {
    let home = resolve_home(opts.home.as_deref())?;
    let index_path = match &opts.index_path {
        Some(p) => p.clone(),
        None => crate::scan::default_index_path(&home),
    };
    // 写句柄：删完要清索引行（Contract 3：不留幽灵行）。
    let idx = Index::open(&index_path)
        .with_context(|| format!("failed to open index for writing: {}", index_path.display()))?;
    let conn = idx.conn();

    let mut report = DeleteReport::default();
    let mut targets: Vec<RemoveTarget> = Vec::new();
    let mut to_archive: Vec<PathBuf> = Vec::new();
    let export_dir = home.join("agent-duster-exports");

    // 第一遍：解析目标 + 导出归档内容。导出失败只作废那一条（warnings），
    // 其余照常——半份导出比没有导出更糟，但一条读不出来的会话不该连累
    // 一整批的删除。
    for &rid in rids {
        let rec = match lookup_session(&idx, rid) {
            Ok(Some(rec)) => rec,
            Ok(None) => {
                report.warnings.push(format!(
                    "session {rid} does not exist (already deleted?); skipped"
                ));
                continue;
            }
            Err(e) => {
                report
                    .warnings
                    .push(format!("session {rid}: {e:#}; skipped"));
                continue;
            }
        };
        let first = query::first_turn(conn, rid)?;
        let is_db = first.as_ref().is_some_and(|t| t.byte_len == 0);
        let path = PathBuf::from(&rec.path);
        let target = RemoveTarget {
            rid,
            kind: if is_db {
                SourceKind::Db
            } else {
                SourceKind::File
            },
            path: path.clone(),
            key: rec.key.clone(),
            agent: rec.agent_id.clone(),
        };
        if opts.archive && !opts.dry_run {
            match export_for_archive(&index_path, rid, &export_dir) {
                Ok(exported) => {
                    to_archive.push(exported);
                    if !is_db {
                        to_archive.push(path.clone());
                    }
                }
                Err(e) => {
                    report.warnings.push(format!(
                        "session {rid}: {e:#}; skipped (nothing was removed)"
                    ));
                    continue;
                }
            }
        }
        targets.push(target);
    }

    if targets.is_empty() {
        return Ok(report);
    }

    // 先归档后删：归档失败整体中止。库型会话也照常走到这里——
    // `to_archive` 里装的是导出的 Markdown，不是二进制库行。
    let archived = if opts.archive && !opts.dry_run {
        Some(delete::archive_before_delete(&to_archive, "session-rm", &home)?)
    } else {
        None
    };

    // 干跑：只报将删什么，一个字节都不动。
    if opts.dry_run {
        report.removed = targets.iter().map(|t| t.path.clone()).collect();
        report.freed_bytes = targets.iter().map(|t| t.removable_bytes()).sum();
        return Ok(report);
    }

    // 第二遍：真删。每条的失败只作废它自己——库锁住、文件被外力挪走，
    // 都不该让别的会话陪葬。
    let op_id = duster_fs::snapshot::new_op_id(SystemTime::now());
    let mut snapshotted_dbs: Vec<PathBuf> = Vec::new();
    for t in &targets {
        match delete_one(&home, &op_id, &mut snapshotted_dbs, t) {
            Ok(freed) => {
                report.removed.push(t.path.clone());
                report.freed_bytes += freed;
                match query::delete_resource(conn, t.rid) {
                    Ok(()) => {}
                    Err(e) => report.warnings.push(format!(
                        "session {} was deleted but the index entry could not be \
                         removed: {e:#} (a rescan will clean it up)",
                        t.rid
                    )),
                }
            }
            Err(e) => report
                .warnings
                .push(format!("session {}: {e:#}; kept", t.rid)),
        }
    }
    report.archived = archived;
    Ok(report)
}

impl RemoveTarget {
    /// 这场会话删掉后释放的磁盘字节。
    ///
    /// 文件型 = 源文件体积（目录/压缩包按实测）；库型 = 0——SQLite 删行
    /// 只把页送进 freelist，文件不缩，报"释放了 X"是拿用户当傻子。
    fn removable_bytes(&self) -> u64 {
        match self.kind {
            SourceKind::File => fs::metadata(&self.path)
                .map(|m| m.len())
                .unwrap_or(0),
            SourceKind::Db => 0,
        }
    }
}

/// 按 rid 找资源行。只认 `kind == "session"`：别的资源类型不该混进
/// `session rm` 的删除面。
fn lookup_session(idx: &Index, rid: i64) -> Result<Option<ResourceRecord>> {
    Ok(query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: Vec::new(),
            kinds: vec!["session".to_string()],
            clean_levels: Vec::new(),
        },
    )?
    .into_iter()
    .find(|r| r.rid == rid))
}

/// 把一场会话导出成 Markdown 落到导出目录，返回落盘路径。
///
/// 归档内容必须是**可读的对话**：文件型会话的原文（jsonl）虽然也是对话，
/// 但库型会话的"原文"是二进制库行，直接归档等于归了一堆谁也读不了的东西。
/// 所以一律先 [`export_to_file`] 导出——落点与 prune `--export-first`
/// 相同（`<home>/agent-duster-exports/<agent>-<key>.md`），归档里、导出
/// 目录里都留一份人读的副本。
fn export_for_archive(index_path: &Path, rid: i64, export_dir: &Path) -> Result<PathBuf> {
    let detail = show(Some(index_path), rid)?;
    let file = export_dir.join(export_file_name(&detail.row.agent_id, &detail.row.key));
    export_to_file(&detail, ExportFormat::Markdown, &file)?;
    Ok(file)
}

/// 真删一个目标。
///
/// - **文件型**：删源文件。文件已经被外力挪走 → 视为已删（索引行照清）。
/// - **库型**：先留整文件快照（同批次同库只快照一次），再走适配器的
///   `delete_session`（schema_guard + 事务都在那边）。库里已经没有这一场
///   （agent 自己删过）→ 返回 Ok(0)，索引行照清。
fn delete_one(
    home: &Path,
    op_id: &str,
    snapshotted_dbs: &mut Vec<PathBuf>,
    t: &RemoveTarget,
) -> Result<u64> {
    match t.kind {
        SourceKind::File => {
            let size = t.removable_bytes();
            match fs::remove_file(&t.path) {
                Ok(()) => Ok(size),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
                Err(e) => Err(e).with_context(|| {
                    format!("failed to remove session file: {}", t.path.display())
                }),
            }
        }
        SourceKind::Db => {
            // 写别人家的库之前先留整文件快照（与 uninstall 写共享配置同一套
            // 规矩）：用户同意的是删一场会话，写坏了丢的是整库。快照失败即
            // 拒绝动手——写不出退路就不写。
            if !snapshotted_dbs.iter().any(|p| p == &t.path) {
                let root = home.join(".agent-duster").join("snapshots");
                duster_fs::snapshot::snapshot_file(&root, op_id, &t.path, home).with_context(
                    || {
                        format!(
                            "refusing to delete a session from {} without a snapshot",
                            t.path.display()
                        )
                    },
                )?;
                // WAL 帧不在主文件里：一并快照（尽力而为，读不到就算了——
                // 快照是防 duster 自己写坏文件结构的兜底，不是完整备份）。
                let wal = PathBuf::from(format!("{}.wal", t.path.display()));
                if wal.is_file() {
                    let _ = duster_fs::snapshot::snapshot_file(&root, op_id, &wal, home);
                }
                snapshotted_dbs.push(t.path.clone());
            }
            let deleted = match t.agent.as_str() {
                "opencode" => {
                    duster_adapter::native::opencode_session::delete_session(&t.path, &t.key)?
                }
                "omp" => duster_adapter::native::omp_session::delete_session(&t.path, &t.key)?,
                other => {
                    bail!(
                        "sessions of agent `{other}` live in a database duster cannot write"
                    )
                }
            };
            // 0 行 = 库里本来就没有这一场（agent 自己删过）；不是失败，索引
            // 行由调用方照清。释放字节按 0 计（见 [`RemoveTarget::removable_bytes`]）。
            let _ = deleted;
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------------------
// 共用小工具
// ---------------------------------------------------------------------------

/// 解析索引库路径并只读打开；库还没建过就先建一次，与 [`crate::search`]
/// 共用 [`crate::freshness::ensure_exists`]——「库在不在、谁负责让它在」
/// 整个程序里只能有一种说法。
fn open_index(index_path: Option<&Path>) -> Result<Index> {
    let path = freshness::ensure_exists(index_path)?;
    Index::open_readonly(&path)
        .with_context(|| format!("failed to open index read-only: {}", path.display()))
}

/// 确定 home：优先注入值，否则真实用户主目录。
/// pub(crate)：`session_migrate` 的目标端落盘也要定位 home，同一个判据只有一份。
pub(crate) fn resolve_home(injected: Option<&Path>) -> Result<PathBuf> {
    if let Some(h) = injected {
        return Ok(h.to_path_buf());
    }
    let h = duster_fs::path::expand_tilde("~");
    if h == Path::new("~") {
        bail!("cannot determine home directory (HOME is not set)");
    }
    Ok(h)
}

pub(crate) fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unix 毫秒 -> UTC `YYYYMMDD-HHMMSS`。日历算术复用 `duster_fs::zst::stamp`，
/// 不在这里再抄一份闰年规则，也不引时区依赖——导出文件跨机器可比。
/// pub(crate)：`session_migrate` 的时间戳/文件名同源于这一份日历算术。
pub(crate) fn stamp_of(ms: i64) -> String {
    let secs = ms.div_euclid(1_000).max(0) as u64;
    duster_fs::zst::stamp(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Unix 毫秒 -> `YYYY-MM-DD`（UTC）。
pub(crate) fn render_date(ms: i64) -> String {
    let s = stamp_of(ms);
    format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
}

/// Unix 毫秒 -> `YYYY-MM-DDTHH:MM:SSZ`（UTC）。
fn render_time(ms: i64) -> String {
    let s = stamp_of(ms);
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        &s[0..4],
        &s[4..6],
        &s[6..8],
        &s[9..11],
        &s[11..13],
        &s[13..15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use duster_index::upsert::{self, ResourceRow};
    use duster_model::{AgentInfo, Role, TurnRecord};
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    /// 固定的"现在"，2025-10-09。所有陈旧判定都相对它算，不看真实时钟。
    const NOW_MS: i64 = 1_760_000_000_000;
    /// 固定的"很久以前"，2020-09-13，距 NOW_MS 约 1852 天。
    const OLD_MS: i64 = 1_600_000_000_000;

    /// Claude 风格：每行一个完整 JSON 对象，第一行带 `cwd`。
    const L1: &str = r#"{"cwd":"/tmp/proj-a","role":"user","text":"第一句"}"#;
    const L2: &str = r#"{"role":"assistant","text":"第二句 hello"}"#;

    fn body(pad: usize) -> String {
        let mut s = format!("{L1}\n{L2}\n");
        if pad > 0 {
            s.push_str(&"x".repeat(pad));
            s.push('\n');
        }
        s
    }

    /// 造一场文件型会话：写源文件 + 一行资源 + 两条轮次。返回 (rid, 源路径)。
    fn add_session(
        idx: &Index,
        home: &Path,
        agent: &str,
        key: &str,
        ts_ms: i64,
        pad: usize,
    ) -> (i64, PathBuf) {
        let path = home.join(format!(".{agent}/projects/{key}"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text = body(pad);
        std::fs::write(&path, &text).unwrap();

        let out = upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: agent.into(),
                kind: "session".into(),
                scope: "user".into(),
                key: key.into(),
                path: path.to_string_lossy().into_owned(),
                size: text.len() as u64,
                mtime_ns: ts_ms * 1_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap();
        upsert::replace_turns(
            idx.conn(),
            out.rid,
            &[
                TurnRecord {
                    seq: 0,
                    role: Role::User,
                    ts_ms: Some(ts_ms),
                    byte_off: 0,
                    byte_len: L1.len() as u64,
                    text: L1.to_string(),
                },
                TurnRecord {
                    seq: 1,
                    role: Role::Assistant,
                    ts_ms: Some(ts_ms + 1_000),
                    byte_off: (L1.len() + 1) as u64,
                    byte_len: L2.len() as u64,
                    text: L2.to_string(),
                },
            ],
        )
        .unwrap();
        (out.rid, path)
    }

    /// 假 home + 空索引库。真实 `$HOME` 一个字节都不碰。
    fn seed_home() -> (TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".agent-duster/index.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let idx = Index::open(&db).unwrap();
        upsert::upsert_agent(
            idx.conn(),
            &AgentInfo {
                id: "claude".into(),
                display_name: "Claude Code".into(),
                root: home.path().join(".claude"),
                version: None,
            },
            OLD_MS,
        )
        .unwrap();
        drop(idx);
        (home, db)
    }

    fn filter(now: i64) -> SessionFilter {
        SessionFilter {
            now_ms: Some(now),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------

    #[test]
    fn list_过滤与聚合都来自_turn_表() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        add_session(&idx, h, "claude", "old.jsonl", OLD_MS, 0);
        add_session(&idx, h, "codex", "new.jsonl", NOW_MS - 60_000, 100);
        drop(idx);

        let all = list(Some(&db), &filter(NOW_MS)).unwrap().rows;
        assert_eq!(all.len(), 2);
        // 最近用过的排前面。
        assert_eq!(all[0].agent_id, "codex");

        let old = all.iter().find(|r| r.agent_id == "claude").unwrap();
        assert_eq!(old.turns, 2, "turns 必须来自 turn 表，不是猜的");
        assert_eq!(old.last_turn_ms, Some(OLD_MS + 1_000));
        assert_eq!(old.cwd.as_deref(), Some("/tmp/proj-a"));
        assert!(!old.compressed);
        assert_eq!(old.bytes, body(0).len() as u64);

        let by_agent = list(
            Some(&db),
            &SessionFilter {
                agents: vec!["codex".into()],
                ..filter(NOW_MS)
            },
        )
        .unwrap().rows;
        assert_eq!(by_agent.len(), 1);
        assert_eq!(by_agent[0].key, "new.jsonl");

        let stale = list(
            Some(&db),
            &SessionFilter {
                older_than_days: Some(30),
                ..filter(NOW_MS)
            },
        )
        .unwrap().rows;
        assert_eq!(stale.len(), 1, "只有 2020 那场超过 30 天");
        assert_eq!(stale[0].key, "old.jsonl");

        let big = list(
            Some(&db),
            &SessionFilter {
                min_bytes: Some(body(0).len() as u64 + 1),
                ..filter(NOW_MS)
            },
        )
        .unwrap().rows;
        assert_eq!(big.len(), 1);
        assert_eq!(big[0].key, "new.jsonl");

        let one = list(
            Some(&db),
            &SessionFilter {
                limit: 1,
                ..filter(NOW_MS)
            },
        )
        .unwrap().rows;
        assert_eq!(one.len(), 1);
    }

    /// 库型会话的源库打不开时:不 panic、不静默跳过——行照常展示,
    /// `<agent>: cannot read <path>: …` 进 warnings,调用方据此抬退出码。
    #[test]
    fn list_源库打不开时_warning_且行照常展示() {
        let (home, db) = seed_home();
        let h = home.path();
        // 文件头是 SQLite 魔数、内容是垃圾:is_sqlite 认它,open 读不出来。
        let db_file = h.join(".opencode/opencode.db");
        std::fs::create_dir_all(db_file.parent().unwrap()).unwrap();
        let mut bytes = b"SQLite format 3\0".to_vec();
        bytes.extend(std::iter::repeat_n(0xCDu8, 4096));
        std::fs::write(&db_file, &bytes).unwrap();

        let idx = Index::open(&db).unwrap();
        let out = upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "opencode".into(),
                kind: "session".into(),
                scope: "user".into(),
                key: "s1".into(),
                path: db_file.to_string_lossy().into_owned(),
                size: bytes.len() as u64,
                mtime_ns: NOW_MS * 1_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap();
        // byte_len == 0 哨兵:byte_off 是源库里的行 id,不是文件偏移。
        upsert::replace_turns(
            idx.conn(),
            out.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: Some(NOW_MS),
                byte_off: 7,
                byte_len: 0,
                text: String::new(),
            }],
        )
        .unwrap();
        drop(idx);

        let list = list(Some(&db), &filter(NOW_MS)).unwrap();
        assert_eq!(list.rows.len(), 1, "会话照常展示");
        assert_eq!(list.rows[0].cwd, None, "库型会话没有 cwd 可还原");
        assert_eq!(list.warnings.len(), 1, "{:?}", list.warnings);
        let w = &list.warnings[0];
        assert!(w.starts_with("opencode: cannot read "), "{w}");
        assert!(w.contains(&db_file.display().to_string()), "{w}");
    }

    /// prune 把会话压成 `.zst` 之后必须照常读得出全文——否则
    /// 「压缩存档、不删除内容」是空话，prune 等于变相删除。
    #[test]
    fn show_在会话被压缩前后返回同一份正文() {
        let (home, db) = seed_home();
        let idx = Index::open(&db).unwrap();
        let (rid, src) = add_session(&idx, home.path(), "claude", "old.jsonl", OLD_MS, 0);
        drop(idx);

        let before = show(Some(&db), rid).unwrap();
        assert_eq!(before.turns.len(), 2);
        assert_eq!(before.turns[0].text, L1);
        assert_eq!(before.turns[1].text, L2);
        assert!(!before.row.compressed);

        let oc = crate::prune::compress_session(&src).unwrap();
        let zst = oc.replaced_by.unwrap();
        assert!(!src.exists(), "往返校验通过后原件应已删除");
        let idx = Index::open(&db).unwrap();
        query::update_resource_path(idx.conn(), rid, &zst).unwrap();
        drop(idx);

        let after = show(Some(&db), rid).unwrap();
        assert!(after.row.compressed);
        let before_text: Vec<&str> = before.turns.iter().map(|t| t.text.as_str()).collect();
        let after_text: Vec<&str> = after.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(before_text, after_text, "压缩不许改变一个字节");
        assert_eq!(after.row.cwd, before.row.cwd);
    }

    /// `byte_len == 0` 的轮次必须走「按行 id 回源库查」，而不是 seek 文件。
    #[test]
    fn show_按行_id_回源库读_sqlite_型会话() {
        let (home, db) = seed_home();
        let src = home.path().join("opencode.db");
        let ids = duster_adapter::native::opencode_session::fixture_db(
            &src,
            "ses_1",
            &[("user", "库里的第一句"), ("assistant", "库里的第二句")],
        )
        .unwrap();
        assert_eq!(ids.len(), 2);

        let idx = Index::open(&db).unwrap();
        let out = upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "opencode".into(),
                kind: "session".into(),
                scope: "user".into(),
                key: "ses_1".into(),
                path: src.to_string_lossy().into_owned(),
                size: 4096,
                mtime_ns: OLD_MS * 1_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap();
        upsert::replace_turns(
            idx.conn(),
            out.rid,
            &[
                TurnRecord {
                    seq: 0,
                    role: Role::User,
                    ts_ms: Some(OLD_MS),
                    byte_off: ids[0] as u64,
                    byte_len: 0,
                    text: "库里的第一句".into(),
                },
                TurnRecord {
                    seq: 1,
                    role: Role::Assistant,
                    ts_ms: Some(OLD_MS + 1_000),
                    byte_off: ids[1] as u64,
                    byte_len: 0,
                    text: "库里的第二句".into(),
                },
            ],
        )
        .unwrap();
        drop(idx);

        let detail = show(Some(&db), out.rid).unwrap();
        assert_eq!(detail.turns[0].text, "库里的第一句");
        assert_eq!(detail.turns[1].text, "库里的第二句");
        // 库里的会话没有可回读的 cwd，只能是 None，不许编。
        assert_eq!(detail.row.cwd, None);
    }

    #[test]
    fn export_markdown_逐字节可复现且两个角色有序() {
        let (home, db) = seed_home();
        let idx = Index::open(&db).unwrap();
        let (rid, _) = add_session(&idx, home.path(), "claude", "old.jsonl", OLD_MS, 0);
        drop(idx);
        let detail = show(Some(&db), rid).unwrap();

        let a = export(&detail, ExportFormat::Markdown).unwrap();
        let b = export(&detail, ExportFormat::Markdown).unwrap();
        assert_eq!(a, b, "同一份 detail 导两次必须一致");

        assert!(a.starts_with("# old.jsonl\n"), "{a}");
        assert!(a.contains("agent: claude | cwd: /tmp/proj-a | turns: 2 | last used: 2020-09-13"));
        let user = a.find("## user").expect("缺 user 分节");
        let assistant = a.find("## assistant").expect("缺 assistant 分节");
        assert!(user < assistant, "分节必须按轮次顺序");
        assert!(a.contains("## user (2020-09-13T12:26:40Z)"), "{a}");
        assert!(a.contains(L1) && a.contains(L2));

        let json = export(&detail, ExportFormat::Json).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["row"]["key"], "old.jsonl");
        assert_eq!(back["row"]["turns"], 2);
        assert_eq!(back["turns"].as_array().unwrap().len(), 2);
        assert_eq!(back["turns"][1]["role"], "assistant");
        assert_eq!(back["turns"][0]["text"], L1);
    }

    #[test]
    fn prune_压缩同龄的未标星会话() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (_, plain) = add_session(&idx, h, "claude", "plain.jsonl", OLD_MS, 0);
        drop(idx);

        let report = prune(&prune_opts(&db, h)).unwrap();

        assert!(report.executed);
        assert!(!plain.exists(), "同龄会话应已压缩并删原件");
        assert!(zst_of(&plain).is_file());
    }

    fn prune_opts(db: &Path, home: &Path) -> SessionPruneOptions {
        SessionPruneOptions {
            index_path: Some(db.to_path_buf()),
            home: Some(home.to_path_buf()),
            agents: Vec::new(),
            older_than_days: 30,
            export_first: false,
            export_dir: Some(home.join("exports")),
            dry_run: false,
            yes: true,
            json: false,
            now_ms: Some(NOW_MS),
        }
    }

    fn zst_of(p: &Path) -> PathBuf {
        PathBuf::from(format!("{}.zst", p.display()))
    }

    #[test]
    fn prune_export_first_在压缩前落一份_markdown() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (_, src) = add_session(&idx, h, "claude", "e.jsonl", OLD_MS, 0);
        drop(idx);

        let exports = h.join("exports");
        prune(&SessionPruneOptions {
            export_first: true,
            export_dir: Some(exports.clone()),
            ..prune_opts(&db, h)
        })
        .unwrap();

        let md = exports.join("claude-e.jsonl.md");
        assert!(md.is_file(), "导出文件缺失：{}", md.display());
        let body = std::fs::read_to_string(&md).unwrap();
        assert!(body.contains("第一句"), "导出里必须有轮次正文：{body}");
        assert!(body.contains("## assistant"));
        // 导出发生在压缩之前，压缩照常进行。
        assert!(!src.exists());
        assert!(zst_of(&src).is_file());
    }

    #[test]
    fn prune_dry_run_不导出也不压缩() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (_, src) = add_session(&idx, h, "claude", "d.jsonl", OLD_MS, 0);
        drop(idx);

        let exports = h.join("exports");
        let report = prune(&SessionPruneOptions {
            export_first: true,
            export_dir: Some(exports.clone()),
            dry_run: true,
            ..prune_opts(&db, h)
        })
        .unwrap();

        assert!(!report.executed);
        assert!(src.is_file(), "预览不许动源文件");
        assert!(!exports.exists(), "预览不许写盘");
        assert_eq!(report.plan.items.len(), 1);
    }

    /// 导到文件与导到字符串必须是同一份字节，字节数也要如实回报——
    /// 外壳把这个数字印给用户当回执。
    #[test]
    fn export_to_file_与_export_逐字节一致且回报字节数() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (rid, _) = add_session(&idx, h, "claude", "x.jsonl", OLD_MS, 0);
        drop(idx);
        let detail = show(Some(&db), rid).unwrap();

        // 落点的父目录不存在也要能写出来：`--out` 是用户手打的路径。
        let out = h.join("out/deep/x.json");
        let n = export_to_file(&detail, ExportFormat::Json, &out).unwrap();
        let on_disk = std::fs::read_to_string(&out).unwrap();
        assert_eq!(on_disk, export(&detail, ExportFormat::Json).unwrap());
        assert_eq!(n, on_disk.len() as u64);

        let md = h.join("out/x.md");
        export_to_file(&detail, ExportFormat::Markdown, &md).unwrap();
        assert_eq!(
            std::fs::read_to_string(&md).unwrap(),
            export(&detail, ExportFormat::Markdown).unwrap()
        );
    }

    /// 会话入口的计划里只能有会话。软件本体那几行永远不会被动，但跟着印出来
    /// 就是在稀释用户唯一要逐行读的那份清单。
    #[test]
    fn prune_的计划里只有会话() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        add_session(&idx, h, "claude", "p.jsonl", OLD_MS, 0);

        let installed = h.join(".claude/plugins/payload.bin");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, vec![b'p'; 1024]).unwrap();
        upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "claude".into(),
                kind: "install".into(),
                scope: "user".into(),
                key: "plugins".into(),
                path: installed.to_string_lossy().into_owned(),
                size: 1024,
                mtime_ns: OLD_MS * 1_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: Some(1024),
                mapper: None,
            },
        )
        .unwrap();
        drop(idx);

        let report = prune(&SessionPruneOptions {
            dry_run: true,
            yes: false,
            ..prune_opts(&db, h)
        })
        .unwrap();
        assert!(!report.plan.items.is_empty(), "夹具里有一场陈旧会话");
        let kinds: BTreeSet<&str> = report.plan.items.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, BTreeSet::from(["session"]), "{kinds:?}");
        assert!(installed.is_file());
    }

    /// 用户勾掉的会话连压缩都轮不到，更不该凭空导出一份 Markdown：
    /// `--export-first` 只导出**合并后过滤器放行**的那些会话。
    #[test]
    fn prune_filtered_被allow挡掉的会话不导出markdown() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (_, picked) = add_session(&idx, h, "claude", "picked.jsonl", OLD_MS, 0);
        let (_, dropped) = add_session(&idx, h, "claude", "dropped.jsonl", OLD_MS, 0);
        drop(idx);

        let exports = h.join("exports");
        prune_filtered(
            &SessionPruneOptions {
                export_first: true,
                export_dir: Some(exports.clone()),
                ..prune_opts(&db, h)
            },
            &crate::plan::PlanFilter::allow_only(vec![(
                picked.clone(),
                crate::plan::Action::CompressFile,
            )]),
        )
        .unwrap();

        let md_files: Vec<_> = std::fs::read_dir(&exports)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".md"))
            .collect();
        assert_eq!(md_files.len(), 1, "导出目录里只能有一份 .md：{md_files:?}");
        assert!(!picked.exists(), "勾过的那场照常压缩并删原件");
        assert!(zst_of(&picked).is_file());
        assert!(dropped.is_file(), "没勾的那场原文件分毫不动");
        assert!(!zst_of(&dropped).exists(), "没勾的那场连压缩都轮不到");
        assert_eq!(
            std::fs::read(&dropped).unwrap(),
            body(0).as_bytes(),
            "没勾的那场原文件逐字节不变"
        );
    }

    /// 调用方传入的 `skip` 必须真的生效：本入口曾只给名单预留容量、
    /// 一条都没拷，调用方挡掉的会话照样被压缩。并入是并集，不是换一张
    /// 白纸——用户勾选留下的会话只可能让名单变长，不可能把入口的边界
    /// 洗掉，反过来也一样。
    #[test]
    fn prune_filtered_调用方的_skip_不会被丢掉() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (_, src) = add_session(&idx, h, "claude", "skipped.jsonl", OLD_MS, 0);
        drop(idx);

        let report = prune_filtered(
            &prune_opts(&db, h),
            &crate::plan::PlanFilter::skipping(vec![src.clone()]),
        )
        .unwrap();

        assert!(report.executed);
        assert!(src.is_file(), "调用方 skip 的会话不许被压缩");
        assert!(!zst_of(&src).exists(), "连压缩都轮不到");
        assert!(
            report.plan.items.iter().all(|i| i.path != src),
            "计划里也不该留着它——否则 --json 的读者会以为它被动过"
        );
    }

    /// 纵向边界必须是谓词而不是快照黑名单：同龄的陈旧 skill 与会话各一份，
    /// 会话照常压缩，skill 分毫不动。快照方案下，两份计划之间新冒出来的
    /// 非会话项会漏网；`only_kind` 让每一份重算出来的计划都自动受约束。
    #[test]
    fn prune_filtered_只动会话_靠的是谓词不是快照() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();

        // 同龄的陈旧 skill：与那场会话同一天"最后使用"，prune 视角下
        // 同样该清。从 `duster session prune` 嘴里删掉一个陈旧 skill
        // 是越权——它就是那条必须被谓词挡住的非会话项。
        let skill = h.join(".claude/skills/my-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "x").unwrap();
        upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "claude".into(),
                kind: "skill".into(),
                scope: "user".into(),
                key: "my-skill".into(),
                path: skill.to_string_lossy().into_owned(),
                size: 1,
                mtime_ns: OLD_MS * 1_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap();
        // 点亮调用证据：让「查过、确实没有调用」成为可判定的陈旧，
        // 而不是整组冻结。
        duster_index::meta::set(idx.conn(), duster_index::meta::SKILL_EVIDENCE_READY, "1")
            .unwrap();

        let (_, sess) = add_session(&idx, h, "claude", "p.jsonl", OLD_MS, 0);
        drop(idx);

        let report = prune(&SessionPruneOptions {
            ..prune_opts(&db, h)
        })
        .unwrap();

        assert!(report.executed);
        assert!(skill.is_dir(), "skill 分毫不动");
        assert!(!sess.exists(), "会话照常压缩");
        assert!(zst_of(&sess).is_file());
        let kinds: BTreeSet<&str> = report.plan.items.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, BTreeSet::from(["session"]), "{kinds:?}");
    }

    /// codex 形态：cwd 在 `session_meta` 行上，而那一行不是轮次。只看第一条
    /// 轮次就还原不出工作目录，`--project` 会把整家 agent 漏掉。
    #[test]
    fn list_从头部区还原_codex_的_cwd() {
        const META: &str =
            r#"{"type":"session_meta","payload":{"id":"a","cwd":"/tmp/codex-proj"}}"#;
        const TURN: &str = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#;

        /// 造一场「头部区 + 一条轮次」的会话，返回还原出来的 cwd。
        /// `header` 为空表示第一条轮次就在文件开头。
        fn cwd_of(idx: &Index, home: &Path, key: &str, header: &str) -> Option<String> {
            let path = home.join(format!(".codex/sessions/{key}"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let head = if header.is_empty() {
                String::new()
            } else {
                format!("{header}\n")
            };
            let text = format!("{head}{TURN}\n");
            std::fs::write(&path, &text).unwrap();

            let out = upsert::upsert_resource(
                idx.conn(),
                &ResourceRow {
                    agent_id: "codex".into(),
                    kind: "session".into(),
                    scope: "user".into(),
                    key: key.into(),
                    path: path.to_string_lossy().into_owned(),
                    size: text.len() as u64,
                    mtime_ns: OLD_MS * 1_000_000,
                    hash_content: None,
                    cheap_print: None,
                    clean_level: None,
                    reclaimable: None,
                    install_bytes: None,
                    mapper: None,
                },
            )
            .unwrap();
            upsert::replace_turns(
                idx.conn(),
                out.rid,
                &[TurnRecord {
                    seq: 0,
                    role: Role::User,
                    ts_ms: Some(OLD_MS),
                    byte_off: head.len() as u64,
                    byte_len: TURN.len() as u64,
                    text: TURN.to_string(),
                }],
            )
            .unwrap();

            list(
                Some(&home.join(".agent-duster/index.db")),
                &SessionFilter {
                    agents: vec!["codex".into()],
                    ..filter(NOW_MS)
                },
            )
            .unwrap()
            .rows
            .into_iter()
            .find(|r| r.rid == out.rid)
            .unwrap()
            .cwd
        }

        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        assert_eq!(
            cwd_of(&idx, h, "with-meta.jsonl", META).as_deref(),
            Some("/tmp/codex-proj")
        );
        // 没有头部区、轮次里也没有 cwd：仍然是"不知道"，不许编一个出来。
        assert_eq!(cwd_of(&idx, h, "headless.jsonl", "").as_deref(), None);
        drop(idx);
    }

    // -----------------------------------------------------------------------
    // rm
    // -----------------------------------------------------------------------

    fn rm_opts(db: &Path, home: &Path, archive: bool, dry_run: bool) -> DeleteOptions {
        DeleteOptions {
            index_path: Some(db.to_path_buf()),
            home: Some(home.to_path_buf()),
            archive,
            dry_run,
        }
    }

    /// 文件型会话：归档后源文件没了、归档包里是可读对话（markdown）+ 源文件、
    /// 索引无幽灵行。
    #[test]
    fn rm_文件型会话归档后删除_归档里是可读对话() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (rid, src) = add_session(&idx, h, "claude", "gone.jsonl", OLD_MS, 0);
        drop(idx);

        let report = remove(&rm_opts(&db, h, true, false), &[rid]).unwrap();

        assert!(!src.exists(), "源文件应已删除");
        assert!(report.removed.contains(&src), "{:?}", report.removed);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let archive_path = report.archived.expect("归档开启必有归档包");
        assert!(archive_path.is_file(), "{}", archive_path.display());

        // 归档里必须是可读的对话：解出来的 markdown 带轮次正文，源文件也在。
        let dest = h.join("restore");
        duster_fs::archive::extract_to(&archive_path, &dest).unwrap();
        let md = dest.join("agent-duster-exports/claude-gone.jsonl.md");
        assert!(md.is_file(), "归档里缺 markdown: {md:?}");
        let body = std::fs::read_to_string(&md).unwrap();
        assert!(body.contains("第一句") && body.contains("第二句"), "{body}");
        assert!(
            dest.join(".claude/projects/gone.jsonl").is_file(),
            "源文件也必须进归档（tar -xf 即原位还原）"
        );

        // 索引无幽灵行：删完 list 不再列它。
        let after = list(Some(&db), &filter(NOW_MS)).unwrap();
        assert!(after.rows.iter().all(|r| r.rid != rid), "{:?}", after.rows);
    }

    /// 库型会话：删的是源库里的那一行，同库其他会话分毫不动；归档里是
    /// 导出的 markdown（可读对话），不是二进制库行。
    #[test]
    fn rm_库型会话删库里一行_同库其他行完好() {
        let (home, db) = seed_home();
        let src = home.path().join("opencode.db");
        // 会话 A（要被删）：fixture 造库 + 两条轮次。
        let ids = duster_adapter::native::opencode_session::fixture_db(
            &src,
            "ses_a",
            &[("user", "会话A第一句"), ("assistant", "会话A第二句")],
        )
        .unwrap();
        // 会话 B（同库，要完好）：直接 INSERT。
        {
            let conn = rusqlite::Connection::open(&src).unwrap();
            conn.execute_batch(
                "INSERT INTO \"session\"(\"id\",\"title\",\"directory\") \
                   VALUES ('ses_b','t','/tmp');\n\
                 INSERT INTO \"message\"(\"id\",\"session_id\",\"time_created\",\"data\") \
                   VALUES ('m_b','ses_b',1,'{\"role\":\"user\"}');\n\
                 INSERT INTO \"part\"(\"id\",\"message_id\",\"session_id\",\"data\") \
                   VALUES ('p_b','m_b','ses_b','{\"type\":\"text\",\"text\":\"会话B第一句\"}');",
            )
            .unwrap();
            let rowid_b: i64 = conn
                .query_row("SELECT rowid FROM \"message\" WHERE \"id\"='m_b'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(rowid_b > 0);
        }

        let idx = Index::open(&db).unwrap();
        let mk = |key: &str| {
            upsert::upsert_resource(
                idx.conn(),
                &ResourceRow {
                    agent_id: "opencode".into(),
                    kind: "session".into(),
                    scope: "user".into(),
                    key: key.into(),
                    path: src.to_string_lossy().into_owned(),
                    size: 4096,
                    mtime_ns: OLD_MS * 1_000_000,
                    hash_content: None,
                    cheap_print: None,
                    clean_level: None,
                    reclaimable: None,
                    install_bytes: None,
                    mapper: None,
                },
            )
            .unwrap()
        };
        let out_a = mk("ses_a");
        upsert::replace_turns(
            idx.conn(),
            out_a.rid,
            &[
                TurnRecord {
                    seq: 0,
                    role: Role::User,
                    ts_ms: Some(OLD_MS),
                    byte_off: ids[0] as u64,
                    byte_len: 0,
                    text: "会话A第一句".into(),
                },
                TurnRecord {
                    seq: 1,
                    role: Role::Assistant,
                    ts_ms: Some(OLD_MS + 1_000),
                    byte_off: ids[1] as u64,
                    byte_len: 0,
                    text: "会话A第二句".into(),
                },
            ],
        )
        .unwrap();
        // B 只需要资源行（这场 rm 的目标是 A，B 留作"同库其他行"的见证）。
        let out_b = mk("ses_b");
        drop(idx);

        let report = remove(&rm_opts(&db, home.path(), true, false), &[out_a.rid]).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        // 库里：A 的行全没了，B 的行全在。
        let conn = rusqlite::Connection::open(&src).unwrap();
        let count = |sql: &str| -> i64 {
            conn.query_row(sql, [], |r| r.get(0)).unwrap()
        };
        assert_eq!(count("SELECT COUNT(*) FROM \"session\" WHERE \"id\"='ses_a'"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM \"message\" WHERE \"session_id\"='ses_a'"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM \"part\" WHERE \"session_id\"='ses_a'"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM \"session\" WHERE \"id\"='ses_b'"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM \"message\" WHERE \"session_id\"='ses_b'"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM \"part\" WHERE \"session_id\"='ses_b'"), 1);
        drop(conn);

        // 归档里是可读对话（导出的 markdown），不是二进制库行。
        let dest = home.path().join("restore");
        let archive = report.archived.unwrap();
        duster_fs::archive::extract_to(&archive, &dest).unwrap();
        let md = dest.join("agent-duster-exports/opencode-ses_a.md");
        assert!(md.is_file(), "库型会话归档里必须有 markdown: {md:?}");
        assert!(
            std::fs::read_to_string(&md).unwrap().contains("会话A第一句"),
            "归档里的 markdown 必须是可读对话"
        );

        // 索引：A 无幽灵行，B 还在。
        let after = list(Some(&db), &filter(NOW_MS)).unwrap();
        assert!(after.rows.iter().all(|r| r.rid != out_a.rid), "{:?}", after.rows);
        assert!(after.rows.iter().any(|r| r.rid == out_b.rid), "B 不该被误删");
    }

    /// 库被锁住（agent 正在写）时这一条进 warnings，不中断删除。
    #[test]
    fn rm_库锁住进_warnings_不中断() {
        let (home, db) = seed_home();
        let src = home.path().join("opencode.db");
        let ids = duster_adapter::native::opencode_session::fixture_db(
            &src,
            "ses_a",
            &[("user", "第一句")],
        )
        .unwrap();

        let idx = Index::open(&db).unwrap();
        let out = upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "opencode".into(),
                kind: "session".into(),
                scope: "user".into(),
                key: "ses_a".into(),
                path: src.to_string_lossy().into_owned(),
                size: 4096,
                mtime_ns: OLD_MS * 1_000_000,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
                mapper: None,
            },
        )
        .unwrap();
        upsert::replace_turns(
            idx.conn(),
            out.rid,
            &[TurnRecord {
                seq: 0,
                role: Role::User,
                ts_ms: Some(OLD_MS),
                byte_off: ids[0] as u64,
                byte_len: 0,
                text: "第一句".into(),
            }],
        )
        .unwrap();
        drop(idx);

        // 独占写锁模拟 agent 正在写库：busy_timeout 等不到 → 该条失败。
        // 用 --no-archive 让删除直接撞锁（归档路径要先导出、先读库，读锁
        // 也会被 EXCLUSIVE 挡住，那不是本测试要验的失败形态）。
        let lock = rusqlite::Connection::open(&src).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE;").unwrap();

        let report = remove(&rm_opts(&db, home.path(), false, false), &[out.rid]).unwrap();
        assert_eq!(report.removed.len(), 0, "锁住时不该删掉任何东西");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("kept"),
            "锁住是这一条的失败,不是整批的: {:?}",
            report.warnings
        );
        // 索引行保留（没删成,不许清掉——删完才能清）。
        let after = list(Some(&db), &filter(NOW_MS)).unwrap();
        assert!(after.rows.iter().any(|r| r.rid == out.rid));
    }

    /// 干跑：不导出、不归档、不删、不动索引。removed 列出"将删的路径"。
    #[test]
    fn rm_干跑不动盘() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (rid, src) = add_session(&idx, h, "claude", "d.jsonl", OLD_MS, 0);
        drop(idx);

        let report = remove(&rm_opts(&db, h, true, true), &[rid]).unwrap();

        assert!(src.is_file(), "预览不许动源文件");
        assert!(report.archived.is_none());
        assert!(!h.join("agent-duster-exports").exists(), "预览不许写导出目录");
        assert!(report.removed.contains(&src), "预览要报将删什么");
        assert_eq!(report.freed_bytes, body(0).len() as u64, "预览要报将释放多少");
        let after = list(Some(&db), &filter(NOW_MS)).unwrap();
        assert!(after.rows.iter().any(|r| r.rid == rid), "索引行不许动");
    }

    /// `--no-archive`：显式接受内容消失，直接删。
    #[test]
    fn rm_不归档直接删() {
        let (home, db) = seed_home();
        let h = home.path();
        let idx = Index::open(&db).unwrap();
        let (rid, src) = add_session(&idx, h, "claude", "na.jsonl", OLD_MS, 0);
        drop(idx);

        let report = remove(&rm_opts(&db, h, false, false), &[rid]).unwrap();
        assert!(!src.exists());
        assert!(report.archived.is_none());
        assert!(report.removed.contains(&src));
    }

    /// 找不到的 rid 进 warnings，不 panic。
    #[test]
    fn rm_未知rid进_warnings() {
        let (home, db) = seed_home();
        let h = home.path();
        let report = remove(&rm_opts(&db, h, true, false), &[9999]).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    }
}
