//! `duster memory list/show/migrate`：把各家的「记忆」归一成一个视图，
//! 再把单条记忆的文本复制进别家的记忆文件（哨兵块，重跑整块替换）。
//!
//! 合并与按目标 agent 导出（`merge` / `export`）排 M3——那需要能力矩阵
//! 才知道哪些投影是有损的。M2 之内 list/show 只读；migrate 只动哨兵块
//! 圈住的那一段，块外的用户内容一个字节不碰。
//!
//! # 各家形态差得很远
//!
//! - claude-code：全局 `~/.claude/CLAUDE.md`，项目级是项目内的 `CLAUDE.md`；
//! - codex：`~/.codex/AGENTS.md`，外加 `memories_1.sqlite`（**SQLite，只读**）；
//! - gemini-cli：`~/.gemini/GEMINI.md`；
//! - qoder：`~/.qoder/memories/<user-hash>/{global,projects/<路径编码>}/<category>/*.md`，
//!   Markdown + YAML frontmatter，一条记忆一个文件；
//! - omp：内嵌在 `config.yml` 里。
//!
//! 归一成 [`MemoryEntry`] 之后，用户第一次能回答「我到底给这些 agent
//! 灌过多少条记忆、有没有互相矛盾的」。
//!
//! # mapper 只用来排除，形态判定看磁盘
//!
//! 清单里 `mapper = "stats-only"` 的 memory 行（settings.json /
//! cc-switch.db 这类配置文件）只承担**体积记账**，不是记忆内容，
//! [`list`] 直接排除——这是本模块唯一用到 mapper 的地方。其余行的内容
//! 形态由磁盘现状判定：目录 / SQLite 魔数 / 其余单文件。反过来完全靠
//! mapper 展开的话，上游哪天把 mapper 改一个字，整个视图就空了。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::codec;
use duster_adapter::manifest::{self, Manifest, MapperName, ResourceSection};
use duster_fs::atomic::write_atomic;
use duster_fs::walk::{WalkOptions, walk_files};
use duster_index::db::{Index, LockBusy};
use duster_index::foreign;
use duster_index::query::{self, ResourceFilter};
use duster_model::ResourceKind;

use crate::freshness;
use crate::scan::{ScanOptions, scan};

/// 目录递归时不深入的目录名。
///
/// 记忆目录里混进 `node_modules` 不是常态，但只要有一次，这条本该是
/// 「看一眼」的视图就会跑上几分钟。
const PRUNE_DIRS: [&str; 3] = ["node_modules", ".git", "dist"];

/// 认作 Markdown 记忆的扩展名（小写比较）。
const MD_EXTS: [&str; 2] = ["md", "markdown"];

/// 取标题时的读取上限。手写记忆文件不会有 1 MiB，超了就只用文件名——
/// 为了一个标题去吸一个巨型文件进内存是不划算的。
const TITLE_READ_CAP: u64 = 1024 * 1024;

/// codex `memories_1.sqlite` 的记忆表（本机实测 2026-08-11，`_sqlx_migrations`
/// 之外只有 `stage1_outputs` 与 `jobs`，记忆正文在前者的 `raw_memory`）。
const CODEX_MEMORY_TABLE: &str = "stage1_outputs";
/// 记忆正文列。这一列缺席即视为「schema 不认识」，降级成整库一条。
const CODEX_MEMORY_BODY: &str = "raw_memory";

/// SQLite 条目在 [`MemoryEntry::path`] 里拼行 id 用的分隔符。
const ROWID_SEP: char = '#';

/// 摘要标题的截断长度（字符）。太长的一行摘要会把列表撑爆。
const TITLE_MAX_CHARS: usize = 80;

/// 记忆的载体形态。决定能不能写回（M3 才用得上），也决定 show 怎么取正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStore {
    /// 一个文件就是一条记忆（`CLAUDE.md` / `AGENTS.md`）。
    ///
    /// 清单把 `~/.claude/settings.json`、`~/.qoder/settings.json` 这类
    /// 单文件也归在 `kind = "memory"` 之下——但那些行声明的是
    /// `stats-only`，已被 [`list`] 过滤，**不会**落进这一档。真正落到
    /// 这一档的是 `memory/markdown` 声明的单文件；标题取文件名（或
    /// frontmatter 的一级标题），不去解析正文（那才是 Markdown）。
    Markdown,
    /// 目录下每个 `.md` 是一条记忆，带 YAML frontmatter（qoder）。
    MarkdownDir,
    /// SQLite 表里的一行（codex `memories_1.sqlite`）。只读。
    Sqlite,
}

/// 归一后的一条记忆。
#[derive(Debug, Clone, Serialize)]
pub struct MemoryEntry {
    pub agent_id: String,
    /// 全局记忆为 None；项目级记忆为该项目的路径（能还原时）。
    ///
    /// qoder 的项目段是**有损编码**（`/Users/x/Code/a-b` 与
    /// `/Users/x/Code/a/b` 编码后同为 `Users-x-Code-a-b`），所以这里
    /// 原样给出编码串而不去猜原路径——猜错的路径比编码串更误导人。
    pub project: Option<String>,
    /// 展示标题：Markdown 取 frontmatter `title` 或首个 `#` 标题，
    /// 都没有就用文件名。
    pub title: String,
    /// 分类（qoder 的 `<category>` 目录名）；其余家为 None。
    pub category: Option<String>,
    pub store: MemoryStore,
    /// 载体位置，同时就是 [`show`] 的 `key`。
    ///
    /// `Sqlite` 时是 `<库路径>#<rowid>`；库读不动而降级成「整库一条」时
    /// 没有 rowid，只有库路径。**必须**把 rowid 带在这里：[`MemoryEntry`]
    /// 是用户手上唯一的句柄，rowid 不在里面，`show` 就没人能调。
    pub path: PathBuf,
    pub bytes: u64,
    pub mtime_ms: i64,
}

/// 归一视图。
#[derive(Debug, Clone, Serialize)]
pub struct MemoryList {
    pub entries: Vec<MemoryEntry>,
    pub total_bytes: u64,
    pub warnings: Vec<String>,
}

/// 列出全部记忆，按 `(agent_id, project, title)` 排序。
///
/// 读索引拿到 `kind = 'memory'` 的行，再按 [`MemoryStore`] 展开：
/// 单文件一条；目录递归出多条；SQLite 走
/// [`duster_index::foreign::open`]（**外部库安全只读**，
/// 读不到就降级成一条「整库」条目并进 warnings，绝不让扫描失败）。
pub fn list(index_path: Option<&Path>, home: Option<&Path>) -> Result<MemoryList> {
    let declared = resolve_index(index_path, home);
    let db = freshness::ensure_exists(Some(&declared))?;
    let roots = memory_roots(&db)?;

    let mut entries: Vec<MemoryEntry> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for (agent_id, path) in &roots {
        // 索引是派生物，可能比磁盘旧一步。缺文件只记一条 warning——
        // 「哪些索引行悬空」是 doctor 的活，视图这边不该替它下结论。
        let Ok(meta) = std::fs::metadata(path) else {
            warnings.push(format!("{}: no longer on disk", path.display()));
            continue;
        };
        if meta.is_dir() {
            expand_dir(agent_id, path, &mut entries, &mut warnings);
        } else if foreign::is_sqlite(path) {
            expand_sqlite(agent_id, path, &meta, &mut entries, &mut warnings);
        } else {
            entries.push(file_entry(agent_id, None, None, path, &meta));
        }
    }

    entries.sort_by(|a, b| {
        a.agent_id
            .cmp(&b.agent_id)
            .then_with(|| a.project.cmp(&b.project))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.path.cmp(&b.path))
    });
    let total_bytes = entries.iter().map(|e| e.bytes).sum();

    Ok(MemoryList {
        entries,
        total_bytes,
        warnings,
    })
}

/// 取一条记忆的正文。Markdown 直接读；SQLite 取那一行的正文列。
///
/// `key` 的形状与 [`MemoryEntry`] 一一对应：Markdown 是路径，
/// SQLite 是 `<库路径>#<rowid>`。
///
/// **`key` 先过索引校验**：只有落在某条 `kind = 'memory'` 资源里的位置
/// 才读得出来。否则 `duster memory show` 就成了一个任取任意文件的读工具，
/// 而它的输出是会被贴进聊天窗口的。
pub fn show(index_path: Option<&Path>, home: Option<&Path>, key: &str) -> Result<String> {
    let declared = resolve_index(index_path, home);
    let db = freshness::ensure_exists(Some(&declared))?;
    let roots = memory_roots(&db)?;

    let (path, rowid) = split_key(key);
    if owner_of(&path, rowid.is_some(), &roots).is_none() {
        bail!("not a known memory location: {key}. Run `duster memory list` to see valid keys.");
    }

    match rowid {
        Some(id) => read_sqlite_body(&path, id),
        None if foreign::is_sqlite(&path) => sqlite_store_hint(&path),
        None => codec::read_to_string_capped(&path, TITLE_READ_CAP)
            .with_context(|| format!("failed to read memory: {}", path.display())),
    }
}

/// 裸路径指到一个 SQLite 库时的答复——**分情况**，不能一口咬定
/// 「key 少了 rowid」。
///
/// 库读得出记忆（真 codex store）：确实是 key 掉了 rowid，指路补
/// `<库路径>#<行号>`；
/// 读不出（不是记忆库 / 锁着 / schema 不认识）：[`list`] 里那种降级条目
/// **根本没有行号可补**。声明 memory/markdown 却指着非 codex 库的行
/// （错配）就是这么答复的——stats-only 的 `cc-switch.db` 已被 [`list`]
/// 过滤，到不了这里。这时按「补 rowid」去答，等于把用户带向一个不存在的
/// 行号。如实说原因，并把用户引回 `memory list`——完整解释（warning）在那里。
fn sqlite_store_hint(db: &Path) -> Result<String> {
    match read_sqlite_rows(db) {
        Ok(_) => bail!(
            "{}: SQLite memory store; use `<database>{}<rowid>` as the key (see `duster memory list`)",
            db.display(),
            ROWID_SEP
        ),
        Err(reason) => bail!("{}: {reason} (see `duster memory list`)", db.display()),
    }
}

// ---------------------------------------------------------------------------
// migrate：把一条记忆的文本复制进另一个 agent 的记忆文件（哨兵块）
// ---------------------------------------------------------------------------

/// 哨兵行的固定前缀。`from=` 属性紧跟其后；匹配旧块时 needle 带上
/// `<agent> ` 的尾随空格，`from=codex` 才不会误认 `from=codex2` 的块。
const SENTINEL_BEGIN: &str = "<!-- duster:begin from=";
const SENTINEL_END: &str = "<!-- duster:end from=";

/// 迁移/删除的读取上限（源与目标同一条）。两者要的都是**完整**正文——
/// 超限是拒绝（capped read 直接报错），绝不是悄悄截断；这个上限只挡
/// 「巨型文件被误声明成记忆」的事故。
const MIGRATE_READ_CAP: u64 = 8 * 1024 * 1024;

/// 一次迁移在目标上的三种结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrateAction {
    /// 目标里已有同 from= 的哨兵块，整块换新。
    Replaced,
    /// 目标文件已存在，块追加在末尾（与已有内容之间空一行）。
    Appended,
    /// 目标文件原先不存在（file 型首迁，或目录型目标的新文件）。
    Created,
}

impl MigrateAction {
    /// 人类模式报告里的字面值，与 `--json` 的 serde 名同一个词。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replaced => "replaced",
            Self::Appended => "appended",
            Self::Created => "created",
        }
    }
}

/// [`migrate`] 的执行报告。dry-run 与真写共用同一形状：块文本恒在，
/// 预览过的人不必为了看内容再跑一遍。
#[derive(Debug, Clone, Serialize)]
pub struct MigrateReport {
    /// 源记忆的归属——索引反查的结果，不是调用方嘴上声明的。
    pub from_agent: String,
    pub to_agent: String,
    /// 源 key，与 [`show`] 同形（路径或 `<库路径>#<rowid>`）。
    pub source: String,
    /// 最终写入的文件：file 型目标即清单声明的文件；目录型目标是
    /// 目录下的新文件 `from-<agent>-<原名>`。
    pub target: PathBuf,
    pub action: MigrateAction,
    pub dry_run: bool,
    /// 写入（或将写入）的哨兵块全文。
    pub block: String,
    /// 迁移成功但索引没能跟上时的补救提示；None = 视图已对齐。
    pub refresh_hint: Option<String>,
}

/// 一个可接收迁移的落点：agent 清单里第一条非 stats-only 的 memory 资源。
#[derive(Debug, Clone, Serialize)]
pub struct MigrateTarget {
    pub agent_id: String,
    /// `~` 已展开的绝对路径。
    pub path: PathBuf,
    /// 磁盘现状是不是目录（目录型写新文件，file 型追加哨兵块）。
    pub dir: bool,
}

