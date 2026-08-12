//! `duster memory list/show`：把各家的「记忆」归一成一个**只读**视图。
//!
//! 合并与按目标 agent 导出（`merge` / `export`）排 M3——那需要能力矩阵
//! 才知道哪些投影是有损的。M2 只做「看得见」。
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
//! # 形态判定看磁盘，不看清单
//!
//! 清单里好几条 `kind = "memory"` 写的是 `mapper = "stats-only"`
//! （`~/.qoder/memories` 是目录、`~/.cc-switch/cc-switch.db` 是库），
//! 那个字段只决定 scan 怎么**采集体积**，不描述内容形态。所以
//! [`MemoryStore`] 一律由磁盘现状判定：目录 / SQLite 魔数 / 其余单文件。
//! 反过来做的话，上游哪天把 mapper 改一个字，整个视图就空了。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::codec;
use duster_fs::walk::{WalkOptions, walk_files};
use duster_index::db::Index;
use duster_index::foreign;
use duster_index::query::{self, ResourceFilter};

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
    /// 单文件也归在 `kind = "memory"` 之下，它们同样落这一档——只是标题
    /// 取文件名而不去解析正文（那不是 Markdown，没有标题可言）。
    /// 让它们出现在视图里是刻意的：这一档的字节数必须能对上 `status`。
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
    let db = resolve_index(index_path, home);
    if !db.is_file() {
        bail!(
            "index database not found: {}. Run `duster scan` first to build it.",
            db.display()
        );
    }
    let idx = Index::open_readonly(&db)
        .with_context(|| format!("failed to open index read-only: {}", db.display()))?;
    let rows = query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: Vec::new(),
            kinds: vec!["memory".to_string()],
            clean_levels: Vec::new(),
        },
    )?;

    let mut entries: Vec<MemoryEntry> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for row in &rows {
        let path = PathBuf::from(&row.path);
        // 索引是派生物，可能比磁盘旧一步。缺文件只记一条 warning——
        // 「哪些索引行悬空」是 doctor 的活，视图这边不该替它下结论。
        let Ok(meta) = std::fs::metadata(&path) else {
            warnings.push(format!("{}: no longer on disk", path.display()));
            continue;
        };
        if meta.is_dir() {
            expand_dir(&row.agent_id, &path, &mut entries, &mut warnings);
        } else if foreign::is_sqlite(&path) {
            expand_sqlite(&row.agent_id, &path, &meta, &mut entries, &mut warnings);
        } else {
            entries.push(file_entry(&row.agent_id, None, None, &path, &meta));
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
    let db = resolve_index(index_path, home);
    if !db.is_file() {
        bail!(
            "index database not found: {}. Run `duster scan` first to build it.",
            db.display()
        );
    }
    let roots: Vec<PathBuf> = {
        let idx = Index::open_readonly(&db)
            .with_context(|| format!("failed to open index read-only: {}", db.display()))?;
        query::list_resources(
            idx.conn(),
            &ResourceFilter {
                agents: Vec::new(),
                kinds: vec!["memory".to_string()],
                clean_levels: Vec::new(),
            },
        )?
        .iter()
        .map(|r| PathBuf::from(&r.path))
        .collect()
    };

    let (path, rowid) = split_key(key);
    if !is_known_location(&path, rowid.is_some(), &roots) {
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
/// **根本没有行号可补**，`cc-switch.db` 这类库在视图里只承担体积记账，
/// 没有正文可看。这时按「补 rowid」去答，等于把用户带向一个不存在的行号。
/// 如实说原因，并把用户引回 `memory list`——完整解释（warning）在那里。
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
            warnings.push(format!("{}: {reason}", db.display()));
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

/// `path` 是否落在某条已索引的 memory 资源里。
///
/// 目录型资源只放行其下的 Markdown 文件；单文件资源要求精确相等。
/// SQLite 行（`want_row`）要求路径本身就是那条资源。
fn is_known_location(path: &Path, want_row: bool, roots: &[PathBuf]) -> bool {
    for root in roots {
        if path == root {
            return true;
        }
        if want_root_dir(root) && path.starts_with(root) && !want_row && is_markdown(path) {
            return true;
        }
    }
    false
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
    fn seed(home: &Path, rows: &[(&str, &str, PathBuf)]) -> PathBuf {
        let db = home.join(".agent-duster").join("index.db");
        let idx = Index::open(&db).unwrap();
        for (agent, key, path) in rows {
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
                ("claude-code", "CLAUDE.md", claude.clone()),
                ("codex", "AGENTS.md", codex.clone()),
                ("qoder", "memories", qoder_root.clone()),
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

    /// show 只放行索引里认得的位置，绝不变成任取文件的读工具。
    #[test]
    fn show_拒绝索引之外的路径() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let claude = home.join(".claude/CLAUDE.md");
        write(&claude, "# hi\n");
        let secret = home.join("elsewhere/private.md");
        write(&secret, "# not a memory\n");
        let db = seed(home, &[("claude-code", "CLAUDE.md", claude)]);

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

        let db = seed(home, &[("codex", "memories", db_file.clone())]);
        let list = list(Some(&db), Some(home)).unwrap();

        assert_eq!(list.entries.len(), 1);
        let e = &list.entries[0];
        assert_eq!(e.store, MemoryStore::Sqlite);
        assert_eq!(e.path, db_file);
        assert_eq!(e.bytes, size);
        assert_eq!(list.total_bytes, size);
        assert_eq!(list.warnings.len(), 1);
        assert!(
            list.warnings[0].contains("whole-database entry"),
            "{:?}",
            list.warnings
        );
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
        let db = seed(home, &[("codex", "memories", db_file.clone())]);
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
        let db = seed(home, &[("codex", "memories", db_file.clone())]);
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
    /// （cc-switch.db 那类）根本没有行号可补，得说清「不是记忆库」——
    /// regression：用户照旧提示去补一个不存在的行号。
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
                ("codex", "memories", good.clone()),
                ("cc-switch", "cc-switch.db", fake.clone()),
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
        let db = seed(home, &[("gemini-cli", "GEMINI.md", gone.clone())]);
        std::fs::remove_file(&gone).unwrap();

        let list = list(Some(&db), Some(home)).unwrap();
        assert!(list.entries.is_empty());
        assert_eq!(list.total_bytes, 0);
        assert_eq!(list.warnings.len(), 1);
        assert!(list.warnings[0].contains("no longer on disk"));
    }
}