/// 把一条记忆的文本复制进另一个 agent 的记忆文件。
///
/// 源用与 [`show`] 同形的 `key` 指定（路径或 `<库路径>#<rowid>`），必须
/// 落在某条已索引、非 stats-only 的 memory 资源里——「什么算记忆」与
/// [`list`] 是同一个判据，settings.json / cc-switch.db 那类记账行既当不了
/// 源也当不了目标。目标从 adapter 清单定位：`to_agent` 第一条非 stats-only
/// 的 memory 资源；磁盘上是单文件就把哨兵块写进这个文件，是目录就写
/// 目录下的新文件 `from-<agent>-<原名>`（头部同样哨兵）。
///
/// # 哨兵块：duster 只动自己画圈的那一段
///
/// ```text
/// <!-- duster:begin from=<agent> src=<原路径~缩写> at=YYYY-MM-DD -->
/// ## From <agent>
/// <正文>
/// <!-- duster:end from=<agent> -->
/// ```
///
/// 同 from= 的块已在目标里 → 整块换新（重跑不累积）；不同 from= → 追加。
/// 块外的每个字节都是用户自己的内容，一律不碰；有 begin 没 end 的残块
/// **报错而不是猜**——从 begin 一路换到文件尾，会把用户写在块后面的
/// 内容一起吃掉。
///
/// `dry_run` 只组块不落盘（报告里带完整块文本）；真写走 duster-fs 的
/// 原子写，成功后跑一次增量扫描把索引对齐——扫不动（锁被占等）不算
/// 迁移失败，报告的 `refresh_hint` 里给补救命令。
///
/// 源文件自己的一级标题会被剥掉：块里的标题是 `## From <agent>`（H2），
/// 源文件的 `# AGENTS` 跟进来就成了「H1 嵌在 H2 下面」——在目标文件的
/// 大纲里它反而盖过自己的出处行，读成用户这份文件的顶级章节。剥的只有
/// 紧贴开头那一个 H1（`# ` 或 setext 下划线那种不管：后者在记忆文件里
/// 没出现过，为它写一套解析等于替不存在的输入养一条分支）；正文中间的
/// 标题一律不动，那是内容结构。
pub fn migrate(
    index_path: Option<&Path>,
    home: Option<&Path>,
    key: &str,
    to_agent: &str,
    dry_run: bool,
) -> Result<MigrateReport> {
    let resolved_home = resolve_home(home)?;
    let declared = resolve_index(index_path, home);
    let db = freshness::ensure_exists(Some(&declared))?;

    // from= 写进哨兵的是索引反查出来的所有权，不是调用方嘴上说的 agent。
    let roots = memory_roots(&db)?;
    let (src_path, rowid) = split_key(key);
    let Some(from_agent) = owner_of(&src_path, rowid.is_some(), &roots).map(str::to_string)
    else {
        bail!("not a known memory location: {key}. Run `duster memory list` to see valid keys.");
    };
    if from_agent == to_agent {
        bail!(
            "source and target are both {to_agent}; migrating a memory into its own agent \
             would only duplicate it"
        );
    }

    let body = match rowid {
        Some(id) => read_sqlite_body(&src_path, id)?,
        // 恒 bail：降级成「整库一条」的条目没有单条正文可迁。
        None if foreign::is_sqlite(&src_path) => sqlite_store_hint(&src_path)?,
        None => codec::read_to_string_capped(&src_path, MIGRATE_READ_CAP)
            .with_context(|| format!("failed to read memory: {}", src_path.display()))?,
    };
    let body = strip_leading_h1(body.trim_end());
    if body.is_empty() {
        bail!("{key}: the source memory is empty; there is nothing to migrate");
    }

    // 目标从清单定位，不从索引：目标 agent 可能从没被扫过（甚至还没装），
    // 清单才是「它的记忆该住在哪」的唯一权威。
    let manifests =
        manifest::load_all(Some(&resolved_home.join(".agent-duster").join("adapters")))?;
    let Some(m) = manifests.iter().find(|m| m.agent.id == to_agent) else {
        let known: Vec<&str> = manifests.iter().map(|m| m.agent.id.as_str()).collect();
        bail!(
            "unknown agent id {to_agent:?}. Known agents: {}",
            known.join(", ")
        );
    };
    let Some(res) = writable_memory(m) else {
        let declared: Vec<&str> = m
            .resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Memory)
            .map(|r| r.path.as_str())
            .collect();
        if declared.is_empty() {
            bail!("{to_agent} declares no memory resource; there is nowhere to migrate into");
        }
        // stats-only 是体积记账（settings.json / cc-switch.db 类），不是记忆
        // 正文；往里写等于替用户改配置文件或数据库。
        bail!(
            "{to_agent} has no writable memory: {} {} stats-only bookkeeping \
             (settings / databases), and duster never writes into those",
            declared.join(", "),
            if declared.len() == 1 { "is" } else { "are" }
        );
    };

    // 形态判定看磁盘，与 [`list`] 同一条哲学：目录就写新文件，其余按
    // 单文件追加。
    let root = expand(&res.path, &resolved_home);
    let target = if std::fs::metadata(&root).map(|m| m.is_dir()).unwrap_or(false) {
        root.join(migrated_file_name(&from_agent, &src_path, rowid))
    } else {
        root
    };
    if target == src_path {
        bail!(
            "target resolves to the source file itself: {}",
            target.display()
        );
    }
    if foreign::is_sqlite(&target) {
        bail!(
            "{}: the target is a SQLite database, not a text file; duster never writes into databases",
            target.display()
        );
    }

    let mut src_attr = fold_home_path(&src_path, &resolved_home);
    if let Some(id) = rowid {
        src_attr.push(ROWID_SEP);
        src_attr.push_str(&id.to_string());
    }
    let block = format!(
        "{SENTINEL_BEGIN}{from_agent} src={src_attr} at={} -->\n## From {from_agent}\n{body}\n{SENTINEL_END}{from_agent} -->\n",
        today_utc()
    );

    let existing = match std::fs::metadata(&target) {
        Ok(meta) if meta.is_dir() => bail!(
            "{}: a directory is in the way of the migration target",
            target.display()
        ),
        Ok(_) => Some(
            codec::read_to_string_capped(&target, MIGRATE_READ_CAP)
                .with_context(|| format!("failed to read target: {}", target.display()))?,
        ),
        Err(_) => None,
    };
    let (content, action) = match &existing {
        None => (block.clone(), MigrateAction::Created),
        Some(old) => splice_block(old, &from_agent, &block)?,
    };

    let refresh_hint = if dry_run {
        None
    } else {
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        write_atomic(&target, content.as_bytes())?;
        refresh_index(&db, home)
    };

    Ok(MigrateReport {
        from_agent,
        to_agent: to_agent.to_string(),
        source: key.to_string(),
        target,
        action,
        dry_run,
        block,
        refresh_hint,
    })
}

/// 剥掉紧贴开头的那一个 `# ` 一级标题（连它后面的空行一起），其余原样。
///
/// 只认 ATX 的 `# `：记忆文件是人手写的 markdown，setext 下划线标题在这
/// 类文件里没出现过，为它写一套解析等于替不存在的输入养一条分支。中间的
/// 标题一律不动——那是内容自己的结构，剥了就改了别人的话。
fn strip_leading_h1(body: &str) -> &str {
    let Some(rest) = body.strip_prefix("# ") else {
        return body;
    };
    // 标题行到行尾；没有换行说明整份记忆就一行标题，剥完是空的，
    // 交给调用方那句「源是空的」去报。
    let after = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => "",
    };
    after.trim_start_matches('\n')
}

/// 列出所有能接收迁移的 agent（交互菜单的目标选单）。
///
/// 判据与 [`migrate`] 完全同源：同一台机器上「菜单里能选」与「真迁会成」
/// 必须是同一个答案。清单里只有 stats-only memory 行的 agent（qoder、
/// cc-switch 这类）不出现。
pub fn migrate_targets(home: Option<&Path>) -> Result<Vec<MigrateTarget>> {
    let home = resolve_home(home)?;
    let manifests = manifest::load_all(Some(&home.join(".agent-duster").join("adapters")))?;
    let mut out: Vec<MigrateTarget> = manifests
        .iter()
        .filter_map(|m| {
            writable_memory(m).map(|r| {
                let path = expand(&r.path, &home);
                let dir = std::fs::metadata(&path).map(|md| md.is_dir()).unwrap_or(false);
                MigrateTarget {
                    agent_id: m.agent.id.clone(),
                    path,
                    dir,
                }
            })
        })
        .collect();
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    Ok(out)
}

/// 清单里第一条可写的 memory 资源。stats-only 不算——那是体积记账
/// （settings.json / cc-switch.db 类），不是能承载文本的记忆。
fn writable_memory(m: &Manifest) -> Option<&ResourceSection> {
    m.resources
        .iter()
        .find(|r| r.kind == ResourceKind::Memory && r.mapper != MapperName::StatsOnly)
}

/// 索引里全部非 stats-only 的 memory 资源根：`(agent_id, 路径)`。
///
/// `mapper = "stats-only"` 的行（settings.json / cc-switch.db 这类）只承担
/// **体积记账**，不是记忆内容：list 不展开、show 不认、migrate 不当源。
/// 字节数不受影响——行还在索引里，status 的 MEMORY 汇总按 kind 求和照旧。
/// NULL 的语义是「未知」（v6 就地升级来的老行还没重扫），按「不是
/// stats-only」处理，绝不因升级整屏消失。
fn memory_roots(db: &Path) -> Result<Vec<(String, PathBuf)>> {
    let idx = Index::open_readonly(db)
        .with_context(|| format!("failed to open index read-only: {}", db.display()))?;
    let rows = query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: Vec::new(),
            kinds: vec!["memory".to_string()],
            clean_levels: Vec::new(),
        },
    )?;
    Ok(rows
        .into_iter()
        .filter(|r| r.mapper.as_deref() != Some("stats-only"))
        .map(|r| (r.agent_id, PathBuf::from(r.path)))
        .collect())
}

// ---------------------------------------------------------------------------
// rm：切掉一个哨兵块，或删掉整份记忆文件
// ---------------------------------------------------------------------------

/// 归档包的动词标签。与 prune 的 `prune` / uninstall 的 `uninstall-<agent>`
/// 同一条命名规矩：用户在导出目录里一眼看得出这包是哪次操作留下的。
const RM_ARCHIVE_LABEL: &str = "rm-memory";

/// `duster memory rm` 确认前先看的**事实**（只读，不删东西）。
///
/// TUI 的确认文案（路径、归类、块数）全靠它；CLI 用它把「删的是什么」
/// 说成一句话。与删除之间隔着用户确认，文件可能变——执行时以重新读到的
/// 磁盘现状为准，不拿勘察结果当执行依据。
#[derive(Debug, Clone, Serialize)]
pub struct RemoveInspection {
    /// 记忆文件（key 展开后的绝对路径）。
    pub file: PathBuf,
    /// 完整哨兵块的 from= 名单（按出现顺序）。
    pub blocks: Vec<String>,
    /// 残块（有 begin 没 end）的 from= 名单。
    pub broken: Vec<String>,
    /// 目录型资源里 `from-<agent>-*.md` 且内容全是 duster 块：
    /// duster 建的整份文件，删它只过一道门。
    pub duster_file: bool,
    /// 块外有用户内容（纯 duster 内容的文件这里是 false）。
    pub has_user_content: bool,
}

impl RemoveInspection {
    /// 文件里 duster 块的总数（完整 + 残块）。
    pub fn block_count(&self) -> usize {
        self.blocks.len() + self.broken.len()
    }
}

/// 只读勘察一次 rm 的目标。不删任何东西。
///
/// 门禁与 [`show`]/[`migrate`] 同一条：`key` 必须落在某条已索引、非
/// stats-only 的 memory 资源里，否则报
/// `not a known memory location`（stats-only 的 settings.json / cc-switch.db
/// 那类记账行本就不在 `memory list` 里，相应地也删不了）。
/// SQLite 记忆行（`<库>#<rowid>`）与裸库路径一律拒删：duster 从不写别人的
/// 数据库——删行是 agent 自己界面里的事，删库是用户自己的事。
pub fn inspect_remove(
    index_path: Option<&Path>,
    home: Option<&Path>,
    key: &str,
) -> Result<RemoveInspection> {
    let declared = resolve_index(index_path, home);
    let db = freshness::ensure_exists(Some(&declared))?;
    let roots = memory_roots(&db)?;

    let (path, rowid) = split_key(key);
    let Some(agent) = owner_of(&path, rowid.is_some(), &roots) else {
        bail!("not a known memory location: {key}. Run `duster memory list` to see valid keys.");
    };
    if rowid.is_some() || foreign::is_sqlite(&path) {
        bail!(
            "cannot delete {key}: duster never writes into SQLite memory stores; \
             delete the row in {agent}'s own interface or remove the database file"
        );
    }

    let content = codec::read_to_string_capped(&path, MIGRATE_READ_CAP)
        .with_context(|| format!("failed to read memory: {}", path.display()))?;
    let (spans, broken) = collect_blocks(&content);
    let has_user_content = has_non_block_content(&content, &spans);
    let duster_file = in_dir_root(&path, &roots)
        && path
            .file_name()
            .is_some_and(|n| is_duster_file_name(&n.to_string_lossy()))
        && !spans.is_empty()
        && broken.is_empty()
        && !has_user_content;
    Ok(RemoveInspection {
        file: path,
        blocks: spans.iter().map(|s| s.from_agent.clone()).collect(),
        broken,
        duster_file,
        has_user_content,
    })
}

/// rm 的目标：删之前先定「删的是什么」，几道门、能不能 --no-archive
/// 都按它分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveTarget {
    /// 切掉单文件里的一个哨兵块（`--from` 指名）。块外每个字节不碰；
    /// 若切完文件里一个字节的用户内容都不剩（那本来就是 duster 建的
    /// 空壳），整份文件一起删。一道确认；归档默认开、`--no-archive` 可关。
    CutBlock { file: PathBuf, from_agent: String },
    /// 目录型资源里 duster 建的整份文件（内容全是 duster 块）：删整个
    /// 文件。一道确认；归档默认开、`--no-archive` 可关。
    DusterFile { file: PathBuf },
    /// 用户手写的整份记忆文件（无 duster 块，或 `--whole-file` 指名整删）：
    /// 删整个文件。两道确认；归档强制（`--no-archive` 拒绝生效）。
    UserFile { file: PathBuf, blocks: usize },
    /// 文件里有 duster 块但没指名哪一块：列出候选让用户选。CLI 直接报错
    /// 给两条出路；TUI 摆子菜单。
    AmbiguousBlocks {
        file: PathBuf,
        agents: Vec<String>,
        broken: Vec<String>,
    },
}

/// 由勘察结果 + 用户的指名（`--from` / `--whole-file`）定出删除目标。
///
/// 规则严格，从不猜：
/// - `--from <A>` 给定时只切 from=A 的块；没有就报错；残块（end 丢了）
///   报 migrate 同款「手改过」错误；
/// - 没给 `--from` 而文件里有 duster 块 → [`RemoveTarget::AmbiguousBlocks`]，
///   出路只有两条：`--from <agent>` 切一块，或 `--whole-file` 删整份——
///   不指名就删整份，等于把「撤销一次迁移」做成「删掉用户自己的
///   CLAUDE.md」；
/// - 文件里一个块都没有 → 用户自己的手写内容（两道确认 + 强制归档）；
/// - `--whole-file` + 文件里有块 → 仍然整删，两道确认 + 强制归档，
///   确认句必须亮出块数。
fn resolve_target(
    inspection: &RemoveInspection,
    from: Option<&str>,
    whole_file: bool,
) -> Result<RemoveTarget> {
    // 两条出路不能同时要：`--from` 切一块，`--whole-file` 删整份。
    // 静默让一个赢，等于替用户猜他哪句是认真的。
    if from.is_some() && whole_file {
        bail!("--from and --whole-file are mutually exclusive: pick one");
    }
    if inspection.duster_file {
        return Ok(RemoveTarget::DusterFile {
            file: inspection.file.clone(),
        });
    }
    if inspection.block_count() == 0 {
        return Ok(RemoveTarget::UserFile {
            file: inspection.file.clone(),
            blocks: 0,
        });
    }
    match from {
        Some(a) => {
            if inspection.broken.iter().any(|b| b == a) {
                bail!(
                    "found `duster:begin from={a}` without its matching end marker; \
                     the block was hand-edited — repair or remove the stale markers first"
                );
            }
            if !inspection.blocks.iter().any(|b| b == a) {
                bail!(
                    "no duster block from={a} in {}; this file holds block(s) from: {}",
                    inspection.file.display(),
                    all_block_agents(inspection).join(", ")
                );
            }
            Ok(RemoveTarget::CutBlock {
                file: inspection.file.clone(),
                from_agent: a.to_string(),
            })
        }
        None if whole_file => Ok(RemoveTarget::UserFile {
            file: inspection.file.clone(),
            blocks: inspection.block_count(),
        }),
        None => Ok(RemoveTarget::AmbiguousBlocks {
            file: inspection.file.clone(),
            agents: inspection.blocks.clone(),
            broken: inspection.broken.clone(),
        }),
    }
}

/// 文件里全部 duster 块的 from=（完整 + 残块，按出现顺序）。
fn all_block_agents(i: &RemoveInspection) -> Vec<&str> {
    i.blocks
        .iter()
        .chain(i.broken.iter())
        .map(|s| s.as_str())
        .collect()
}

/// 一次 `memory rm` 删的是什么（`--json` 的 `kind` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoveKind {
    /// 精确切掉一个哨兵块（块外内容保留；块外本来就无内容时整份删除）。
    DusterBlock,
    /// duster 建的整份文件（目录型 `from-<agent>-*.md`）。
    DusterFile,
    /// 用户手写的整份记忆文件。
    UserFile,
}

/// `duster memory rm` 的执行报告。dry-run 与真写共用同一形状；字段名与
/// [`crate::delete::DeleteReport`] 对齐（`archived` / `removed` /
/// `freed_bytes` / `warnings` 的名字与含义一致），`--json` 输出是删除底座
/// 报告的超集——多出的 `kind` / `from_agent` / `modified` / `dry_run`
/// 是记忆这一组特有的信息，底座不关心。
#[derive(Debug, Clone, Serialize)]
pub struct RemoveReport {
    pub kind: RemoveKind,
    /// 切块时是哪一块（整文件删除时为 None）。
    pub from_agent: Option<String>,
    /// 归档包路径；dry-run / --no-archive 时为 None。
    pub archived: Option<PathBuf>,
    /// 整文件删除的路径（切块且块外还有内容时为空）。
    pub removed: Vec<PathBuf>,
    /// 切块改写过的文件（整文件删除时为空）。
    pub modified: Vec<PathBuf>,
    pub freed_bytes: u64,
    /// 删除成功但索引没能跟上时的补救提示。
    pub warnings: Vec<String>,
    pub dry_run: bool,
}

/// 归档落点目录的预览（无副作用）：`<home>/agent-duster-exports`。
///
/// 确认文案要在执行前亮出归档去处，而归档发生在确认之后。给的是**目录**
/// 而不是精确包名：包名带秒级时间戳，预览时算出来的那一个到真跑那一刻
/// 必然已经不是它了——预告一个保证不会存在的文件名是假信息。实际落点在
/// 真跑之后的报告里。目录本身与 [`crate::delete::archive_before_delete`]
/// 同一条规则，复用同一个 [`crate::delete::exports_dir`]。
pub fn remove_archive_dest(home: Option<&Path>) -> Result<PathBuf> {
    Ok(crate::delete::exports_dir(&resolve_home(home)?))
}

/// 把切块留下的拼缝接起来，并把接缝处连续空行归一到「最多一个空行」。
///
/// duster 追加块时块前放一个空行作分隔。切掉中间某一块，前半段尾部那个
/// 分隔空行与后半段自己的分隔空行会叠成两个——反复迁/切就一路堆到用户
/// 脸上。这里只动**接缝那一处**的连续换行：前半段末尾与后半段开头各自的
/// 换行合起来超过两个就压到两个（= 一个空行，正是 duster 自己的分隔约定）。
/// 两段内部的空行一个字节都不碰——那是用户自己的排版。
///
/// 前半段为空（切的是文件第一块）时不留前导空行：文件不该以空行开头。
fn splice_collapse_blanks(head: &str, tail: &str) -> String {
    let head_trimmed = head.trim_end_matches('\n');
    let tail_trimmed = tail.trim_start_matches('\n');
    if head_trimmed.is_empty() {
        return tail_trimmed.to_string();
    }
    if tail_trimmed.is_empty() {
        // 切的是最后一块：尾部留一个换行收尾即可。
        return format!("{head_trimmed}\n");
    }
    format!("{head_trimmed}\n\n{tail_trimmed}")
}

/// 对一个已分类的目标执行删除（不负责归档——调用方先归档，这里只删）。
///
/// - [`RemoveTarget::CutBlock`]：精确切掉那一段，块外每个字节不碰；切完
///   文件里没有用户内容了就把整份删掉（它是 duster 建的空壳）。
/// - [`RemoveTarget::DusterFile`]：整份文件删除。
/// - [`RemoveTarget::UserFile`]：整份文件删除。
///
/// 执行时重新读盘、重新定位：勘察与执行之间隔着确认，文件可能已经变过；
/// 块定位与 migrate 共用 [`locate_block`]——同一份实现，残块同报
/// 「手改过」的错。
fn execute_target(
    target: &RemoveTarget,
    archived: Option<PathBuf>,
    dry_run: bool,
) -> Result<RemoveReport> {
    let kind = match target {
        RemoveTarget::CutBlock { .. } => RemoveKind::DusterBlock,
        RemoveTarget::DusterFile { .. } => RemoveKind::DusterFile,
        RemoveTarget::UserFile { .. } => RemoveKind::UserFile,
        RemoveTarget::AmbiguousBlocks { .. } => {
            unreachable!("execute_target only receives resolved targets")
        }
    };
    let (removed, modified, freed_bytes) = match target {
        RemoveTarget::CutBlock { file, from_agent } => {
            let content = codec::read_to_string_capped(file, MIGRATE_READ_CAP)
                .with_context(|| format!("failed to read memory: {}", file.display()))?;
            let Some(span) = locate_block(&content, from_agent)? else {
                bail!(
                    "no duster block from={from_agent} in {} anymore (file changed?)",
                    file.display()
                );
            };
            let block_bytes = content[span.begin..span.stop].len() as u64;
            // 拼缝处的空行要归一：duster 追加块时在块前加一个空行作分隔，
            // 切掉块之后那个分隔空行会和后一块自己的分隔空行叠起来，反复
            // 迁/切就一路堆空行到用户脸上。只归一这一个拼缝(最多留一个
            // 空行,正是 duster 自己的分隔约定),块外别处的空行一律不碰。
            let new_content = splice_collapse_blanks(&content[..span.begin], &content[span.stop..]);
            if new_content.trim().is_empty() {
                // 切完一个字节都不剩：那份文件本来就是 duster 建的（migrate
                // 首迁的 Created 就是整份一块），整份删掉，不留下空壳。
                if !dry_run {
                    std::fs::remove_file(file)
                        .with_context(|| format!("failed to delete {}", file.display()))?;
                }
                let size = std::fs::metadata(file)
                    .map(|m| m.len())
                    .unwrap_or(block_bytes);
                (vec![file.clone()], vec![], size)
            } else {
                if !dry_run {
                    write_atomic(file, new_content.as_bytes())?;
                }
                (vec![], vec![file.clone()], block_bytes)
            }
        }
        RemoveTarget::DusterFile { file } | RemoveTarget::UserFile { file, .. } => {
            let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            if !dry_run {
                std::fs::remove_file(file)
                    .with_context(|| format!("failed to delete {}", file.display()))?;
            }
            (vec![file.clone()], vec![], size)
        }
        RemoveTarget::AmbiguousBlocks { .. } => unreachable!(),
    };
    let from_agent = match target {
        RemoveTarget::CutBlock { from_agent, .. } => Some(from_agent.clone()),
        _ => None,
    };
    Ok(RemoveReport {
        kind,
        from_agent,
        archived,
        removed,
        modified,
        freed_bytes,
        warnings: Vec::new(),
        dry_run,
    })
}

/// 用户手写文件不归档就不删：这不是仪式感，`--no-archive` 会被写进脚本、
/// 会被 shell history 补全，而这个动作删的是用户自己写的字。dry-run 不
/// 落盘，`--no-archive` 无从生效，放行（报的仍是「会删什么」）。
fn check_user_file_archive(targets: &[&RemoveTarget], archive: bool, dry_run: bool) -> Result<()> {
    if archive || dry_run {
        return Ok(());
    }
    if let Some(RemoveTarget::UserFile { file, .. }) =
        targets.iter().find(|t| matches!(t, RemoveTarget::UserFile { .. }))
    {
        bail!(
            "{}: this is your own writing, not something duster put there; \
             duster refuses to delete it without an archive. Drop `--no-archive`.",
            file.display()
        );
    }
    Ok(())
}

/// `duster memory rm` 的执行：先分类，再删。
///
/// - [`RemoveTarget::CutBlock`]：精确切掉那一段，块外每个字节不碰；切完
///   文件里没有用户内容了就把整份删掉（它是 duster 建的空壳）。
/// - [`RemoveTarget::DusterFile`]：整份文件删除（一道确认的语义，执行
///   层不再设卡）。
/// - [`RemoveTarget::UserFile`]：整份文件删除，**归档强制**——`--no-archive`
///   在这支上拒绝生效，报一句为何。
/// - [`RemoveTarget::AmbiguousBlocks`]：报错给两条出路（`--from <agent>`
///   切一块 / `--whole-file` 删整份）。
///
/// 删除一律先归档后删（`archive: false` 显式关掉，脚本用）；dry-run 只报
/// 不动。删完跑一次增量扫描把索引对齐——扫不动（锁被占等）不算删除
/// 失败，报告的 `warnings` 里给补救命令。
pub fn remove(
    index_path: Option<&Path>,
    home: Option<&Path>,
    key: &str,
    from: Option<&str>,
    whole_file: bool,
    archive: bool,
    dry_run: bool,
) -> Result<RemoveReport> {
    let resolved_home = resolve_home(home)?;
    let inspection = inspect_remove(index_path, home, key)?;
    let target = resolve_target(&inspection, from, whole_file)?;
    if let RemoveTarget::AmbiguousBlocks { file, agents, broken } = &target {
        let mut known: Vec<&str> = agents.iter().map(String::as_str).collect();
        known.extend(broken.iter().map(String::as_str));
        let mut msg = format!(
            "{} holds {} duster block(s) from: {}. duster won't guess which one to cut: \
             pass `--from <agent>`, or `--whole-file` to delete the whole file \
             (two confirmations; forced archive).",
            file.display(),
            known.len(),
            known.join(", ")
        );
        if !broken.is_empty() {
            msg.push_str(" Note: some blocks are missing their end marker (hand-edited).");
        }
        bail!(msg);
    }
    check_user_file_archive(&[&target], archive, dry_run)?;

    // 先归档后删，顺序不能反——归档失败即中止（archive_before_delete 自己
    // 就是这么承诺的），文件一个字节都不动。
    let archived = if archive && !dry_run {
        Some(
            crate::delete::archive_before_delete(
                &[inspection.file.clone()],
                RM_ARCHIVE_LABEL,
                &resolved_home,
            )
            .with_context(|| {
                format!(
                    "failed to archive {} before deleting; nothing was removed",
                    inspection.file.display()
                )
            })?,
        )
    } else {
        None
    };

    let mut report = execute_target(&target, archived, dry_run)?;
    report.warnings = refresh_after_delete(index_path, home, dry_run);
    Ok(report)
}

/// 删完跑一次增量扫描把索引对齐——扫不动（锁被占等）不算删除失败，
/// 补救命令进报告的 `warnings`。
fn refresh_after_delete(index_path: Option<&Path>, home: Option<&Path>, dry_run: bool) -> Vec<String> {
    if dry_run {
        return Vec::new();
    }
    let declared = resolve_index(index_path, home);
    let Ok(db) = freshness::ensure_exists(Some(&declared)) else {
        return Vec::new();
    };
    refresh_index(&db, home).into_iter().collect()
}

// ---------------------------------------------------------------------------
// rm 批量（TUI 勾选集合）
// ---------------------------------------------------------------------------

/// 一次批量删除的请求：key + 指名切的块。
#[derive(Debug, Clone)]
pub struct RemoveRequest {
    pub key: String,
    /// 指名切哪一块；None = 由分类决定（单块文件自动指名，其余整删）。
    pub from: Option<String>,
}

impl RemoveTarget {
    /// 这份目标对应的文件路径。
    pub fn path(&self) -> &Path {
        match self {
            RemoveTarget::CutBlock { file, .. }
            | RemoveTarget::DusterFile { file }
            | RemoveTarget::UserFile { file, .. }
            | RemoveTarget::AmbiguousBlocks { file, .. } => file,
        }
    }
}

/// 批量计划里的一份：最终目标（CutBlock 的 from= 已被指名或自动指名）。
#[derive(Debug, Clone)]
pub struct BatchItem {
    pub target: RemoveTarget,
}

/// 批量计划里跳过的（没法在集合级指名，得回详情屏逐条删）。
#[derive(Debug, Clone, Serialize)]
pub struct BatchSkip {
    pub key: String,
    pub reason: String,
}

/// 批量删除的规划：每一份将怎么删 + 跳过的。
///
/// 确认文案与执行共用这一份分类——同一套判据，两种措辞不会走散。
#[derive(Debug, Clone)]
pub struct BatchPlan {
    pub items: Vec<BatchItem>,
    pub skipped: Vec<BatchSkip>,
}

/// 批量删除的执行报告。整批只打一个归档包（`archived`），逐条的报告
/// 各自引用它。
#[derive(Debug, Clone, Serialize)]
pub struct BatchRemoveReport {
    pub archived: Option<PathBuf>,
    pub reports: Vec<RemoveReport>,
    pub skipped: Vec<BatchSkip>,
    pub dry_run: bool,
}

/// 批量删除前把每一份分类。能自动指名的（单块、duster 建的整份文件、
/// 无块用户文件）进计划；多块/残块文件没法在集合级指名哪一块，跳过并
/// 说明——批量的确认句是集合级的，逐块指名是详情屏的事。
///
/// 单块文件自动指名那块（不存在歧义，菜单里用户选中的就是这份文件）；
/// 需要 `--from` 才能表达的歧义只在多块时出现，那种文件直接跳过。
pub fn plan_batch(
    index_path: Option<&Path>,
    home: Option<&Path>,
    requests: &[RemoveRequest],
) -> Result<BatchPlan> {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for req in requests {
        let inspection = match inspect_remove(index_path, home, &req.key) {
            Ok(i) => i,
            Err(e) => {
                skipped.push(BatchSkip {
                    key: req.key.clone(),
                    reason: format!("{e:#}"),
                });
                continue;
            }
        };
        // 单块文件自动指名那一块。
        let from = req
            .from
            .clone()
            .or_else(|| (inspection.blocks.len() == 1 && inspection.broken.is_empty())
                .then(|| inspection.blocks[0].clone()));
        match resolve_target(&inspection, from.as_deref(), false) {
            Ok(RemoveTarget::AmbiguousBlocks { agents, broken, .. }) => {
                let mut known: Vec<&str> = agents.iter().map(String::as_str).collect();
                known.extend(broken.iter().map(String::as_str));
                skipped.push(BatchSkip {
                    key: req.key.clone(),
                    reason: format!(
                        "holds duster blocks from: {}; delete it individually to pick which one",
                        known.join(", ")
                    ),
                });
            }
            Ok(t) => items.push(BatchItem { target: t }),
            Err(e) => skipped.push(BatchSkip {
                key: req.key.clone(),
                reason: format!("{e:#}"),
            }),
        }
    }
    Ok(BatchPlan { items, skipped })
}

/// 批量删除的执行：先规划，再删。
///
/// 整批只打一个归档包：逐条归档会在同一秒撞名（包名是秒级时间戳，而
/// [`duster_fs::archive`] 拒绝覆盖已有包）。「先归档后删」的顺序在批量上
/// 保持——包打好了才动任何一个文件。用户手写文件照旧强制归档（`archive:
/// false` 且有用户文件即拒绝）。
pub fn remove_many(
    index_path: Option<&Path>,
    home: Option<&Path>,
    requests: &[RemoveRequest],
    archive: bool,
    dry_run: bool,
) -> Result<BatchRemoveReport> {
    let resolved_home = resolve_home(home)?;
    let plan = plan_batch(index_path, home, requests)?;
    let targets: Vec<&RemoveTarget> = plan.items.iter().map(|i| &i.target).collect();
    check_user_file_archive(&targets, archive, dry_run)?;

    // 去重后统一打包（切块目标归档的是整份文件，两份请求不可能指同一
    // 个文件——key 就是文件路径，列表里一份文件只出现一次；去重只是
    // 防手滑）。
    let mut files: Vec<PathBuf> = targets.iter().map(|t| t.path().to_path_buf()).collect();
    files.sort();
    files.dedup();
    let archived = if archive && !dry_run && !files.is_empty() {
        Some(
            crate::delete::archive_before_delete(&files, RM_ARCHIVE_LABEL, &resolved_home)
                .with_context(|| {
                    format!("failed to archive {} file(s) before deleting; nothing was removed", files.len())
                })?,
        )
    } else {
        None
    };

    let mut reports = Vec::new();
    for item in &plan.items {
        reports.push(execute_target(&item.target, archived.clone(), dry_run)?);
    }
    for r in &mut reports {
        r.warnings = refresh_after_delete(index_path, home, dry_run);
    }
    Ok(BatchRemoveReport {
        archived,
        reports,
        skipped: plan.skipped,
        dry_run,
    })
}

/// 一个哨兵块在文件里的字节范围：`[begin, stop)`。
///
/// begin 是 begin 标记行的行首；stop 是 end 标记行（含）之后的第一字节——
/// `hay[begin..stop]` 恰好是可以整块替换 / 整块切掉的那一段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockSpan {
    from_agent: String,
    begin: usize,
    stop: usize,
}

/// 给一个已知的 begin 行位置找配对的 end 标记，算出块尾。
///
/// 从 `hay[b..]` 里找 from= 配对的 end 行，找不到就是残块：**报错而不是
/// 猜**——从 begin 一路换到文件尾，会把用户写在块后面的内容一起吃掉。
/// `stop` 含 end 标记那一整行（含行尾换行）。
fn block_end(hay: &str, b: usize, from_agent: &str) -> Result<usize> {
    let end = format!("{SENTINEL_END}{from_agent} -->");
    let Some(e) = find_line_start(&hay[b..], &end).map(|p| b + p) else {
        bail!(
            "found `duster:begin from={from_agent}` without its matching end marker; \
             the old block was hand-edited — repair or remove the stale markers first"
        );
    };
    Ok(hay[e..].find('\n').map(|p| e + p + 1).unwrap_or(hay.len()))
}

/// 定位 `from_agent` 的哨兵块。migrate 的整块替换与 rm 的整块切掉共用
/// 这一份定位——同一套行首语义、同一句残块错误；两份实现迟早走散。
///
/// 返回 `None` 表示文件里没有该 from= 的块。
fn locate_block(hay: &str, from_agent: &str) -> Result<Option<BlockSpan>> {
    let begin = format!("{SENTINEL_BEGIN}{from_agent} ");
    let Some(b) = find_line_start(hay, &begin) else {
        return Ok(None);
    };
    let stop = block_end(hay, b, from_agent)?;
    Ok(Some(BlockSpan {
        from_agent: from_agent.to_string(),
        begin: b,
        stop,
    }))
}

/// 枚举文件里全部哨兵块（完整块 + 残块名单）。
///
/// 完整块带字节范围返回；有 begin 没 end 的残块按 from= 单独点名（从残块
/// 之后的结构不再可信，扫描到此为止——报错而不是猜，但列表调用方仍
/// 需要知道残块的 from= 才能给人看路）。
fn collect_blocks(hay: &str) -> (Vec<BlockSpan>, Vec<String>) {
    let mut spans = Vec::new();
    let mut broken: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(b) = find_line_start(&hay[from..], SENTINEL_BEGIN).map(|p| from + p) {
        // 读 begin 行的 from= 属性：`from=` 到下一个空白为止。
        let Some(agent) = begin_agent(&hay[b..]) else {
            // begin 行读不出 from=（手改过）：结构坏了，往后不再可信。
            broken.push("<unreadable>".to_string());
            break;
        };
        match block_end(hay, b, &agent) {
            Ok(stop) => {
                spans.push(BlockSpan {
                    from_agent: agent,
                    begin: b,
                    stop,
                });
                from = stop;
            }
            Err(_) => {
                broken.push(agent);
                break;
            }
        }
    }
    (spans, broken)
}

/// begin 行上 `from=` 的属性值（到下一个空白为止）。
fn begin_agent(line: &str) -> Option<String> {
    let after = line.get(SENTINEL_BEGIN.len()..)?;
    let end = after.find(char::is_whitespace).unwrap_or(after.len());
    let agent = &after[..end];
    (!agent.is_empty()).then(|| agent.to_string())
}

/// 块外有没有非空白内容。duster 建的整份文件里块外只能是空行——
/// 有任何真内容，这份文件就不再纯是 duster 的。
fn has_non_block_content(hay: &str, spans: &[BlockSpan]) -> bool {
    let mut pos = 0;
    for s in spans {
        if hay[pos..s.begin].chars().any(|c| !c.is_whitespace()) {
            return true;
        }
        pos = s.stop;
    }
    hay[pos..].chars().any(|c| !c.is_whitespace())
}

/// 目录型资源里 duster 建的整份文件的名字：`from-<agent>-<原名>`。
///
/// 只看名字形状；是不是 duster 的还要内容校验（见 [`has_non_block_content`]
/// 与 [`collect_blocks`]）——名字像但内容是用户手写的，绝不按 duster 文件删。
fn is_duster_file_name(name: &str) -> bool {
    match name.strip_prefix("from-") {
        Some(rest) => rest.contains('-') && is_markdown(Path::new(name)),
        None => false,
    }
}

/// `file` 是不是某条目录型 memory 资源的成员（而非单文件资源本身）。
fn in_dir_root(file: &Path, roots: &[(String, PathBuf)]) -> bool {
    roots
        .iter()
        .any(|(_, root)| want_root_dir(root) && file != root && file.starts_with(root))
}

/// 把哨兵块放进已有正文：同 from= 整块替换，否则追加（前空一行）。
///
/// 纯函数好单测。追加从不**删**任何已有字节——文件尾缺换行补换行、
/// 缺空行补空行，仅此而已；块外内容逐字节保留。
fn splice_block(old: &str, from_agent: &str, block: &str) -> Result<(String, MigrateAction)> {
    if let Some(span) = locate_block(old, from_agent)? {
        return Ok((
            format!("{}{block}{}", &old[..span.begin], &old[span.stop..]),
            MigrateAction::Replaced,
        ));
    }
    let mut out = old.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    out.push_str(block);
    Ok((out, MigrateAction::Appended))
}

/// `needle` 作为**行首**出现的第一个字节位置。哨兵是行级标记，
/// 行中间出现（散文里引用它）不算数。
fn find_line_start(hay: &str, needle: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        let at = from + rel;
        if at == 0 || hay.as_bytes()[at - 1] == b'\n' {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// 目录型目标里新文件的名字：`from-<agent>-<原名>`。
///
/// SQLite 行没有文件形态的「原名」，用 `<库名去扩展>-<rowid>`；两种一律
/// 保证 `.md` 结尾——[`expand_dir`] 只认 Markdown，后缀不对，迁进去的
/// 内容在 `memory list` 里会凭空消失。
fn migrated_file_name(from_agent: &str, src: &Path, rowid: Option<i64>) -> String {
    let base = match rowid {
        Some(id) => format!(
            "{}-{id}",
            src.file_stem().unwrap_or_default().to_string_lossy()
        ),
        None => src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    };
    let name = format!("from-{from_agent}-{base}");
    if is_markdown(Path::new(&name)) {
        name
    } else {
        format!("{name}.md")
    }
}

/// `<home>/x` → `~/x`；home 之外原样返回。`src=` 属性会随文件被拷去别的
/// 机器，把本机用户名烤进哨兵只会泄露路径，没人用得上。
fn fold_home_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// 迁移落盘后把索引对齐磁盘（增量扫描）。
///
/// 失败**不算**迁移失败——文件已经写成了，只是视图暂时旧一步；把补救
/// 命令递到用户手上即可。锁被占（另一个 duster 在写）认类型不认文案，
/// 与 [`crate::freshness`] 同一条规矩。
fn refresh_index(db: &Path, home: Option<&Path>) -> Option<String> {
    let outcome = scan(&ScanOptions {
        home: home.map(Path::to_path_buf),
        index_path: Some(db.to_path_buf()),
        full: false,
    });
    match outcome {
        Ok(_) => None,
        Err(e) if e.chain().any(|c| c.is::<LockBusy>()) => Some(
            "another duster is writing the index; run `duster scan` afterwards to refresh the view"
                .to_string(),
        ),
        Err(e) => Some(format!(
            "index refresh failed ({e:#}); run `duster scan` to update the view"
        )),
    }
}

/// 确定 home：优先注入值，否则真实用户主目录。清单路径展开与 `src=`
/// 折叠都以它为基准——注入了假 home 却按真 home 展开，是测试里最难
/// 发现的一类错。
fn resolve_home(injected: Option<&Path>) -> Result<PathBuf> {
    if let Some(h) = injected {
        return Ok(h.to_path_buf());
    }
    let h = duster_fs::path::expand_tilde("~");
    if h == Path::new("~") {
        bail!("cannot determine home directory (HOME is not set)");
    }
    Ok(h)
}

/// 把 `~`/`~/...` 相对指定 home 展开；其余形式原样返回。与 scan 的同名
/// 小函数同一形状——清单路径的 `~` 语义必须全仓一致。
fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

/// 今天的 `YYYY-MM-DD`（UTC）。日历算术复用 `duster_fs::zst::stamp`，
/// 不在这里再抄一份闰年规则；at= 是「哪天迁的」的粗粒度记号，不需要
/// 时区精度。
fn today_utc() -> String {
    let s = duster_fs::zst::stamp(std::time::SystemTime::now());
    format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
}

/// 解析一个 Markdown 记忆文件的展示标题。
///
/// 顺序：YAML frontmatter 的 `title` → 首个 `# ` 一级标题 → 文件名（去扩展名）。
/// frontmatter 解析失败不报错，降级到下一级——记忆文件是用户手写的，
/// 一个坏冒号不该让整个视图打不开。
pub fn title_of(markdown: &str, fallback: &str) -> String {
    if let Some(fm) = frontmatter(markdown)
        && let Some(t) = yaml_scalar(fm, "title")
    {
        return clamp_title(&t);
    }
    for line in markdown.lines() {
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        if let Some(h) = line.strip_prefix("# ") {
            let h = h.trim();
            if !h.is_empty() {
                return clamp_title(h);
            }
        }
    }
    fallback.to_string()
}

/// 提取 `---` 围栏内的 frontmatter 文本。无围栏或未闭合返回 `None`。
///
/// 规则与 `duster_adapter::mapper::skill` 里那份实测过的提取器一致
/// （容忍 BOM 与 `\r\n`，闭合围栏必须独占一行），只是这里作用在内存中的
/// 字符串上而不是 `SKILL.md` 文件。[`crate::doctor`] 的 `skill-metadata`
/// 检查复用它，免得同一套容错规则在仓库里出现第二份。
pub(crate) fn frontmatter(md: &str) -> Option<&str> {
    let md = md.strip_prefix('\u{feff}').unwrap_or(md);
    let rest = md
        .strip_prefix("---")
        .and_then(|r| r.strip_prefix("\r\n").or_else(|| r.strip_prefix('\n')))?;
    if let Some(inner) = fence_at_line_start(rest, 0) {
        return Some(inner);
    }
    let mut from = 0;
    while let Some(pos) = rest[from..].find("\n---") {
        let at = from + pos + 1;
        if let Some(inner) = fence_at_line_start(rest, at) {
            return Some(inner);
        }
        from = at + 3;
    }
    None
}

/// 若 `rest[at..]` 以 `---` 开头且该行仅含围栏，返回 `rest[..at]`。
fn fence_at_line_start(rest: &str, at: usize) -> Option<&str> {
    let after = rest[at..].strip_prefix("---")?;
    let ok = after.is_empty()
        || after.starts_with('\n')
        || after.starts_with("\r\n")
        || after.starts_with('\r');
    ok.then(|| &rest[..at])
}

/// 从 frontmatter 文本里取一个**顶层标量**字段。
///
/// 手写扫描而不是上 YAML 解析器：duster-core 没有 `serde_yaml`（也不该为了
/// 读一个 `title` 去加依赖），而这里要的容错本来就比 YAML 严格解析更宽——
/// 同一份文件里别的键写坏了，`title` 照样该读得出来。
///
/// 只认零缩进行，所以嵌套映射里的同名键（`author:\n  title: Dr.`）不会串味；
/// 值两侧的引号剥掉，块标量（`title: |`）与空值一律当作「没有」，
/// 交给上层降级到下一条规则。
pub(crate) fn yaml_scalar(fm: &str, key: &str) -> Option<String> {
    for line in fm.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        // 零缩进 = 顶层键。注释行直接跳过。
        if line.starts_with([' ', '\t']) || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(v) = rest.strip_prefix(':') else {
            continue;
        };
        let v = v.trim();
        // `|` / `>` 是块标量的引导符，正文在后续缩进行里，这里读不到。
        if v.is_empty() || v == "|" || v == ">" {
            return None;
        }
        let v = unquote(v);
        return (!v.is_empty()).then(|| v.to_string());
    }
    None
}

/// 剥掉成对的单/双引号。不做转义还原：记忆标题里出现 `\"` 的概率远低于
/// 一个还原错的标题带来的困惑。
fn unquote(v: &str) -> &str {
    for q in ['"', '\''] {
        if v.len() >= 2
            && v.starts_with(q)
            && v.ends_with(q)
            && let Some(inner) = v.get(1..v.len() - 1)
        {
            return inner;
        }
    }
    v
}

/// 标题取首行并按字符边界截断。摘要列可能是一整段话，原样放进列表会撑爆终端。
fn clamp_title(raw: &str) -> String {
    let first = raw.lines().next().unwrap_or("").trim();
    if first.chars().count() <= TITLE_MAX_CHARS {
        return first.to_string();
    }
    let cut: String = first.chars().take(TITLE_MAX_CHARS).collect();
    format!("{cut}…")
}

/// 缺省索引库位置。`home` 注入时相对它取，绝不回落到真实用户目录——
/// 测试传了假 home 却读到真索引，是这类代码最难发现的一种错。
fn resolve_index(index_path: Option<&Path>, home: Option<&Path>) -> PathBuf {
    match (index_path, home) {
        (Some(p), _) => p.to_path_buf(),
        (None, Some(h)) => h.join(".agent-duster").join("index.db"),
        (None, None) => duster_fs::path::expand_tilde("~/.agent-duster/index.db"),
    }
}

/// 目录型资源：递归出每个 Markdown 文件一条。
///
/// 目录里的非 Markdown 文件不计入——它们不是记忆（qoder 的 `memories`
/// 下就只有 `.md`）。跳过 `node_modules` / `.git` / `dist`。
fn expand_dir(
    agent_id: &str,
    root: &Path,
    entries: &mut Vec<MemoryEntry>,
    warnings: &mut Vec<String>,
) {
    let opts = WalkOptions {
        follow_links: false,
        prune_dirs: PRUNE_DIRS.iter().map(|s| (*s).to_string()).collect(),
    };
    // 先收集再逐个读：walk_files 的回调跑在并行 worker 上，在里面做 IO 会拖住遍历。
    let mut files: Vec<PathBuf> = Vec::new();
    if let Err(e) = walk_files(root, &opts, |p, m| {
        if m.is_file() && is_markdown(p) {
            files.push(p.to_path_buf());
        }
    }) {
        warnings.push(format!("{}: {e}", root.display()));
        return;
    }

    // walk_files 会把指向目录的根符号链接解析成真实路径，相对路径要按它算。
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for f in &files {
        let Ok(meta) = std::fs::metadata(f) else {
            warnings.push(format!("{}: unreadable", f.display()));
            continue;
        };
        let rel = f
            .strip_prefix(&canon)
            .or_else(|_| f.strip_prefix(root))
            .unwrap_or(f);
        let (project, category) = locate(rel);
        let mut e = file_entry(agent_id, project, category, f, &meta);
        e.store = MemoryStore::MarkdownDir;
        entries.push(e);
    }
}

/// 从资源根下的相对路径推出「项目」与「分类」。
///
/// 通用规则，不硬编码 qoder：
/// - `projects` 段之后紧跟的那一段是项目标识（qoder 的路径编码串）；
/// - 文件所在目录名即分类，文件直接躺在资源根下时没有分类。
///
/// 于是 `<hash>/global/<cat>/x.md` → `(None, Some(cat))`，
/// `<hash>/projects/<enc>/<cat>/x.md` → `(Some(enc), Some(cat))`。
fn locate(rel: &Path) -> (Option<String>, Option<String>) {
    let segs: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if segs.len() < 2 {
        return (None, None);
    }
    let dirs = &segs[..segs.len() - 1];
    let project = dirs
        .iter()
        .position(|s| s == "projects")
        .and_then(|i| dirs.get(i + 1))
        .cloned();
    let category = dirs.last().cloned().filter(|c| Some(c) != project.as_ref());
    (project, category)
}

/// 单文件一条。Markdown 才去解析标题，其余（`settings.json` 之类）用文件名。
fn file_entry(
    agent_id: &str,
    project: Option<String>,
    category: Option<String>,
    path: &Path,
    meta: &std::fs::Metadata,
) -> MemoryEntry {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let title = if is_markdown(path) && meta.len() <= TITLE_READ_CAP {
        match codec::read_to_string_capped(path, TITLE_READ_CAP) {
            Ok(text) => title_of(&text, &stem),
            // 读不动就退回文件名：一条记忆的标题不值得让整个视图失败。
            Err(_) => stem,
        }
    } else {
        stem
    };
    MemoryEntry {
        agent_id: agent_id.to_string(),
        project,
        title,
        category,
        store: MemoryStore::Markdown,
        path: path.to_path_buf(),
        bytes: meta.len(),
        mtime_ms: mtime_ms_of(meta),
    }
}

/// SQLite 型：一行一条；库读不动或 schema 不认识就降级成整库一条。
///
/// 降级而不是跳过，是因为跳过会让这个库的字节数从视图里凭空消失，
/// 用户看到的 total 与 `duster status` 对不上，还找不到原因。
fn expand_sqlite(
    agent_id: &str,
    db: &Path,
    meta: &std::fs::Metadata,
    entries: &mut Vec<MemoryEntry>,
    warnings: &mut Vec<String>,
) {
    match read_sqlite_rows(db) {
        Ok(rows) => {
            for (rowid, title, bytes, ts) in rows {
                entries.push(MemoryEntry {
                    agent_id: agent_id.to_string(),
                    project: None,
                    title,
                    category: None,
                    store: MemoryStore::Sqlite,
                    path: PathBuf::from(format!("{}{ROWID_SEP}{rowid}", db.display())),
                    bytes,
                    mtime_ms: ts,
                });
            }
        }
        Err(reason) => {
            // 格式与 session list 的源库警告同一条口径:`<agent>: cannot read
            // <path>: <err>`——用户得知道「谁的记忆读不出来了」。
            warnings.push(format!(
                "{agent_id}: cannot read {}: {reason}",
                db.display()
            ));
            entries.push(MemoryEntry {
                agent_id: agent_id.to_string(),
                project: None,
                title: db
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| db.display().to_string()),
                category: None,
                store: MemoryStore::Sqlite,
                path: db.to_path_buf(),
                bytes: meta.len(),
                mtime_ms: mtime_ms_of(meta),
            });
        }
    }
}

/// 读出库里的全部记忆行 `(rowid, 标题, 正文字节数, mtime_ms)`。
///
/// `Err` 的语义是「这个库读不出记忆」而不是「出错了」，调用方据此降级；
/// 消息直接进 warnings，所以写成人话。
///
/// 上游改 schema 是常态（`_sqlx_migrations` 就在库里摆着），所以表名与
/// **每一列**都先探测：正文列缺席即放弃，其余列缺席就填 `NULL` 继续。
fn read_sqlite_rows(db: &Path) -> Result<Vec<(i64, String, u64, i64)>, String> {
    let conn = match foreign::open(db) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Err(
                "not readable (locked, encrypted, or mid-recovery); listed as one whole-database \
                 entry"
                    .to_string(),
            );
        }
        Err(e) => return Err(format!("{e}; listed as one whole-database entry")),
    };
    match foreign::has_table(&conn, CODEX_MEMORY_TABLE) {
        Ok(true) => {}
        Ok(false) => {
            return Err(format!(
                "no `{CODEX_MEMORY_TABLE}` table; listed as one whole-database entry"
            ));
        }
        Err(e) => return Err(format!("{e}; listed as one whole-database entry")),
    }
    let cols = foreign::columns(&conn, CODEX_MEMORY_TABLE).map_err(|e| e.to_string())?;
    if !cols.iter().any(|c| c == CODEX_MEMORY_BODY) {
        return Err(format!(
            "`{CODEX_MEMORY_TABLE}` has no `{CODEX_MEMORY_BODY}` column; listed as one \
             whole-database entry"
        ));
    }
    // 列名全是本文件里的字面量，不来自外部输入；缺席的换成 NULL 字面量。
    let col = |c: &'static str| {
        if cols.iter().any(|x| x == c) {
            c
        } else {
            "NULL"
        }
    };
    let sql = format!(
        "SELECT rowid, {}, {}, {}, length({CODEX_MEMORY_BODY}), {} FROM {CODEX_MEMORY_TABLE} \
         ORDER BY rowid",
        col("thread_id"),
        col("rollout_slug"),
        col("rollout_summary"),
        col("generated_at"),
    );
    // prepare → 一次性取完 → 立刻 drop：读锁的存活时间以毫秒计，
    // 不挡住对方的 checkpoint（见 duster_index::foreign 的调用方义务）。
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut out: Vec<(i64, String, u64, i64)> = Vec::new();
    let mapped = stmt
        .query_map([], |r| {
            let rowid: i64 = r.get(0)?;
            let thread: Option<String> = r.get(1)?;
            let slug: Option<String> = r.get(2)?;
            let summary: Option<String> = r.get(3)?;
            let len: Option<i64> = r.get(4)?;
            let ts: Option<i64> = r.get(5)?;
            let title = [slug, summary, thread]
                .into_iter()
                .flatten()
                .map(|s| clamp_title(&s))
                .find(|s| !s.is_empty())
                .unwrap_or_else(|| format!("row {rowid}"));
            Ok((
                rowid,
                title,
                len.unwrap_or(0).max(0) as u64,
                ts.map(to_ms).unwrap_or(0),
            ))
        })
        .map_err(|e| e.to_string())?;
    // 逐行推进而不是 `collect::<Result<_, _>>`：那需要写出错误类型，而
    // duster-core 不认识 `rusqlite`（层级约束：SQL 一律在 duster-index 后面）。
    for row in mapped {
        out.push(row.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// 读 SQLite 里某一行的正文。
fn read_sqlite_body(db: &Path, rowid: i64) -> Result<String> {
    let conn = match foreign::open(db)? {
        Some(c) => c,
        None => bail!(
            "{}: not readable (locked, encrypted, or mid-recovery)",
            db.display()
        ),
    };
    if !foreign::has_table(&conn, CODEX_MEMORY_TABLE)? {
        bail!("{}: no `{CODEX_MEMORY_TABLE}` table", db.display());
    }
    let sql = format!("SELECT {CODEX_MEMORY_BODY} FROM {CODEX_MEMORY_TABLE} WHERE rowid = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([rowid])?;
    match rows.next()? {
        Some(r) => Ok(r.get::<_, Option<String>>(0)?.unwrap_or_default()),
        None => bail!("{}{ROWID_SEP}{rowid}: no such row", db.display()),
    }
}

/// 把 `key` 拆成 `(路径, 可选 rowid)`。
///
/// 只有在 `#` 右边是十进制整数、且左边确实是个 SQLite 文件时才认作 rowid：
/// 文件名里出现 `#` 是合法的，误拆会让那个文件永远打不开。
fn split_key(key: &str) -> (PathBuf, Option<i64>) {
    if let Some((left, right)) = key.rsplit_once(ROWID_SEP)
        && let Ok(id) = right.parse::<i64>()
    {
        let p = Path::new(left);
        if foreign::is_sqlite(p) {
            return (p.to_path_buf(), Some(id));
        }
    }
    (PathBuf::from(key), None)
}

/// `path` 落在哪条已索引的 memory 资源里（返回归属 agent）。
///
/// 目录型资源只放行其下的 Markdown 文件；单文件资源要求精确相等。
/// SQLite 行（`want_row`）要求路径本身就是那条资源。找不到 = 不是已知
/// 记忆位置——show 拒读、migrate 拒迁都以此为准。
fn owner_of<'a>(path: &Path, want_row: bool, roots: &'a [(String, PathBuf)]) -> Option<&'a str> {
    for (agent, root) in roots {
        if path == root {
            return Some(agent);
        }
        if want_root_dir(root) && path.starts_with(root) && !want_row && is_markdown(path) {
            return Some(agent);
        }
    }
    None
}

/// 资源根现在是不是一个目录。
fn want_root_dir(root: &Path) -> bool {
    std::fs::metadata(root).map(|m| m.is_dir()).unwrap_or(false)
}

/// 扩展名是否属于 Markdown（大小写不敏感）。
fn is_markdown(p: &Path) -> bool {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| MD_EXTS.contains(&e.as_str()))
}

/// 元数据 mtime -> Unix 毫秒；取不到记 0（视为「无时间戳」，上层按最近用过处理）。
fn mtime_ms_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 库里的时间戳可能是秒也可能是毫秒，上游没统一。按量级判：
/// 10^11 毫秒是 1973 年，任何真实的毫秒时间戳都比它大；
/// 10^11 秒是 5138 年，任何真实的秒时间戳都比它小。中间没有歧义区。
fn to_ms(raw: i64) -> i64 {
    if raw.abs() < 100_000_000_000 {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use duster_index::db::Index;
    use duster_index::upsert::{self, ResourceRow};

    /// 往假 home 的索引里塞若干条 `kind = 'memory'` 资源行，返回索引路径。
    ///
    /// 第四元是 mapper（`None` 模拟从 v6 就地升级而来的老行——mapper 未知，
    /// list 按「不是 stats-only」处理）。
    fn seed(home: &Path, rows: &[(&str, &str, PathBuf, Option<&str>)]) -> PathBuf {
        let db = home.join(".agent-duster").join("index.db");
        let idx = Index::open(&db).unwrap();
        for (agent, key, path, mapper) in rows {
            upsert::upsert_agent(
                idx.conn(),
                &duster_model::AgentInfo {
                    id: (*agent).to_string(),
                    display_name: (*agent).to_string(),
                    root: home.join(format!(".{agent}")),
                    version: None,
                },
                0,
            )
            .unwrap();
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            upsert::upsert_resource(
                idx.conn(),
                &ResourceRow {
                    agent_id: (*agent).to_string(),
                    kind: "memory".to_string(),
                    scope: "global".to_string(),
                    key: (*key).to_string(),
                    path: path.display().to_string(),
                    size,
                    mtime_ns: 0,
                    hash_content: None,
                    cheap_print: None,
                    clean_level: None,
                    reclaimable: None,
                    install_bytes: None,
                    mapper: mapper.map(str::to_string),
                },
            )
            .unwrap();
        }
        // Index 持有单实例写锁，读之前必须放手。
        drop(idx);
        db
    }

    fn write(path: &Path, body: &str) -> u64 {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        body.len() as u64
    }

    /// 三种形态混在一起：两个单文件 + 一棵 qoder 形状的目录树。
    #[test]
    fn 三家记忆归一每个_markdown_文件一条且体积对得上() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        let claude = home.join(".claude/CLAUDE.md");
        let codex = home.join(".codex/AGENTS.md");
        let qoder_root = home.join(".qoder/memories");
        let q_global = qoder_root.join("d5108488/global/user_hobby/hobby.md");
        let q_proj = qoder_root.join("d5108488/projects/Users-x-Code-app/task_summary/done.md");

        let mut total = 0;
        total += write(&claude, "# Claude Global\n\nbe terse\n");
        total += write(&codex, "no heading, no frontmatter\n");
        total += write(
            &q_global,
            "---\ntitle: \"喜欢深色主题\"\nkeywords:\n  - ui\n---\n\nbody\n",
        );
        total += write(&q_proj, "# 任务总结\n\nbody\n");
        // 目录里的非 Markdown 文件不是记忆，不该出现在视图里。
        write(
            &qoder_root.join("d5108488/global/user_hobby/.DS_Store"),
            "junk",
        );

        let db = seed(
            home,
            &[
                ("claude-code", "CLAUDE.md", claude.clone(), Some("memory/markdown")),
                ("codex", "AGENTS.md", codex.clone(), Some("memory/markdown")),
                ("qoder", "memories", qoder_root.clone(), Some("memory/markdown")),
            ],
        );

        let list = list(Some(&db), Some(home)).unwrap();
        assert!(list.warnings.is_empty(), "{:?}", list.warnings);
        assert_eq!(list.entries.len(), 4);
        assert_eq!(list.total_bytes, total);

        // 排序键是 (agent_id, project, title)。
        let ids: Vec<&str> = list.entries.iter().map(|e| e.agent_id.as_str()).collect();
        assert_eq!(ids, ["claude-code", "codex", "qoder", "qoder"]);

        let claude_e = &list.entries[0];
        assert_eq!(claude_e.store, MemoryStore::Markdown);
        assert_eq!(claude_e.title, "Claude Global");
        assert_eq!(claude_e.project, None);

        // 无 frontmatter 无标题 -> 文件名。
        assert_eq!(list.entries[1].title, "AGENTS");

        let global = &list.entries[2];
        assert_eq!(global.store, MemoryStore::MarkdownDir);
        assert_eq!(global.project, None);
        assert_eq!(global.category.as_deref(), Some("user_hobby"));
        assert_eq!(global.title, "喜欢深色主题");

        let proj = &list.entries[3];
        assert_eq!(proj.store, MemoryStore::MarkdownDir);
        assert_eq!(proj.project.as_deref(), Some("Users-x-Code-app"));
        assert_eq!(proj.category.as_deref(), Some("task_summary"));
        assert_eq!(proj.title, "任务总结");

        // show 能按 path 原样回读正文。
        let body = show(Some(&db), Some(home), &proj.path.display().to_string()).unwrap();
        assert!(body.contains("任务总结"));
    }

    /// 验收：stats-only 的 memory 行（settings.json / cc-switch.db 这类
    /// 配置文件）只记体积、不是记忆，list 必须不生成条目；memory/markdown
    /// 行照常展开。字节数不动——行还在索引里，status 的 MEMORY 汇总按
    /// kind 求和，不受影响。
    #[test]
    fn list_过滤掉_stats_only_的假记忆行() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let real = home.join(".claude/CLAUDE.md");
        write(&real, "# Claude\n\nbe terse\n");
        // 假货：claude-code 的 settings.json 声明成 stats-only。
        let fake = home.join(".claude/settings.json");
        write(&fake, "{\"mcpServers\":{}}\n");
        let db = seed(
            home,
            &[
                ("claude-code", "CLAUDE.md", real.clone(), Some("memory/markdown")),
                ("claude-code", "settings.json", fake.clone(), Some("stats-only")),
            ],
        );

        let list = list(Some(&db), Some(home)).unwrap();
        assert!(list.warnings.is_empty(), "{:?}", list.warnings);
        assert_eq!(list.entries.len(), 1, "stats-only 行不得生成条目");
        assert_eq!(list.entries[0].path, real);
        assert_eq!(list.entries[0].title, "Claude");

        // 同一个视图里 show 也不认 stats-only 的位置——list 里都没有它，
        // show 凭什么读得出它（settings.json 的正文会整个被印出来）。
        let err = show(Some(&db), Some(home), &fake.display().to_string()).unwrap_err();
        assert!(
            err.to_string().contains("not a known memory location"),
            "{err}"
        );
    }

    /// 老索引（v6 就地升级）的 memory 行 mapper 是 NULL——未知，不是
    /// stats-only。在下一轮 scan 补齐之前必须照常展开，视图不能因升级消失。
    #[test]
    fn mapper_为_null_的老行照常展开() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude/CLAUDE.md");
        write(&claude, "# Old\n");
        let db = seed(home, &[("claude-code", "CLAUDE.md", claude.clone(), None)]);

        let list = list(Some(&db), Some(home)).unwrap();
        assert_eq!(list.entries.len(), 1);
        assert_eq!(list.entries[0].path, claude);
    }

    /// show 只放行索引里认得的位置，绝不变成任取文件的读工具。
    #[test]
    fn show_拒绝索引之外的路径() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude/CLAUDE.md");
        write(&claude, "# hi\n");
        let secret = home.join("elsewhere/private.md");
        write(&secret, "# not a memory\n");
        let db = seed(home, &[("claude-code", "CLAUDE.md", claude, Some("memory/markdown"))]);

        let err = show(Some(&db), Some(home), &secret.display().to_string()).unwrap_err();
        assert!(err.to_string().contains("not a known memory location"));
    }

    #[test]
    fn 标题优先_frontmatter_再一级标题_最后文件名() {
        assert_eq!(
            title_of("---\ntitle: From Frontmatter\n---\n# Heading\n", "fallback"),
            "From Frontmatter"
        );
        assert_eq!(
            title_of("# The Heading\n\nbody\n", "fallback"),
            "The Heading"
        );
        assert_eq!(
            title_of("plain body, nothing else\n", "fallback"),
            "fallback"
        );
        // 引号剥掉，`##` 不算一级标题。
        assert_eq!(title_of("---\ntitle: 'Quoted'\n---\n", "fb"), "Quoted");
        assert_eq!(title_of("## Second Level\n# First\n", "fb"), "First");
    }

    #[test]
    fn frontmatter_坏了就降级不报错() {
        // 围栏没闭合：整块不算 frontmatter，退到一级标题。
        assert_eq!(
            title_of("---\ntitle: broken\nkeywords: [a\n\n# Real Heading\n", "fb"),
            "Real Heading"
        );
        // 嵌套映射里的同名键不串味，退到文件名。
        assert_eq!(
            title_of("---\nauthor:\n  title: Dr\n---\nbody\n", "fb"),
            "fb"
        );
        // 空值与块标量都当作"没有"。
        assert_eq!(title_of("---\ntitle:\n---\n# H\n", "fb"), "H");
        assert_eq!(title_of("---\ntitle: |\n  long\n---\n# H\n", "fb"), "H");
    }

    /// 库读不动时降级成整库一条 + 一条 warning：字节数绝不能从视图里消失。
    #[test]
    fn 读不动的_sqlite_记忆库降级成整库一条() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let db_file = home.join(".codex/memories_1.sqlite");
        std::fs::create_dir_all(db_file.parent().unwrap()).unwrap();
        // 文件头是 SQLite 魔数，内容是垃圾：is_sqlite 认它，open 读不出来。
        let mut bytes = b"SQLite format 3\0".to_vec();
        bytes.extend(std::iter::repeat_n(0xABu8, 4096));
        std::fs::write(&db_file, &bytes).unwrap();
        let size = bytes.len() as u64;

        let db = seed(home, &[("codex", "memories", db_file.clone(), Some("memory/markdown"))]);
        let list = list(Some(&db), Some(home)).unwrap();

        assert_eq!(list.entries.len(), 1);
        let e = &list.entries[0];
        assert_eq!(e.store, MemoryStore::Sqlite);
        assert_eq!(e.path, db_file);
        assert_eq!(e.bytes, size);
        assert_eq!(list.total_bytes, size);
        assert_eq!(list.warnings.len(), 1);
        let w = &list.warnings[0];
        assert!(
            w.starts_with("codex: cannot read "),
            "要点名 agent 并说读不动: {w}"
        );
        assert!(w.contains(&db_file.display().to_string()), "{w}");
        assert!(w.contains("whole-database entry"), "{w}");
    }

    /// 能读的库按行展开，`path` 带上 rowid 好让 show 有句柄。
    #[test]
    fn 可读的_sqlite_记忆库一行一条并能回读正文() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let db_file = home.join(".codex/memories_1.sqlite");
        std::fs::create_dir_all(db_file.parent().unwrap()).unwrap();
        {
            let c = rusqlite::Connection::open(&db_file).unwrap();
            c.execute_batch(
                "CREATE TABLE stage1_outputs(
                     thread_id TEXT PRIMARY KEY,
                     raw_memory TEXT NOT NULL,
                     rollout_summary TEXT NOT NULL,
                     rollout_slug TEXT,
                     generated_at INTEGER NOT NULL);
                 INSERT INTO stage1_outputs VALUES
                   ('t1', 'the body of one', 'summary one', 'slug-one', 1700000000),
                   ('t2', 'body two', 'summary two', NULL, 1700000001);",
            )
            .unwrap();
        }
        let db = seed(home, &[("codex", "memories", db_file.clone(), Some("memory/markdown"))]);
        let list = list(Some(&db), Some(home)).unwrap();

        assert!(list.warnings.is_empty(), "{:?}", list.warnings);
        assert_eq!(list.entries.len(), 2);
        let titles: Vec<&str> = list.entries.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["slug-one", "summary two"]);
        // 秒被归一成毫秒。
        assert_eq!(list.entries[0].mtime_ms, 1_700_000_000_000);
        assert_eq!(list.entries[0].bytes, "the body of one".len() as u64);

        let key = list.entries[0].path.display().to_string();
        assert!(key.ends_with("#1"), "{key}");
        let body = show(Some(&db), Some(home), &key).unwrap();
        assert_eq!(body, "the body of one");
    }

    /// 上游把表改名了也不能让这个库从视图里消失。
    #[test]
    fn schema_不认识时同样降级成整库一条() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let db_file = home.join(".codex/memories_1.sqlite");
        std::fs::create_dir_all(db_file.parent().unwrap()).unwrap();
        {
            let c = rusqlite::Connection::open(&db_file).unwrap();
            c.execute_batch(
                "CREATE TABLE something_else(a TEXT); INSERT INTO something_else VALUES('x');",
            )
            .unwrap();
        }
        let size = std::fs::metadata(&db_file).unwrap().len();
        let db = seed(home, &[("codex", "memories", db_file.clone(), Some("memory/markdown"))]);
        let list = list(Some(&db), Some(home)).unwrap();

        assert_eq!(list.entries.len(), 1);
        assert_eq!(list.entries[0].bytes, size);
        assert!(
            list.warnings[0].contains("stage1_outputs"),
            "{:?}",
            list.warnings
        );
    }

    /// 裸库路径该分情况答复：真 codex store 才指路补 `#<rowid>`，降级条目
    /// 根本没有行号可补，得说清「不是记忆库」——regression：用户照旧提示
    /// 去补一个不存在的行号。
    ///
    /// 现实里 `cc-switch.db` 声明为 stats-only，会被 [`list`]/[`show`] 过滤、
    /// 根本到不了这里；这个用例里的假 store 用 memory/markdown 声明（错配：
    /// 说好是 Markdown 却指着 SQLite），覆盖的是「降级条目」分支仍然成立。
    #[test]
    fn show_裸库路径按库形态分别答复() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // 真 codex store：裸路径 = key 掉了 rowid，指路补 `#<rowid>`。
        let good = home.join(".codex/memories_1.sqlite");
        std::fs::create_dir_all(good.parent().unwrap()).unwrap();
        {
            let c = rusqlite::Connection::open(&good).unwrap();
            c.execute_batch(
                "CREATE TABLE stage1_outputs(
                     thread_id TEXT PRIMARY KEY,
                     raw_memory TEXT NOT NULL,
                     rollout_summary TEXT NOT NULL,
                     rollout_slug TEXT,
                     generated_at INTEGER NOT NULL);
                 INSERT INTO stage1_outputs VALUES ('t1', 'body', 'summary', NULL, 1700000000);",
            )
            .unwrap();
        }
        // 假 store：表不是记忆表，list 里降级成整库一条——没有行号可补。
        let fake = home.join(".cc-switch/cc-switch.db");
        std::fs::create_dir_all(fake.parent().unwrap()).unwrap();
        {
            let c = rusqlite::Connection::open(&fake).unwrap();
            c.execute_batch("CREATE TABLE providers(a TEXT); INSERT INTO providers VALUES('x');")
                .unwrap();
        }
        let db = seed(
            home,
            &[
                ("codex", "memories", good.clone(), Some("memory/markdown")),
                ("cc-switch", "cc-switch.db", fake.clone(), Some("memory/markdown")),
            ],
        );

        let err = show(Some(&db), Some(home), &good.display().to_string()).unwrap_err();
        assert!(
            err.to_string().contains("use `<database>#<rowid>`"),
            "{err}"
        );

        let err = show(Some(&db), Some(home), &fake.display().to_string()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stage1_outputs"), "{msg}");
        assert!(!msg.contains("use `<database>#<rowid>`"), "{msg}");
        assert!(msg.contains("memory list"), "{msg}");
    }

    /// 索引行指向的文件没了：记一条 warning，不报错、不假装它还在。
    #[test]
    fn 索引行悬空只记_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let gone = home.join(".gemini/GEMINI.md");
        write(&gone, "# g\n");
        let db = seed(home, &[("gemini-cli", "GEMINI.md", gone.clone(), Some("memory/markdown"))]);
        std::fs::remove_file(&gone).unwrap();

        let list = list(Some(&db), Some(home)).unwrap();
        assert!(list.entries.is_empty());
        assert_eq!(list.total_bytes, 0);
        assert_eq!(list.warnings.len(), 1);
        assert!(list.warnings[0].contains("no longer on disk"));
    }

    // ── migrate ──────────────────────────────────────────────────────

    /// 迁移的最小夹具：假 home 里放一条 claude-code 源记忆并建好索引。
    /// 目标（codex 的 `~/.codex/AGENTS.md`）由**内置清单**相对假 home
    /// 展开，不需要额外声明。返回 `(索引路径, 源 key)`。
    fn migrate_fixture(home: &Path, body: &str) -> (PathBuf, String) {
        let claude = home.join(".claude/CLAUDE.md");
        write(&claude, body);
        let db = seed(
            home,
            &[("claude-code", "CLAUDE.md", claude.clone(), Some("memory/markdown"))],
        );
        (db, claude.display().to_string())
    }

    /// 验收：追加到已有文件。用户内容原封不动打头，隔一个空行接哨兵块，
    /// src= 折叠成 `~` 形式；成功后索引已对齐（refresh_hint 为空，且
    /// 目标文件出现在 memory list 里）。
    #[test]
    fn migrate_追加到已有目标文件且用户内容原封不动() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "# Claude Global\n\nbe terse\n");
        let target = home.join(".codex/AGENTS.md");
        write(&target, "user wrote this first\n");

        let report = migrate(Some(&db), Some(home), &key, "codex", false).unwrap();
        assert_eq!(report.action, MigrateAction::Appended);
        assert_eq!(report.from_agent, "claude-code");
        assert_eq!(report.to_agent, "codex");
        assert_eq!(report.target, target);
        assert!(report.refresh_hint.is_none(), "{:?}", report.refresh_hint);

        let out = std::fs::read_to_string(&target).unwrap();
        assert!(
            out.starts_with("user wrote this first\n\n<!-- duster:begin from=claude-code "),
            "{out}"
        );
        assert!(out.contains("src=~/.claude/CLAUDE.md at="), "{out}");
        // 源文件自己的 `# Claude Global` 被剥掉:块里的标题是 `## From …`,
        // H1 跟进来会在目标大纲里盖过自己的出处行。
        assert!(
            out.contains("## From claude-code\nbe terse\n"),
            "{out}"
        );
        assert!(!out.contains("# Claude Global"), "{out}");
        assert!(out.ends_with("<!-- duster:end from=claude-code -->\n"), "{out}");

        // 迁移自带的增量扫描把 codex 的 AGENTS.md 收进了索引：视图立刻可见。
        let after = list(Some(&db), Some(home)).unwrap();
        assert!(
            after.entries.iter().any(|e| e.path == target && e.agent_id == "codex"),
            "{:?}",
            after.entries
        );
    }

    /// 验收：同源重迁整块替换，不累积。块内换成新正文，旧正文消失，
    /// begin 标记只出现一次，用户内容照旧。
    #[test]
    fn migrate_同源重迁整块替换不累积() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "old wisdom\n");
        let target = home.join(".codex/AGENTS.md");
        write(&target, "keep me\n");

        migrate(Some(&db), Some(home), &key, "codex", false).unwrap();
        write(&home.join(".claude/CLAUDE.md"), "new wisdom\n");
        let second = migrate(Some(&db), Some(home), &key, "codex", false).unwrap();
        assert_eq!(second.action, MigrateAction::Replaced);

        let out = std::fs::read_to_string(&target).unwrap();
        assert_eq!(out.matches("<!-- duster:begin").count(), 1, "{out}");
        assert!(out.contains("new wisdom"), "{out}");
        assert!(!out.contains("old wisdom"), "{out}");
        assert!(out.starts_with("keep me\n"), "{out}");
    }

    /// 验收：不同源各自成块共存，块与块之间隔一个空行。
    #[test]
    fn migrate_不同源各自成块共存() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude/CLAUDE.md");
        let gemini = home.join(".gemini/GEMINI.md");
        write(&claude, "from claude\n");
        write(&gemini, "from gemini\n");
        let db = seed(
            home,
            &[
                ("claude-code", "CLAUDE.md", claude.clone(), Some("memory/markdown")),
                ("gemini-cli", "GEMINI.md", gemini.clone(), Some("memory/markdown")),
            ],
        );

        let first = migrate(
            Some(&db),
            Some(home),
            &claude.display().to_string(),
            "codex",
            false,
        )
        .unwrap();
        assert_eq!(first.action, MigrateAction::Created);
        let second = migrate(
            Some(&db),
            Some(home),
            &gemini.display().to_string(),
            "codex",
            false,
        )
        .unwrap();
        assert_eq!(second.action, MigrateAction::Appended);

        let out = std::fs::read_to_string(home.join(".codex/AGENTS.md")).unwrap();
        assert_eq!(out.matches("<!-- duster:begin").count(), 2, "{out}");
        assert!(out.contains("from claude"), "{out}");
        assert!(out.contains("from gemini"), "{out}");
        // 第二块与第一块之间恰好空一行。
        assert!(
            out.contains("<!-- duster:end from=claude-code -->\n\n<!-- duster:begin from=gemini-cli "),
            "{out}"
        );
    }

    /// 验收：目录型目标写新文件 `from-<agent>-<原名>`；重迁在同一个文件里
    /// 整块替换，不再造第二个文件。
    #[test]
    fn migrate_目录型目标写新文件_重迁原地替换() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "v1\n");
        // 目录型 memory 资源只能来自用户清单：内置清单里 qoder 的目录
        // 是 stats-only（扫描器还不支持目录型 memory 展开）。
        let adapters = home.join(".agent-duster").join("adapters");
        std::fs::create_dir_all(&adapters).unwrap();
        std::fs::write(
            adapters.join("dirmem.toml"),
            r#"
[agent]
id = "dirmem"
display_name = "Dir Mem"

[probe]
any_of = ["~/.dirmem"]

[[resource]]
kind = "memory"
scope = "global"
path = "~/.dirmem/memories"
mapper = "memory/markdown"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".dirmem/memories")).unwrap();

        let report = migrate(Some(&db), Some(home), &key, "dirmem", false).unwrap();
        let expect = home.join(".dirmem/memories/from-claude-code-CLAUDE.md");
        assert_eq!(report.action, MigrateAction::Created);
        assert_eq!(report.target, expect);
        let out = std::fs::read_to_string(&expect).unwrap();
        assert!(out.starts_with("<!-- duster:begin from=claude-code "), "{out}");
        assert!(out.ends_with("<!-- duster:end from=claude-code -->\n"), "{out}");

        // 重迁：同一个文件，整块替换，目录里不长第二个文件。
        write(&home.join(".claude/CLAUDE.md"), "v2\n");
        let again = migrate(Some(&db), Some(home), &key, "dirmem", false).unwrap();
        assert_eq!(again.action, MigrateAction::Replaced);
        assert_eq!(again.target, expect);
        let out = std::fs::read_to_string(&expect).unwrap();
        assert!(out.contains("v2"), "{out}");
        assert!(!out.contains("v1"), "{out}");
        assert_eq!(
            std::fs::read_dir(home.join(".dirmem/memories")).unwrap().count(),
            1
        );
    }

    /// 验收：目标 agent 的 memory 资源全是 stats-only（内置 qoder）——
    /// 报明确错误，一个字节都不写。
    #[test]
    fn migrate_stats_only_目标报明确错误() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "hello\n");

        let err = migrate(Some(&db), Some(home), &key, "qoder", false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("stats-only"), "{msg}");
        assert!(msg.contains("qoder"), "{msg}");
        assert!(!home.join(".qoder").exists());
    }

    /// 验收：dry-run 出完整块但不落盘，也不建目录、不跑扫描。
    #[test]
    fn migrate_dry_run_出块不落盘() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "preview me\n");

        let report = migrate(Some(&db), Some(home), &key, "codex", true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.action, MigrateAction::Created);
        assert!(report.block.starts_with("<!-- duster:begin from=claude-code "));
        assert!(report.block.contains("preview me"));
        assert!(report.refresh_hint.is_none());
        assert!(!home.join(".codex").exists(), "dry-run 不得留下任何痕迹");
    }

    /// 有 begin 没 end 的残块：报错而不是猜——从 begin 换到文件尾会把
    /// 用户写在块后面的内容一起吃掉。目标文件原样不动。
    #[test]
    fn migrate_残块有头无尾拒改() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "hello\n");
        let target = home.join(".codex/AGENTS.md");
        let broken = "<!-- duster:begin from=claude-code src=x at=2026-01-01 -->\nstale\nuser text below\n";
        write(&target, broken);

        let err = migrate(Some(&db), Some(home), &key, "codex", false).unwrap_err();
        assert!(
            format!("{err:#}").contains("without its matching end marker"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), broken);
    }

    /// 迁回源 agent 自己没有意义（只会原地复制一份），拒绝。
    #[test]
    fn migrate_拒绝迁回源_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "hello\n");

        let err = migrate(Some(&db), Some(home), &key, "claude-code", false).unwrap_err();
        assert!(
            format!("{err:#}").contains("source and target are both"),
            "{err:#}"
        );
    }

    /// 源必须是索引认得的记忆位置——与 show 同一道门。目标 agent 不存在
    /// 时报已知 agent 清单。
    #[test]
    fn migrate_源与目标都要过校验() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = migrate_fixture(home, "hello\n");

        let stray = home.join("elsewhere/private.md");
        write(&stray, "secret\n");
        let err = migrate(
            Some(&db),
            Some(home),
            &stray.display().to_string(),
            "codex",
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not a known memory location"),
            "{err:#}"
        );

        let err = migrate(Some(&db), Some(home), &key, "no-such-agent", false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown agent id"), "{msg}");
        assert!(msg.contains("Known agents:"), "{msg}");
    }

    // ── rm ──────────────────────────────────────────────────────────

    /// 一块标准哨兵块（与 migrate 写出来的形状一致）。
    fn block(from: &str, body: &str) -> String {
        format!(
            "<!-- duster:begin from={from} src=~/x at=2026-08-14 -->\n## From {from}\n{body}\n\
             <!-- duster:end from={from} -->\n"
        )
    }

    /// 单文件 rm 的夹具：假 home 里放一个记忆文件并建好索引。
    fn rm_fixture(home: &Path, content: &str) -> (PathBuf, String) {
        let claude = home.join(".claude/CLAUDE.md");
        write(&claude, content);
        let db = seed(
            home,
            &[("claude-code", "CLAUDE.md", claude.clone(), Some("memory/markdown"))],
        );
        (db, claude.display().to_string())
    }

    /// 验收：切哨兵块时块外内容逐字不变、块数 -1，归档包先于改写落盘。
    #[test]
    fn rm_切掉哨兵块_块外内容逐字不变() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let before = format!(
            "# My own notes\n\nuser line one\n{}\nuser line two\n",
            block("claude-code", "the wisdom")
        );
        let (db, key) = rm_fixture(home, &before);

        let report =
            remove(Some(&db), Some(home), &key, Some("claude-code"), false, true, false).unwrap();
        assert_eq!(report.kind, RemoveKind::DusterBlock);
        assert_eq!(report.from_agent.as_deref(), Some("claude-code"));
        assert!(report.removed.is_empty(), "{:?}", report.removed);
        assert_eq!(report.modified, vec![home.join(".claude/CLAUDE.md")]);
        let archived = report.archived.expect("默认要归档");
        assert!(archived.is_file(), "{}", archived.display());
        assert_eq!(report.freed_bytes, block("claude-code", "the wisdom").len() as u64);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let out = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
        // 块外两段原样拼接，一个字节不多一个字节不少。
        assert_eq!(out, "# My own notes\n\nuser line one\n\nuser line two\n");
        assert_eq!(out.matches("<!-- duster:begin").count(), 0);
    }

    /// 验收：同文件两个不同 from= 块，只删指名的那一个，另一个连同
    /// 用户内容原样留下；不指名时报错给两条出路。
    #[test]
    fn rm_同文件两个不同from块只删指名的那一个() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let before = format!(
            "top\n{}\n{}\nbottom\n",
            block("claude-code", "one"),
            block("gemini-cli", "two")
        );
        let (db, key) = rm_fixture(home, &before);

        // 没指名：duster 不猜，报错列出候选与两条出路。
        let err = remove(Some(&db), Some(home), &key, None, false, true, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("holds 2 duster block"), "{msg}");
        assert!(msg.contains("claude-code"), "{msg}");
        assert!(msg.contains("gemini-cli"), "{msg}");
        assert!(msg.contains("--from"), "{msg}");
        assert!(msg.contains("--whole-file"), "{msg}");

        let report =
            remove(Some(&db), Some(home), &key, Some("claude-code"), false, true, false).unwrap();
        assert_eq!(report.kind, RemoveKind::DusterBlock);
        assert_eq!(report.from_agent.as_deref(), Some("claude-code"));

        let out = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
        assert_eq!(out.matches("<!-- duster:begin").count(), 1, "{out}");
        assert!(!out.contains("from=claude-code"), "{out}");
        assert!(out.contains("from=gemini-cli"), "{out}");
        assert!(out.starts_with("top\n"), "{out}");
        assert!(out.ends_with("\nbottom\n"), "{out}");
        assert!(out.contains("two"), "{out}");
        assert!(!out.contains("one"), "{out}");

        // 指名一个不存在的 from=：报错并把已有的列出来。
        let err =
            remove(Some(&db), Some(home), &key, Some("no-such"), false, true, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no duster block from=no-such"), "{msg}");
        assert!(msg.contains("gemini-cli"), "{msg}");
    }

    /// 验收：删掉最后一个块后文件仍在，只剩用户内容——块外字节一个不碰。
    #[test]
    fn rm_删掉最后一个块后文件仍在只剩用户内容() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let before = format!("only my words\n\n{}", block("claude-code", "x"));
        let (db, key) = rm_fixture(home, &before);

        let report =
            remove(Some(&db), Some(home), &key, Some("claude-code"), false, true, false).unwrap();
        assert_eq!(report.kind, RemoveKind::DusterBlock);
        assert!(report.modified == vec![home.join(".claude/CLAUDE.md")]);
        assert!(report.removed.is_empty(), "{:?}", report.removed);

        let out = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
        // 拼缝归一：块前那个分隔空行跟着块一起走,不在文件尾留一个空行。
        // 反复迁/切不该在用户文件里堆空行。
        assert_eq!(out, "only my words\n");
        assert!(home.join(".claude/CLAUDE.md").exists());
        // 索引跟着更新：文件不再是 duster 块载体，视图里只剩用户内容那条。
        let after = list(Some(&db), Some(home)).unwrap();
        assert!(after.entries.iter().any(|e| e.path == home.join(".claude/CLAUDE.md")));
    }

    /// 验收：切完没有用户内容的空壳（migrate 首迁 Created 的那种）——
    /// 整份删除，不留空文件。
    #[test]
    fn rm_切完没有用户内容_整份删除() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = rm_fixture(home, &block("claude-code", "only block"));
        let file = home.join(".claude/CLAUDE.md");

        let report =
            remove(Some(&db), Some(home), &key, Some("claude-code"), false, true, false).unwrap();
        assert_eq!(report.kind, RemoveKind::DusterBlock);
        assert_eq!(report.removed, vec![file.clone()]);
        assert!(!file.exists());
    }

    /// 验收：用户手写文件走两道门——核心层即 --no-archive 拒绝生效、
    /// 归档强制、先归档后删（归档里能解回原文）。
    #[test]
    fn rm_用户手写文件_拒绝no_archive且强制归档() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = rm_fixture(home, "my precious notes\nsecond line\n");
        let file = home.join(".claude/CLAUDE.md");

        let err = remove(Some(&db), Some(home), &key, None, false, false, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("your own writing"), "{msg}");
        assert!(msg.contains("--no-archive"), "{msg}");
        assert!(file.exists(), "拒绝时必须一个字节都不动");

        let report = remove(Some(&db), Some(home), &key, None, false, true, false).unwrap();
        assert_eq!(report.kind, RemoveKind::UserFile);
        assert!(report.from_agent.is_none());
        let archived = report.archived.expect("用户手写文件必须归档");
        assert!(archived.starts_with(home.join("agent-duster-exports")), "{archived:?}");
        assert!(!file.exists(), "归档成功之后才删");

        let dest = home.join("unpack");
        duster_fs::archive::extract_to(&archived, &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join(".claude/CLAUDE.md")).unwrap(),
            "my precious notes\nsecond line\n"
        );

        // 删完索引里不再有它。
        let after = list(Some(&db), Some(home)).unwrap();
        assert!(!after.entries.iter().any(|e| e.path == file), "{:?}", after.entries);
    }

    /// 验收：目录型资源里 duster 建的 `from-<agent>-*.md` 整份删除；
    /// 用户往里面加了私货就不再按 duster 文件处理。
    #[test]
    fn rm_目录型from文件整份删除() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let adapters = home.join(".agent-duster").join("adapters");
        std::fs::create_dir_all(&adapters).unwrap();
        std::fs::write(
            adapters.join("dirmem.toml"),
            r#"
[agent]
id = "dirmem"
display_name = "Dir Mem"

[probe]
any_of = ["~/.dirmem"]

[[resource]]
kind = "memory"
scope = "global"
path = "~/.dirmem/memories"
mapper = "memory/markdown"
"#,
        )
        .unwrap();
        let dir = home.join(".dirmem/memories");
        std::fs::create_dir_all(&dir).unwrap();

        let file = dir.join("from-claude-code-CLAUDE.md");
        write(&file, &block("claude-code", "migrated wisdom"));
        let db = seed(home, &[("dirmem", "memories", dir.clone(), Some("memory/markdown"))]);

        let inspect = inspect_remove(Some(&db), Some(home), &file.display().to_string()).unwrap();
        assert!(inspect.duster_file, "{inspect:?}");

        let report =
            remove(Some(&db), Some(home), &file.display().to_string(), None, false, true, false)
                .unwrap();
        assert_eq!(report.kind, RemoveKind::DusterFile);
        assert_eq!(report.removed, vec![file.clone()]);
        assert!(!file.exists());

        // 用户往 duster 文件里加了私货 → 不再是 duster 文件（要 --from 或
        // --whole-file，两道门）。
        let file2 = dir.join("from-gemini-cli-GEMINI.md");
        write(&file2, &format!("user appended\n{}", block("gemini-cli", "x")));
        let db2 = seed(home, &[("dirmem", "memories", dir.clone(), Some("memory/markdown"))]);
        let inspect2 =
            inspect_remove(Some(&db2), Some(home), &file2.display().to_string()).unwrap();
        assert!(!inspect2.duster_file, "{inspect2:?}");
        assert!(inspect2.has_user_content);
    }

    /// 验收：--whole-file + 文件里有块，仍走用户文件门（强制归档）；
    /// --no-archive 同样拒绝。
    #[test]
    fn rm_whole_file_带块仍走用户文件门() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let before = format!("mine\n{}", block("claude-code", "x"));
        let (db, key) = rm_fixture(home, &before);
        let file = home.join(".claude/CLAUDE.md");

        let err = remove(Some(&db), Some(home), &key, None, true, false, false).unwrap_err();
        assert!(format!("{err:#}").contains("your own writing"), "{err:#}");

        let report = remove(Some(&db), Some(home), &key, None, true, true, false).unwrap();
        assert_eq!(report.kind, RemoveKind::UserFile);
        assert_eq!(report.removed, vec![file.clone()]);
        assert!(report.archived.is_some());
        assert!(!file.exists());
    }

    /// 验收：dry-run 只报不动——文件原样、不打包、不删。
    #[test]
    fn rm_dry_run_不动盘() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let before = format!("mine\n{}", block("claude-code", "x"));
        let (db, key) = rm_fixture(home, &before);

        let report =
            remove(Some(&db), Some(home), &key, Some("claude-code"), false, true, true).unwrap();
        assert!(report.dry_run);
        assert!(report.archived.is_none());
        assert_eq!(report.kind, RemoveKind::DusterBlock);
        assert_eq!(report.freed_bytes, block("claude-code", "x").len() as u64);
        assert_eq!(std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(), before);
        assert!(!home.join("agent-duster-exports").exists(), "dry-run 不得打包");

        // 用户文件 + dry-run + --no-archive：不落盘，无从生效，放行。
        let user = home.join(".codex/AGENTS.md");
        write(&user, "hand written\n");
        let db2 = seed(home, &[("codex", "AGENTS.md", user.clone(), Some("memory/markdown"))]);
        let report2 = remove(
            Some(&db2),
            Some(home),
            &user.display().to_string(),
            None,
            false,
            false,
            true,
        )
        .unwrap();
        assert!(report2.dry_run);
        assert_eq!(report2.kind, RemoveKind::UserFile);
        assert!(user.exists());
    }

    /// 验收：stats-only 行不在 memory list 里，rm 同报 not a known memory
    /// location；SQLite 记忆行与裸库路径一律拒删（duster 从不写别人的
    /// 数据库）。
    #[test]
    fn rm_stats_only_与_sqlite_都拒删() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let real = home.join(".claude/CLAUDE.md");
        write(&real, "# hi\n");
        let fake = home.join(".claude/settings.json");
        write(&fake, "{}");
        let db = seed(
            home,
            &[
                ("claude-code", "CLAUDE.md", real.clone(), Some("memory/markdown")),
                ("claude-code", "settings.json", fake.clone(), Some("stats-only")),
            ],
        );

        let err = remove(
            Some(&db),
            Some(home),
            &fake.display().to_string(),
            None,
            false,
            true,
            false,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not a known memory location"), "{err:#}");
        assert!(fake.exists());

        let db_file = home.join(".codex/memories_1.sqlite");
        std::fs::create_dir_all(db_file.parent().unwrap()).unwrap();
        {
            let c = rusqlite::Connection::open(&db_file).unwrap();
            c.execute_batch(
                "CREATE TABLE stage1_outputs(
                     thread_id TEXT PRIMARY KEY,
                     raw_memory TEXT NOT NULL,
                     rollout_summary TEXT NOT NULL,
                     rollout_slug TEXT,
                     generated_at INTEGER NOT NULL);
                 INSERT INTO stage1_outputs VALUES ('t1','body','summary',NULL,1700000000);",
            )
            .unwrap();
        }
        let db2 = seed(home, &[("codex", "memories", db_file.clone(), Some("memory/markdown"))]);
        let key = format!("{}#1", db_file.display());
        let err = remove(Some(&db2), Some(home), &key, None, false, true, false).unwrap_err();
        assert!(format!("{err:#}").contains("never writes into SQLite"), "{err:#}");

        let err = remove(
            Some(&db2),
            Some(home),
            &db_file.display().to_string(),
            None,
            false,
            true,
            false,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("never writes into SQLite"), "{err:#}");
    }

    /// 残块（有 begin 没 end）：rm 与 migrate 同一条规矩，报错而不是猜。
    #[test]
    fn rm_残块指名_报手改过错误() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let broken = "mine\n<!-- duster:begin from=claude-code src=x at=2026-01-01 -->\nstale\n";
        let (db, key) = rm_fixture(home, broken);

        let err =
            remove(Some(&db), Some(home), &key, Some("claude-code"), false, true, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("without its matching end marker"),
            "{err:#}"
        );
        assert_eq!(std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(), broken);
    }

    /// `--from` 与 `--whole-file` 是两条出路，不能同时要。
    #[test]
    fn rm_from与whole_file互斥() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (db, key) = rm_fixture(home, &format!("mine\n{}", block("claude-code", "x")));

        let err =
            remove(Some(&db), Some(home), &key, Some("claude-code"), true, true, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("mutually exclusive"),
            "{err:#}"
        );
    }

    /// 批量（TUI 勾选集合）：单块文件自动指名直切、用户手写文件整删、
    /// 多块文件跳过；整批只打一个归档包。
    #[test]
    fn rm_批量_单块直切_用户文件整删_多块跳过() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // 单块文件：用户内容 + 一个 claude-code 块。
        let single = home.join(".claude/CLAUDE.md");
        write(
            &single,
            &format!("mine\n{}", block("claude-code", "one")),
        );
        // 用户手写文件：没有块。
        let user = home.join(".codex/AGENTS.md");
        write(&user, "hand written\n");
        // 多块文件：两个块，批量里没法指名，必须跳过。
        let multi = home.join(".gemini/GEMINI.md");
        write(
            &multi,
            &format!("{}{}", block("claude-code", "a"), block("gemini-cli", "b")),
        );
        let db = seed(
            home,
            &[
                ("claude-code", "CLAUDE.md", single.clone(), Some("memory/markdown")),
                ("codex", "AGENTS.md", user.clone(), Some("memory/markdown")),
                ("gemini-cli", "GEMINI.md", multi.clone(), Some("memory/markdown")),
            ],
        );

        let requests = vec![
            RemoveRequest { key: single.display().to_string(), from: None },
            RemoveRequest { key: user.display().to_string(), from: None },
            RemoveRequest { key: multi.display().to_string(), from: None },
        ];
        let report = remove_many(Some(&db), Some(home), &requests, true, false).unwrap();

        // 两个目标（单块 + 用户文件），多块文件被跳过并说明。
        assert_eq!(report.reports.len(), 2);
        assert_eq!(report.reports[0].kind, RemoveKind::DusterBlock);
        assert_eq!(report.reports[0].from_agent.as_deref(), Some("claude-code"));
        assert_eq!(report.reports[1].kind, RemoveKind::UserFile);
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].key.ends_with("GEMINI.md"), "{:?}", report.skipped);
        assert!(report.skipped[0].reason.contains("individually"), "{:?}", report.skipped);

        // 整批只打一个归档包（同一秒重跑会撞名，批量必须一次打包）。
        let archived = report.archived.expect("批量要归档");
        assert!(archived.is_file());
        let exports = home.join("agent-duster-exports");
        assert_eq!(
            std::fs::read_dir(&exports).unwrap().count(),
            1,
            "整批只能有一个归档包"
        );

        // 单块文件：块切掉，用户内容原样保留；用户文件：消失；多块文件：没动。
        assert_eq!(std::fs::read_to_string(&single).unwrap(), "mine\n");
        assert!(!user.exists());
        assert!(multi.exists());
        assert_eq!(std::fs::read_to_string(&multi).unwrap().matches("<!-- duster:begin").count(), 2);

        // 批量里含用户文件时 --no-archive 同样拒绝。
        let err = remove_many(Some(&db), Some(home), &requests, false, false).unwrap_err();
        assert!(format!("{err:#}").contains("your own writing"), "{err:#}");
    }
}
