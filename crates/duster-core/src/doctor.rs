//! `duster doctor`：一份**只读**的健康报告。
//!
//! 六项检查，每项独立跑、独立失败：`secrets`（明文凭据）·
//! `skill-metadata`（SKILL.md 元数据）· `config-syntax`（配置能不能解析）·
//! `dangling-reference`（索引行与符号链接是否悬空）· `sqlite-integrity`
//! （库是否完好）· `mcp-reachability`（声明还能不能启动）。
//!
//! # 两条框架层的规矩
//!
//! 1. **单项失败绝不中断整轮**。一项检查报错就变成一条
//!    [`Severity::Error`] 的 [`Finding`] 外加一条 warning，其余照跑——
//!    体检报告死在第一个问题上就不是报告了。
//! 2. **[`DoctorReport::checks_run`] 只记真的跑过的**。跑了没发现是信息，
//!    没跑却装作跑过是撒谎：被开关关掉的（`--secrets` / `--ping`）与
//!    前置条件缺失的（索引还没建）都不进这个列表，改进 warnings 说明原因。
//!
//! # `--secrets`：明文凭据扫描
//!
//! ## 三条不可协商的约束
//!
//! 1. **掩码输出**。命中的值一律只显示前 4 后 4，中间打码。
//!    一个扫描凭据的工具把凭据原样打进终端，等于自己变成了泄露源
//!    （终端有 scrollback、CI 有日志、截图会发群里）。
//! 2. **结果不落盘、不入索引**。`index.db` 是可丢弃的派生物，会被同步、
//!    会被备份；凭据不能借道它扩散到别处。
//! 3. **只报告，不修改**。撤销 token 必须去 platform 做，duster 报出
//!    "在哪、是什么、建议去哪撤销"就到此为止。
//!
//! ## 判据
//!
//! 已知位置（[`known_secret_paths`]）优先——那是实测出来的确定命中点；
//! 熵检测作为补充，用于捞出未知位置里的高熵串，宁可多报几条让用户自己看。
//!
//! 文本扫描有一个天生的盲区：**opencode 把 token 存在 SQLite 列里**
//! （`account.access_token` / `control_account.refresh_token` /
//! `credential.value`，本机实测 2026-08-11 确有其表）。逐行扫文本永远看不见
//! 它们，所以 [`doctor`] 额外只读打开那个库、按列取值、掩码后报出。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::codec;
use duster_adapter::manifest;
use duster_fs::walk::{WalkOptions, walk_files};
use duster_index::db::Index;
use duster_index::query::{self, ResourceFilter, ResourceRecord};
use duster_index::{foreign, maintenance};

/// 一条发现的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// 值得知道，但不用做什么。
    Info,
    /// 该看一眼：明文凭据、缺元数据的 skill、悬空的索引行。
    Warn,
    /// 坏了：配置解析不了、库损坏、某项检查自己跑挂了。
    Error,
}

/// 一条发现。
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// 产出它的检查名，与 [`DoctorReport::checks_run`] 里的字符串一致。
    pub check: String,
    pub severity: Severity,
    /// 出问题的东西：文件路径、`<库>#<表>.<列>:<行>`、或 server 名。
    pub subject: String,
    /// 人话描述。**凭据一律已掩码**，任何时候都不含原文。
    pub detail: String,
    /// 可照做的修法（如 `duster scan`）；没有明确修法为 None。
    pub fix: Option<String>,
}

/// 一轮体检的结果。整体直接进 `--json` 信封的 `data`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DoctorReport {
    pub findings: Vec<Finding>,
    /// **真的跑过**的检查名，按执行顺序。
    ///
    /// 跑了没发现的检查也在这里——那是信息（"这一项我查过了"）。
    /// 被开关关掉或前置条件缺失而没跑的**不在**这里，原因进
    /// [`DoctorReport::warnings`]。
    pub checks_run: Vec<String>,
    pub warnings: Vec<String>,
}

/// 体检的输入。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DoctorOptions {
    /// 索引库路径；缺省 `<home>/.agent-duster/index.db`。
    pub index_path: Option<PathBuf>,
    /// 假 home 注入口（测试用）；缺省真实用户主目录。
    pub home: Option<PathBuf>,
    /// 只查这几个 agent；空 Vec = 全部。
    pub agents: Vec<String>,
    /// 是否跑明文凭据扫描。**默认关**：它要读遍 agent 的配置文件，
    /// 用户得先说要。
    pub secrets: bool,
    /// 是否 ping MCP。**默认关**：凭一份配置文件就去 spawn 别人机器上的
    /// 进程，是不该在没被要求时做的事——`doctor` 的其余五项一个字节都不写、
    /// 一个进程都不起，这一项破的正是那条性质。
    pub ping: bool,
    /// 只跑这几项（取值域 [`ALL_CHECKS`]）；空 = 全跑，即默认行为。
    ///
    /// 这是**真的不跑**，不是跑完再筛结果：`sqlite-integrity` 在本机要对
    /// 822 MB 的库做全库校验，用户明说了不要还照跑，等于这个参数没有。
    pub checks: Vec<String>,
}

/// 检查名。集中一处，`checks_run` 与每条 [`Finding::check`] 共用同一份字面量。
const CHECK_SECRETS: &str = "secrets";
const CHECK_SKILL_METADATA: &str = "skill-metadata";
const CHECK_CONFIG_SYNTAX: &str = "config-syntax";
const CHECK_DANGLING: &str = "dangling-reference";
const CHECK_SQLITE: &str = "sqlite-integrity";
const CHECK_MCP: &str = "mcp-reachability";

/// 全部检查名，**按 [`doctor`] 的执行顺序**。
///
/// 同时是 [`DoctorOptions::checks`] 的取值域与 CLI `--check` 的候选表：
/// 名字写在这里一处，命令行的补全、报错清单与报告里的分组顺序就不会走散。
pub const ALL_CHECKS: [&str; 6] = [
    CHECK_SECRETS,
    CHECK_SKILL_METADATA,
    CHECK_CONFIG_SYNTAX,
    CHECK_DANGLING,
    CHECK_SQLITE,
    CHECK_MCP,
];

/// opencode 的库（本机实测 2026-08-11）。token 存在库内列里，文本扫描看不见。
const OPENCODE_DB: &str = ".local/share/opencode/opencode.db";

/// 库内 token 列（表名, 列名）。表或列缺席一律跳过——上游改 schema 是常态。
const OPENCODE_TOKEN_COLS: [(&str, &str); 4] = [
    ("account", "access_token"),
    ("account", "refresh_token"),
    ("control_account", "access_token"),
    ("control_account", "refresh_token"),
];

/// 凭据表。本机实测列为 `id` / `label` / `value`。
const CREDENTIAL_TABLE: &str = "credential";

/// 跑一轮体检。
///
/// **全程只读**：不写任何文件、不改索引、不碰 agent 的数据；唯一的例外是
/// `--ping` 会 spawn 子进程，所以它默认关着。
///
/// 任何一项检查失败都只变成一条 [`Severity::Error`] 的 [`Finding`] 加一条
/// warning，其余照跑。返回 `Err` 只剩两种情况：连 home 都定不下来，
/// 或者 [`DoctorOptions::checks`] 里有不认识的名字——那时候什么都无从查起，
/// 而后者若默默跑成一份空报告，用户会把「名字打错了」读成「一切正常」。
pub fn doctor(opts: &DoctorOptions) -> Result<DoctorReport> {
    if let Some(bad) = opts
        .checks
        .iter()
        .find(|c| !ALL_CHECKS.contains(&c.as_str()))
    {
        bail!(
            "unknown check: {bad}. Valid checks are: {}",
            ALL_CHECKS.join(", ")
        );
    }
    let home = resolve_home(opts.home.as_deref())?;
    let index_path = match &opts.index_path {
        Some(p) => p.clone(),
        None => home.join(".agent-duster").join("index.db"),
    };
    let mut rep = DoctorReport::default();

    if opts.secrets && selected(opts, CHECK_SECRETS) {
        run_check(&mut rep, CHECK_SECRETS, |f, w| check_secrets(&home, f, w));
    }

    // 三项要读索引。库还没建时它们**没跑**，于是不进 checks_run，
    // 原因写进 warnings——把"没查过"混进"查过没发现"里是这份报告最贵的谎。
    //
    // 一项都没选中就连索引都不开：那条 warning 说的是"这几项被跳过了"，
    // 而用户压根没要它们，报出来是无中生有。
    let wanted: Vec<&str> = [CHECK_SKILL_METADATA, CHECK_DANGLING, CHECK_SQLITE]
        .into_iter()
        .filter(|c| selected(opts, c))
        .collect();
    let rows: Option<Vec<ResourceRecord>> = if wanted.is_empty() {
        None
    } else if index_path.is_file() {
        match load_rows(&index_path, &opts.agents) {
            Ok(r) => Some(r),
            Err(e) => {
                rep.warnings.push(format!(
                    "{}: {e:#}; skipped {}",
                    index_path.display(),
                    wanted.join(", ")
                ));
                None
            }
        }
    } else {
        rep.warnings.push(format!(
            "index database not found: {}; skipped {}. Run `duster scan` first.",
            index_path.display(),
            wanted.join(", ")
        ));
        None
    };

    if let Some(rows) = &rows
        && selected(opts, CHECK_SKILL_METADATA)
    {
        run_check(&mut rep, CHECK_SKILL_METADATA, |f, _| {
            check_skill_metadata(rows, f)
        });
    }

    if selected(opts, CHECK_CONFIG_SYNTAX) {
        run_check(&mut rep, CHECK_CONFIG_SYNTAX, |f, _| {
            check_config_syntax(&home, &opts.agents, f)
        });
    }

    if let Some(rows) = &rows {
        if selected(opts, CHECK_DANGLING) {
            run_check(&mut rep, CHECK_DANGLING, |f, w| {
                check_dangling(rows, &home, &opts.agents, f, w)
            });
        }
        if selected(opts, CHECK_SQLITE) {
            run_check(&mut rep, CHECK_SQLITE, |f, _| {
                check_sqlite_integrity(rows, &index_path, opts.agents.is_empty(), f)
            });
        }
    }

    if opts.ping && selected(opts, CHECK_MCP) {
        run_check(&mut rep, CHECK_MCP, |f, w| {
            check_mcp(&index_path, &opts.agents, f, w)
        });
    }

    Ok(rep)
}

/// 这一项在不在 `--check` 的选择里。空 = 全选，即默认行为。
fn selected(opts: &DoctorOptions, name: &str) -> bool {
    opts.checks.is_empty() || opts.checks.iter().any(|c| c == name)
}

/// 跑一项检查并统一降级：报错变成一条 Error finding + 一条 warning。
///
/// 检查在失败前已经产出的 finding **照样保留**——一项检查在第 900 个文件上
/// 摔了，前 899 个的结论仍然是真的，丢掉它们才是浪费。
fn run_check(
    rep: &mut DoctorReport,
    name: &'static str,
    body: impl FnOnce(&mut Vec<Finding>, &mut Vec<String>) -> Result<()>,
) {
    rep.checks_run.push(name.to_string());
    let mut findings = Vec::new();
    let mut warnings = Vec::new();
    let outcome = body(&mut findings, &mut warnings);
    rep.findings.append(&mut findings);
    rep.warnings.append(&mut warnings);
    if let Err(e) = outcome {
        rep.warnings.push(format!("{name}: {e:#}"));
        rep.findings.push(finding(
            name,
            Severity::Error,
            name.to_string(),
            format!("check failed: {e:#}"),
            None,
        ));
    }
}

/// [`Finding`] 的构造快捷方式。
fn finding(
    check: &str,
    severity: Severity,
    subject: String,
    detail: String,
    fix: Option<&str>,
) -> Finding {
    Finding {
        check: check.to_string(),
        severity,
        subject,
        detail,
        fix: fix.map(str::to_string),
    }
}

/// 只读取出索引里的资源行（按 agent 过滤）。
fn load_rows(index_path: &Path, agents: &[String]) -> Result<Vec<ResourceRecord>> {
    let idx = Index::open_readonly(index_path)
        .with_context(|| format!("failed to open index read-only: {}", index_path.display()))?;
    query::list_resources(
        idx.conn(),
        &ResourceFilter {
            agents: agents.to_vec(),
            kinds: Vec::new(),
            clean_levels: Vec::new(),
        },
    )
}

/// 检查 1：明文凭据。文本扫描 + opencode 的库内列。
fn check_secrets(
    home: &Path,
    findings: &mut Vec<Finding>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let report = scan_secrets(Some(home), &[])?;
    warnings.extend(report.warnings.iter().cloned());
    for h in &report.hits {
        findings.push(finding(
            CHECK_SECRETS,
            Severity::Warn,
            format!("{}:{}", h.path, h.line),
            format!(
                "{} {} = {}",
                h.kind,
                if h.key.is_empty() {
                    "(inline value)"
                } else {
                    h.key.as_str()
                },
                h.masked
            ),
            Some(h.advice.as_str()),
        ));
    }
    check_opencode_db_secrets(home, findings, warnings)
}

/// 采集结论 5 的盲区：opencode 把 token 存进 SQLite 列，逐行扫文本看不见。
///
/// 只读打开、只取值、**只报掩码**。取到的原文除了喂给 [`mask`] 之外
/// 不进任何变量之外的地方：不打印、不入索引、不写文件。
fn check_opencode_db_secrets(
    home: &Path,
    findings: &mut Vec<Finding>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let db = home.join(OPENCODE_DB);
    if !db.is_file() {
        return Ok(());
    }
    let Some(conn) = foreign::open(&db)? else {
        // 读不到就明说。悄悄跳过等于给用户一份看不见这块盲区的报告。
        warnings.push(format!(
            "{}: not readable; credentials stored in its columns were not checked",
            db.display()
        ));
        return Ok(());
    };
    let shown = db.display().to_string();

    // 闭包持有 conn 的共享借用与 findings 的可变借用；本函数其余部分
    // 只做表/列探测（同样是共享借用），不再直接碰 findings。
    let mut emit = |table: &str, id_expr: &str, label_expr: &str, col: &str| -> Result<()> {
        // 表名与列名全部来自本文件的字面量或 foreign::columns 的校验结果，
        // 不含外部输入，拼进 SQL 是安全的。
        let sql = format!(
            "SELECT {id_expr}, {label_expr}, {col} FROM {table} \
             WHERE {col} IS NOT NULL AND {col} <> ''"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id: String = r.get::<_, Option<String>>(0)?.unwrap_or_default();
            let label: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            let value: String = r.get::<_, Option<String>>(2)?.unwrap_or_default();
            if value.is_empty() {
                continue;
            }
            let named = if label.is_empty() {
                String::new()
            } else {
                format!(" ({label})")
            };
            findings.push(finding(
                CHECK_SECRETS,
                Severity::Warn,
                format!("{shown}#{table}.{col}:{id}"),
                format!(
                    "plaintext credential in a SQLite column{named} = {}",
                    mask(&value)
                ),
                Some("revoke it with the provider, then sign in again from opencode"),
            ));
        }
        Ok(())
    };

    for (table, col) in OPENCODE_TOKEN_COLS {
        if !foreign::has_table(&conn, table)? {
            continue;
        }
        let cols = foreign::columns(&conn, table)?;
        if !cols.iter().any(|c| c == col) {
            continue;
        }
        emit(table, &id_expr(&cols), label_expr(&cols), col)?;
    }

    if foreign::has_table(&conn, CREDENTIAL_TABLE)? {
        let cols = foreign::columns(&conn, CREDENTIAL_TABLE)?;
        // `value` 是实测的列名；上游改名了就按名字形状找回来，
        // 免得一次重命名就让整张凭据表从报告里消失。
        let value_cols: Vec<&str> = if cols.iter().any(|c| c == "value") {
            vec!["value"]
        } else {
            cols.iter()
                .filter(|c| looks_secret_column(c))
                .map(String::as_str)
                .collect()
        };
        let id = id_expr(&cols);
        let label = label_expr(&cols);
        for col in value_cols {
            emit(CREDENTIAL_TABLE, &id, label, col)?;
        }
    }
    Ok(())
}

/// 行标识列：有 `id` 用它，否则退回 `rowid`。一律 CAST 成 TEXT——
/// `id` 可能是整数也可能是字符串，取值端只想要一个能显示的东西。
fn id_expr(cols: &[String]) -> String {
    if cols.iter().any(|c| c == "id") {
        "CAST(id AS TEXT)".to_string()
    } else {
        "CAST(rowid AS TEXT)".to_string()
    }
}

/// 人可读的标签列；没有就给个空串占位（`emit` 的第二列必须存在）。
fn label_expr(cols: &[String]) -> &'static str {
    if cols.iter().any(|c| c == "label") {
        "label"
    } else {
        "''"
    }
}

/// 列名看着像不像装凭据的。只在 `value` 缺席时兜底用。
fn looks_secret_column(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["token", "secret", "password", "credential", "apikey"]
        .iter()
        .any(|k| n.contains(k))
        || n == "key"
}

/// 检查 2：skill 目录的 `SKILL.md` 缺失、读不动、或没有 frontmatter `name`。
///
/// 没有 `name` 不是致命错（`duster_adapter::mapper::skill` 会退回目录名），
/// 但那意味着这个 skill 在跨 agent 视图里的身份是目录名——改个目录名它就
/// 变成另一个 skill，copies 与 link 都会跟着走偏。
fn check_skill_metadata(rows: &[ResourceRecord], findings: &mut Vec<Finding>) -> Result<()> {
    for r in rows.iter().filter(|r| r.kind == "skill") {
        let root = Path::new(&r.path);
        // 整个目录都不在了是 dangling-reference 的活，这里不重复报。
        if !root.exists() {
            continue;
        }
        let md = if root.is_dir() {
            root.join("SKILL.md")
        } else {
            root.to_path_buf()
        };
        let subject = md.display().to_string();
        match std::fs::read_to_string(&md) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => findings.push(finding(
                CHECK_SKILL_METADATA,
                Severity::Warn,
                subject,
                "SKILL.md is missing".to_string(),
                Some("add a SKILL.md with a `name:` field in its YAML frontmatter"),
            )),
            Err(e) => findings.push(finding(
                CHECK_SKILL_METADATA,
                Severity::Warn,
                subject,
                format!("SKILL.md is unreadable: {e}"),
                Some("fix the file permissions"),
            )),
            Ok(text) => {
                let named = crate::memory::frontmatter(&text)
                    .and_then(|fm| crate::memory::yaml_scalar(fm, "name"))
                    .is_some();
                if !named {
                    findings.push(finding(
                        CHECK_SKILL_METADATA,
                        Severity::Warn,
                        subject,
                        "SKILL.md has no `name` in its YAML frontmatter; the directory name is \
                         used instead"
                            .to_string(),
                        Some("add a `name:` field to the YAML frontmatter"),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 检查 3：清单声明的 JSON/TOML 资源里，有哪些解析不了。
///
/// JSONC 现在能正常读（注释、尾逗号、BOM 都不再是问题），所以命中这一项
/// 的必然是**真的语法错**，而不是 duster 认不出的方言。
fn check_config_syntax(home: &Path, agents: &[String], findings: &mut Vec<Finding>) -> Result<()> {
    let manifests = manifest::load_all(Some(&home.join(".agent-duster").join("adapters")))?;
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for m in &manifests {
        if !agents.is_empty() && !agents.contains(&m.agent.id) {
            continue;
        }
        for r in &m.resources {
            let p = expand(&r.path, home);
            if !is_structured_config(&p) || !p.is_file() || !seen.insert(p.clone()) {
                continue;
            }
            if let Err(e) = codec::read_file(&p) {
                findings.push(finding(
                    CHECK_CONFIG_SYNTAX,
                    Severity::Error,
                    p.display().to_string(),
                    format!("{e:#}"),
                    Some(
                        "fix the syntax error; duster refuses to read or rewrite a file it \
                         cannot parse",
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// 扩展名是不是 duster 会当结构化配置去解析的那几种。
fn is_structured_config(p: &Path) -> bool {
    p.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "json" | "jsonc" | "toml"))
}

/// 检查 4：悬空引用。两路来源，索引行与磁盘上的断链。
///
/// 遍历 agent 根是这六项里最贵的一步（`~/.claude` 可以有十万个会话文件），
/// 但断链只可能在磁盘上，索引里根本没有它们的行——不走一遍就看不见。
/// 跳过 `node_modules` / `.git` / `dist` 把最坏情况压下来。
fn check_dangling(
    rows: &[ResourceRecord],
    home: &Path,
    agents: &[String],
    findings: &mut Vec<Finding>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let mut reported: BTreeSet<String> = BTreeSet::new();

    for r in rows {
        let p = Path::new(&r.path);
        let detail = match std::fs::symlink_metadata(p) {
            Err(_) => format!("indexed {} no longer exists on disk", r.kind),
            Ok(m) if m.file_type().is_symlink() && std::fs::metadata(p).is_err() => {
                format!("indexed {} is a symlink that resolves nowhere", r.kind)
            }
            Ok(_) => continue,
        };
        if reported.insert(r.path.clone()) {
            findings.push(finding(
                CHECK_DANGLING,
                Severity::Warn,
                r.path.clone(),
                detail,
                Some("duster scan"),
            ));
        }
    }

    let manifests = manifest::load_all(Some(&home.join(".agent-duster").join("adapters")))?;
    let mut roots: BTreeSet<PathBuf> = BTreeSet::new();
    for m in &manifests {
        if !agents.is_empty() && !agents.contains(&m.agent.id) {
            continue;
        }
        for raw in m.probe.any_of.iter().chain(m.probe.all_of.iter()) {
            let p = expand(raw, home);
            if p.is_dir() {
                roots.insert(p);
            }
        }
    }

    let opts = WalkOptions {
        follow_links: false,
        prune_dirs: PRUNE_DIRS.iter().map(|s| (*s).to_string()).collect(),
    };
    for root in &roots {
        // 先收集再逐个 stat：walk_files 的回调跑在并行 worker 上。
        let mut links: Vec<PathBuf> = Vec::new();
        if let Err(e) = walk_files(root, &opts, |p, m| {
            if m.file_type().is_symlink() {
                links.push(p.to_path_buf());
            }
        }) {
            warnings.push(format!("{}: {e}", root.display()));
            continue;
        }
        for l in links {
            let shown = l.display().to_string();
            if std::fs::metadata(&l).is_err() && reported.insert(shown.clone()) {
                findings.push(finding(
                    CHECK_DANGLING,
                    Severity::Warn,
                    shown,
                    "symlink resolves nowhere".to_string(),
                    Some("duster scan"),
                ));
            }
        }
    }
    Ok(())
}

/// 检查 5：每个已索引的 SQLite 资源过一遍 `PRAGMA integrity_check`，
/// 外加 duster 自己的索引库。
///
/// 单个库检查失败（打不开、报出问题）只变成一条 Error finding，
/// 下一个库照查——一块坏盘不该让其余十个库的结论一起丢掉。
///
/// 代价要认：`integrity_check` 是全库校验，本机的 `~/.codex/logs_2.sqlite`
/// 有 822 MB，这一项会真的花掉几秒。这是 doctor 的定位换来的——
/// 它是"体检"不是"看一眼"。
fn check_sqlite_integrity(
    rows: &[ResourceRecord],
    index_path: &Path,
    include_own: bool,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let mut targets: BTreeSet<PathBuf> = rows
        .iter()
        .map(|r| PathBuf::from(&r.path))
        .filter(|p| foreign::is_sqlite(p))
        .collect();
    // 按 agent 过滤时不查 duster 自己的库——那不属于任何一个 agent。
    if include_own && index_path.is_file() {
        targets.insert(index_path.to_path_buf());
    }
    for t in &targets {
        let own = t == index_path;
        let fix = if own {
            Some("delete the index and run `duster scan`; it is a disposable derivative")
        } else {
            Some("restore the file from a backup, or repair it with `sqlite3 .recover`")
        };
        match maintenance::integrity_ok(t) {
            Ok(true) => {}
            Ok(false) => findings.push(finding(
                CHECK_SQLITE,
                Severity::Error,
                t.display().to_string(),
                "PRAGMA integrity_check reported problems".to_string(),
                fix,
            )),
            Err(e) => findings.push(finding(
                CHECK_SQLITE,
                Severity::Error,
                t.display().to_string(),
                format!("integrity check could not run: {e:#}"),
                fix,
            )),
        }
    }
    Ok(())
}

/// 检查 6：MCP 可达性。**只在 `--ping` 时跑**。
///
/// 每个合并后的 server 只 ping 一次：合并的前提就是规格逐字节相同，
/// 按声明处各 spawn 一遍纯属重复烧进程。归属算给第一处匹配的声明。
fn check_mcp(
    index_path: &Path,
    agents: &[String],
    findings: &mut Vec<Finding>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let list = crate::mcp::list(Some(index_path))?;
    warnings.extend(list.warnings.iter().cloned());
    for s in &list.servers {
        let Some(d) = s
            .declared_in
            .iter()
            .find(|d| agents.is_empty() || agents.contains(&d.agent_id))
        else {
            continue;
        };
        let r = crate::mcp::ping(
            &s.spec,
            &d.agent_id,
            &crate::mcp::PingOptions {
                enabled: true,
                ..Default::default()
            },
        );
        if !r.ok {
            findings.push(finding(
                CHECK_MCP,
                Severity::Warn,
                format!("{} ({})", s.name, d.agent_id),
                r.error.unwrap_or_else(|| "handshake failed".to_string()),
                Some("check the command and its PATH, or remove the declaration"),
            ));
        }
    }
    Ok(())
}

/// 把清单里的 `~`/`~/...` 相对给定 home 展开；其余形式原样返回。
///
/// 与 `crate::scan` 里那份同规则：注入了假 home 就绝不回落到真实主目录。
fn expand(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(raw),
    }
}

/// 一处命中。
#[derive(Debug, Clone, Serialize)]
pub struct SecretHit {
    pub path: String,
    /// 1 起的行号。
    pub line: u32,
    /// 命中类型：`api-key` / `bearer-token` / `oauth-refresh` / `high-entropy` 等。
    pub kind: String,
    /// 命中所在的键名（`OPENAI_API_KEY` / `Authorization`），取不到为空串。
    pub key: String,
    /// **掩码后**的值，如 `sk-a…7f2b`。绝不放原文。
    pub masked: String,
    /// 建议动作，如 "revoke at platform.openai.com"。
    pub advice: String,
}

/// 扫描结果。
#[derive(Debug, Clone, Serialize)]
pub struct SecretsReport {
    pub hits: Vec<SecretHit>,
    pub scanned_files: usize,
    pub warnings: Vec<String>,
}

/// 已知明文凭据位置（本机实测，doctor 必命中）：
///
/// `~/.codex/config.toml` 内联 header · `~/.codex/auth.json` · `~/.codex/.env` ·
/// `~/.gemini/google_accounts.json` · `~/.gemini/.env` · `~/.kimi/credentials`
pub fn known_secret_paths(home: &Path) -> Vec<PathBuf> {
    [
        ".codex/config.toml",
        ".codex/auth.json",
        ".codex/.env",
        ".gemini/google_accounts.json",
        ".gemini/.env",
        ".kimi/credentials",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect()
}

/// 单个文件的扫描上限 4 MB。凭据不会藏在 100 MB 的日志里，扫它只是在烧时间；
/// 有上限还能保证 `doctor --secrets` 的耗时不被一个巨型文件拖爆。
const MAX_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// 二进制嗅探窗口：前 8 KiB 内出现 NUL 就判定为二进制。
const SNIFF_BYTES: usize = 8 * 1024;

/// 目录递归时不深入的目录名。
const PRUNE_DIRS: [&str; 3] = ["node_modules", ".git", "dist"];

/// 熵阈值与最短 token 长度（见 [`shannon_entropy`] 的经验值说明）。
const ENTROPY_MIN: f64 = 3.5;
const ENTROPY_TOKEN_MIN: usize = 20;

/// 已知前缀规则的最短 token：`AKIA` + 16 位是最短的真 key（20 字符），
/// 取 12 留足余量，同时挡掉散文里孤零零的 `sk-`。
const PREFIX_TOKEN_MIN: usize = 12;

/// 键名规则要求值至少 9 个字符。
///
/// 不是为了"够长才算密码"，而是 [`mask`] 对 ≤ 8 字符全打码——报出来是一串
/// 省略号，用户既对不上号也没法判断真假，纯噪声。短值几乎全是
/// `"true"` / `"none"` / `"latest"` 这类枚举。
const MIN_SECRET_LEN: usize = 9;

/// 已知值前缀。kind 一律 `api-key`；具体撤销地点由 [`advice_for`] 按形状再分。
const VALUE_PREFIXES: [&str; 8] = [
    "sk-",
    "github_pat_",
    "ghp_",
    "gho_",
    "xoxb-",
    "AKIA",
    "AIza",
    "ya29.",
];

/// 扫描 `roots`（文件或目录）里的明文凭据。
///
/// `roots` 为空时扫 [`known_secret_paths`]。目录递归但跳过
/// `node_modules` / `.git` / `dist`，并跳过二进制文件与超过 4 MB 的文件
/// （凭据不会藏在 100 MB 的日志里，扫它只是在烧时间）。
///
/// 缺文件不是错误——大多数 agent 用户根本没装，静默跳过；读不动的（权限）
/// 进 `warnings`。返回值里的每个值都已经过 [`mask`]。
pub fn scan_secrets(home: Option<&Path>, roots: &[PathBuf]) -> Result<SecretsReport> {
    // ⚠️ 本函数只读:不写任何文件、不建缓存、不碰 index.db、不打印原文。
    // 这是最容易被后来"顺手加个缓存/加条 debug 日志"破坏的性质——凭据一旦
    // 借道派生物(会被同步、被备份)或终端 scrollback 扩散出去就收不回来。
    // 改这里之前先回去读本模块顶部的三条不可协商约束。
    let mut report = SecretsReport {
        hits: Vec::new(),
        scanned_files: 0,
        warnings: Vec::new(),
    };

    let targets: Vec<PathBuf> = if roots.is_empty() {
        known_secret_paths(&resolve_home(home)?)
    } else {
        roots.to_vec()
    };

    let opts = WalkOptions {
        follow_links: false,
        prune_dirs: PRUNE_DIRS.iter().map(|s| (*s).to_string()).collect(),
    };

    for target in &targets {
        // metadata 失败一律当"不存在"处理:大多数 agent 没装,报错只会刷屏。
        let Ok(meta) = std::fs::metadata(target) else {
            continue;
        };
        if meta.is_dir() {
            // 先收集再逐个读:walk_files 的回调里做 IO 会拖住并行遍历的 worker。
            let mut files: Vec<PathBuf> = Vec::new();
            let walked = walk_files(target, &opts, |p, m| {
                if m.is_file() && m.len() <= MAX_SCAN_BYTES {
                    files.push(p.to_path_buf());
                }
            });
            if let Err(e) = walked {
                report.warnings.push(format!("{}: {e}", target.display()));
                continue;
            }
            for f in &files {
                scan_one_file(f, &mut report);
            }
        } else if meta.is_file() && meta.len() <= MAX_SCAN_BYTES {
            scan_one_file(target, &mut report);
        }
    }

    // 目录遍历是并行的,出场顺序不稳定;排序让输出与测试都可复现。
    report
        .hits
        .sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
    Ok(report)
}

/// 确定 home:优先注入值,否则真实用户主目录(经 duster-fs 的 `~` 展开)。
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

/// 读一个文件并逐行检测。二进制/读不动的不计入 `scanned_files`——
/// 那个计数的语义是"真正看过的文件数",虚报会让用户高估覆盖面。
fn scan_one_file(path: &Path, report: &mut SecretsReport) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        // io::Error 只含路径与 errno,不会带出文件内容。
        Err(e) => {
            report.warnings.push(format!("{}: {e}", path.display()));
            return;
        }
    };
    if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return;
    }
    report.scanned_files += 1;

    let text = String::from_utf8_lossy(&bytes);
    let shown = path.display().to_string();
    for (i, line) in text.lines().enumerate() {
        if let Some(h) = scan_line(&shown, i as u32 + 1, line) {
            report.hits.push(h);
        }
    }
}

/// 单行检测:规则 1(已知键名)→ 2(已知值前缀)→ 3(高熵)依次尝试,
/// **先命中的赢,一行最多报一条**——同一行报三遍只会淹没真正的信号。
fn scan_line(path: &str, line_no: u32, line: &str) -> Option<SecretHit> {
    if let Some((kind, key, value)) = match_known_key(line) {
        return Some(new_hit(path, line_no, kind, key, value));
    }
    if let Some(token) = match_value_prefix(line) {
        let kind = if token.starts_with("-----BEGIN") {
            "private-key"
        } else {
            "api-key"
        };
        return Some(new_hit(path, line_no, kind, "", token));
    }
    let token = match_high_entropy(line)?;
    Some(new_hit(path, line_no, "high-entropy", "", token))
}

/// 构造命中。
///
/// **掩码发生在这里,即捕获点**：`SecretHit` 从诞生起就不含原文,
/// 后续任何重构(换序列化、加日志、塞进错误消息)都没有机会把原值带出去。
/// 如果改成"展示时再掩码",泄露就只差一个 `{:?}` 的距离。
fn new_hit(path: &str, line_no: u32, kind: &str, key: &str, value: &str) -> SecretHit {
    SecretHit {
        path: path.to_string(),
        line: line_no,
        kind: kind.to_string(),
        key: key.to_string(),
        masked: mask(value),
        advice: advice_for(path, value).to_string(),
    }
}

/// 规则 1:`key = value` / `"key": "value"` / `key: value` / `export KEY=value`。
///
/// 手写解析而非正则:依赖一个正则引擎只为了认几个键名不划算,而且逐行跑
/// 十来条正则比一次反向扫描慢。做法是遍历行内每个 `=` / `:`,取紧邻左侧的
/// 标识符当键——这样内联表(`headers = { Authorization = "…" }`)和单行 JSON
/// 里的第二、三个键也能命中。
fn match_known_key(line: &str) -> Option<(&'static str, &str, &str)> {
    for (i, c) in line.char_indices() {
        if c != '=' && c != ':' {
            continue;
        }
        let key = key_before(&line[..i]);
        if key.is_empty() {
            continue;
        }
        let Some(kind) = classify_key(key) else {
            continue;
        };
        let value = strip_auth_scheme(value_after(&line[i + c.len_utf8()..]));
        if !plausible_secret(value) {
            continue;
        }
        return Some((kind, key, value));
    }
    None
}

/// 取分隔符左侧的键名:带引号的取引号内(JSON),否则取结尾的一段标识符
/// (`export FOO` → `FOO`,`{ Authorization` → `Authorization`)。
fn key_before(seg: &str) -> &str {
    let s = seg.trim_end();
    if let Some(q) = s.chars().next_back().filter(|c| *c == '"' || *c == '\'') {
        let inner = &s[..s.len() - q.len_utf8()];
        return match inner.rfind(q) {
            Some(open) => &inner[open + q.len_utf8()..],
            None => "",
        };
    }
    let start = s
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .last()
        .map(|(i, _)| i);
    start.map_or("", |i| &s[i..])
}

/// 取分隔符右侧的值:带引号的取引号内,否则到 `,;}]#` 或行尾为止。
fn value_after(seg: &str) -> &str {
    let s = seg.trim_start();
    if let Some(q) = s.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let rest = &s[q.len_utf8()..];
        return rest.find(q).map_or(rest, |e| &rest[..e]);
    }
    let end = s.find([',', ';', '}', ']', '#']).unwrap_or(s.len());
    s[..end].trim_end()
}

/// 剥掉 HTTP 认证方案前缀,让掩码和建议都落在真 token 上
/// （`Authorization = "Bearer sk-…"` 的秘密是 `sk-…`,不是 `Bearer sk-…`）。
///
/// 比较走字节而不是 `&v[..n]`:扫描对象是任意 UTF-8 文本,值可能以多字节
/// 字符开头(`Authorization = "凭据…"`),按字节切 `&str` 会直接 panic。
/// 字节比较通过之后前 n 个字节必然全是 ASCII,那时 n 才是合法 char 边界。
fn strip_auth_scheme(v: &str) -> &str {
    let bytes = v.as_bytes();
    for scheme in ["bearer ", "basic ", "token "] {
        let n = scheme.len();
        if bytes.len() > n && bytes[..n].eq_ignore_ascii_case(scheme.as_bytes()) {
            return v[n..].trim();
        }
    }
    v
}

/// 键名 → 归一化 kind。归一化时抹掉 `_`/`-` 与大小写,
/// 于是 `api_key` / `API-KEY` / `apiKey` 走同一条分支。
/// 顺序从具体到宽泛,先匹配到的赢。
fn classify_key(key: &str) -> Option<&'static str> {
    let n: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if n.is_empty() {
        return None;
    }
    let kind = if n.contains("refreshtoken") {
        "oauth-refresh"
    } else if n.contains("clientsecret") {
        "client-secret"
    } else if n.contains("privatekey") {
        "private-key"
    } else if n.contains("authorization") || n.contains("bearer") {
        "bearer-token"
    } else if n.contains("apikey") {
        "api-key"
    } else if n.contains("token") {
        "token"
    } else if n.contains("password") || n.contains("passwd") {
        "password"
    } else if n.contains("credential") {
        "credential"
    } else if n.contains("secret") {
        "secret"
    } else {
        return None;
    };
    Some(kind)
}

/// 值是否像个真凭据。
///
/// 三道闸门,每道都是实测出来的误报源:
/// - 含空白 → 散文（`The token: a short-lived credential …` 会被键名规则捞到）;
/// - `<…>` / `$VAR` / `${VAR}` → 占位符或环境变量引用,不是凭据本身;
/// - `your` / `example` / `changeme` → 文档模板。
fn plausible_secret(v: &str) -> bool {
    if v.chars().count() < MIN_SECRET_LEN {
        return false;
    }
    if v.chars().any(char::is_whitespace) || !v.chars().any(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    if v.starts_with('<') || v.starts_with('$') {
        return false;
    }
    let lower = v.to_ascii_lowercase();
    !["your", "example", "changeme", "placeholder", "xxxx"]
        .iter()
        .any(|p| lower.contains(p))
}

/// 规则 2:行内任意位置出现已知值前缀。
///
/// 前缀区分大小写（`AKIA`/`AIza` 本就大小写敏感），并要求前缀左侧不是
/// token 字符——否则 `task-list` 里的 `sk-` 会天天误报。
fn match_value_prefix(line: &str) -> Option<&str> {
    // PEM 头单独判:私钥本体在后续行,这一行就是定位标记。
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY-----") {
        return Some(line.trim());
    }
    let mut best: Option<usize> = None;
    for prefix in VALUE_PREFIXES {
        for (i, _) in line.match_indices(prefix) {
            if line[..i].chars().next_back().is_some_and(is_value_char) {
                continue;
            }
            if best.is_none_or(|b| i < b) {
                best = Some(i);
            }
            break; // match_indices 递增,本前缀第一个合法的即最左
        }
    }
    let i = best?;
    let rest = &line[i..];
    let end = rest.find(|c: char| !is_value_char(c)).unwrap_or(rest.len());
    let token = &rest[..end];
    (token.len() >= PREFIX_TOKEN_MIN).then_some(token)
}

/// 规则 3:行内存在长度 ≥ 20 且熵 > 3.5 的随机串。
fn match_high_entropy(line: &str) -> Option<&str> {
    line.split(|c: char| !is_token_char(c)).find(|t| {
        t.len() >= ENTROPY_TOKEN_MIN
            && !looks_like_path(t)
            && looks_random(t)
            && shannon_entropy(t) > ENTROPY_MIN
    })
}

/// 路径与 URL 天然高熵,但它们不是凭据。
///
/// 本机实测(kimi 的 `context.jsonl`、`kimi.json`):每一行的 cwd
/// `/Users/<name>/Documents/Code/miaomiao-camera-app` 都会命中规则 3。
/// 一份满屏都是自己项目路径的凭据报告,用户看一眼就再也不会看第二遍——
/// 这道闸门保护的不是准确率,是这份报告还有没有人读。
///
/// 代价是恰好含两个以上 `/` 的 base64 密钥会被漏掉。可以接受:
/// 规则 1(键名)与规则 2(前缀)才是抓真 key 的主力,规则 3 只是兜底网。
fn looks_like_path(tok: &str) -> bool {
    tok.starts_with('/')
        || tok.starts_with("./")
        || tok.starts_with("../")
        || tok.starts_with("~/")
        || tok.contains("://")
        || tok.matches('/').count() >= 2
}

/// 熵之外再加一道字符类闸门:真随机 token 几乎必然混用大小写或数字。
///
/// 只有一类字符的长串基本都是连字符拼出来的英文
/// (`state-of-the-art-design-system` 熵能到 3.6),放它们过去就是误报风暴。
fn looks_random(tok: &str) -> bool {
    let classes = u8::from(tok.bytes().any(|b| b.is_ascii_lowercase()))
        + u8::from(tok.bytes().any(|b| b.is_ascii_uppercase()))
        + u8::from(tok.bytes().any(|b| b.is_ascii_digit()));
    classes >= 2
}

/// 熵检测的 token 字符集(base64 / base64url)。
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-')
}

/// 前缀规则的 token 字符集:比 [`is_token_char`] 多一个 `.`（`ya29.` 系列)。
fn is_value_char(c: char) -> bool {
    is_token_char(c) || c == '.'
}

/// 建议动作:先看 token 形状(最准),形状认不出来再退回文件出处。
///
/// 文件兜底只认 vendor 自己的 auth 文件——`~/.codex/config.toml` 里的
/// `Authorization` 很可能是第三方 MCP 端点的 token,指错撤销地点比不指更糟。
fn advice_for(path: &str, value: &str) -> &'static str {
    if value.starts_with("sk-ant-") {
        return "revoke at console.anthropic.com";
    }
    if value.starts_with("sk-") {
        return "revoke at platform.openai.com";
    }
    if value.starts_with("ghp_") || value.starts_with("gho_") || value.starts_with("github_pat_") {
        return "revoke at github.com/settings/tokens";
    }
    if value.starts_with("AKIA") {
        return "rotate in AWS IAM";
    }
    if value.starts_with("AIza") || value.starts_with("ya29.") {
        return "revoke in Google Cloud console";
    }
    if path.contains(".codex") && path.ends_with("auth.json") {
        return "revoke at platform.openai.com";
    }
    if path.contains(".gemini") && path.ends_with("google_accounts.json") {
        return "revoke in Google Cloud console";
    }
    "revoke at the issuing platform"
}

/// 掩码：保留前 4 后 4，中间统一替换。长度 ≤ 8 时全部打码。
///
/// 保留首尾是为了让用户能把它和 platform 上的 token 列表对上号；
/// 中间用固定长度的省略而不是按原长打点，免得泄露长度信息。
pub fn mask(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "…".repeat(chars.len().min(8));
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Shannon 熵（bits/char）。高熵短串是随机 token 的特征。
///
/// 阈值由调用方定；经验值 3.5 以上 + 长度 ≥ 20 才值得报，
/// 再低就会把 base64 编码的正常内容一起捞进来。
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    let bytes = s.as_bytes();
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_keeps_head_and_tail_only() {
        assert_eq!(mask("sk-proj-abcdefgh1234"), "sk-p…1234");
        // ≤ 8 字符全打码：5 个字符 → 5 个省略号，不留任何首尾。
        assert_eq!(mask("short"), "…".repeat(5));
    }

    #[test]
    fn entropy_separates_random_from_prose() {
        assert!(shannon_entropy("aaaaaaaaaaaaaaaaaaaa") < 1.0);
        assert!(shannon_entropy("xK9mQ2vL7pR4tW8nB3jH") > 3.5);
    }

    /// 在假 home 下写一个文件（自动建父目录）。**永远不碰真实 `$HOME`。**
    fn put(home: &Path, rel: &str, body: &str) -> PathBuf {
        let p = home.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        p
    }

    /// 测试用的假 token：形状像真的，但不是任何平台上存在的凭据。
    const FAKE_OPENAI: &str = "sk-proj-T3Blbk9JVGhpc0lzQVRlc3RUb2tlbjEyMzQ1Njc4";

    #[test]
    fn 已知位置的_openai_token_被识别并只留首尾() {
        let tmp = tempfile::tempdir().unwrap();
        put(
            tmp.path(),
            ".codex/auth.json",
            &format!("{{\n  \"OPENAI_API_KEY\": \"{FAKE_OPENAI}\"\n}}\n"),
        );

        let r = scan_secrets(Some(tmp.path()), &[]).unwrap();
        assert_eq!(r.hits.len(), 1, "{:?}", r.hits);
        let h = &r.hits[0];
        assert_eq!(h.kind, "api-key");
        assert_eq!(h.key, "OPENAI_API_KEY");
        assert_eq!(h.line, 2);
        assert_eq!(h.advice, "revoke at platform.openai.com");
        assert_eq!(h.masked, mask(FAKE_OPENAI));

        // Debug 覆盖全部字段：原 token 的任意 5 字符窗口都不许出现，
        // 也就是说泄露上限恰好是 mask 保留的前 4 + 后 4。
        let blob = format!("{h:?}");
        let chars: Vec<char> = FAKE_OPENAI.chars().collect();
        for w in chars.windows(5) {
            let piece: String = w.iter().collect();
            assert!(!blob.contains(&piece), "泄露了片段 {piece}");
        }
    }

    #[test]
    fn 内联_authorization_头报出正确行号() {
        let tmp = tempfile::tempdir().unwrap();
        put(
            tmp.path(),
            ".codex/config.toml",
            "# codex config\n\
             model = \"gpt-5\"\n\
             \n\
             [mcp_servers.example.http_headers]\n\
             Authorization = \"Bearer abc123DEFghi456JKLmno789PQR\"\n",
        );

        let r = scan_secrets(Some(tmp.path()), &[]).unwrap();
        assert_eq!(r.hits.len(), 1, "{:?}", r.hits);
        let h = &r.hits[0];
        assert_eq!(h.line, 5);
        assert_eq!(h.kind, "bearer-token");
        assert_eq!(h.key, "Authorization");
        // Bearer 方案前缀已剥掉，掩码落在真 token 上。
        assert_eq!(h.masked, mask("abc123DEFghi456JKLmno789PQR"));
    }

    #[test]
    fn 普通散文不产生误报() {
        let tmp = tempfile::tempdir().unwrap();
        let md = put(
            tmp.path(),
            "notes/design.md",
            "# Cleanup notes\n\
             \n\
             Note: the planner runs in dry-run mode by default.\n\
             The token: a short-lived credential used by the adapter.\n\
             This document describes how the cleanup subsystem removes stale sessions,\n\
             and why every deletion must pass a lock probe before it happens.\n",
        );

        let r = scan_secrets(Some(tmp.path()), &[md]).unwrap();
        assert_eq!(r.scanned_files, 1);
        assert!(r.hits.is_empty(), "{:?}", r.hits);
    }

    #[test]
    fn 二进制文件被跳过且不计入_scanned_files() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("blob.bin");
        let mut body = b"\x7fELF\x00\x01\x02".to_vec();
        body.extend_from_slice(FAKE_OPENAI.as_bytes());
        std::fs::write(&bin, body).unwrap();

        let r = scan_secrets(Some(tmp.path()), &[bin]).unwrap();
        assert_eq!(r.scanned_files, 0);
        assert!(r.hits.is_empty());
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn 目录扫描跳过_node_modules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        put(&root, "app.env", &format!("OPENAI_API_KEY={FAKE_OPENAI}\n"));
        put(
            &root,
            "node_modules/pkg/.env",
            &format!("OPENAI_API_KEY={FAKE_OPENAI}\n"),
        );

        let r = scan_secrets(Some(tmp.path()), &[root]).unwrap();
        assert_eq!(r.scanned_files, 1);
        assert_eq!(r.hits.len(), 1, "{:?}", r.hits);
        assert!(r.hits[0].path.ends_with("app.env"));
        assert!(!r.hits.iter().any(|h| h.path.contains("node_modules")));
    }

    #[test]
    fn 空的假_home_返回空报告() {
        let tmp = tempfile::tempdir().unwrap();
        let r = scan_secrets(Some(tmp.path()), &[]).unwrap();
        assert!(r.hits.is_empty());
        assert_eq!(r.scanned_files, 0);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn 无键名时靠值前缀识别并给出对应撤销地点() {
        let tmp = tempfile::tempdir().unwrap();
        let f = put(
            tmp.path(),
            "notes.txt",
            "curl -H ghp_ABCDefgh1234IJKLmnop5678QRSTuvwx90 https://api.github.com\n\
             aws_id AKIAIOSFODNN7EXAMPLE1\n\
             -----BEGIN OPENSSH PRIVATE KEY-----\n\
             the task-list feature is unrelated\n",
        );

        let r = scan_secrets(Some(tmp.path()), &[f]).unwrap();
        let kinds: Vec<(&str, u32, &str)> = r
            .hits
            .iter()
            .map(|h| (h.kind.as_str(), h.line, h.advice.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("api-key", 1, "revoke at github.com/settings/tokens"),
                ("api-key", 2, "rotate in AWS IAM"),
                ("private-key", 3, "revoke at the issuing platform"),
            ],
            "第 4 行的 task-list 里含 `sk-`，边界检查必须挡住它"
        );
    }

    #[test]
    fn 高熵串兜底命中而低熵长串不命中() {
        let tmp = tempfile::tempdir().unwrap();
        let f = put(
            tmp.path(),
            "misc.log",
            "state-of-the-art-design-system\n\
             opaque xK9mQ2vL7pR4tW8nB3jH5cZ6dY1a done\n",
        );

        let r = scan_secrets(Some(tmp.path()), &[f]).unwrap();
        assert_eq!(r.hits.len(), 1, "{:?}", r.hits);
        assert_eq!(r.hits[0].kind, "high-entropy");
        assert_eq!(r.hits[0].line, 2);
        assert_eq!(r.hits[0].key, "");
        assert_eq!(r.hits[0].masked, mask("xK9mQ2vL7pR4tW8nB3jH5cZ6dY1a"));
    }

    /// 路径不是凭据。本机实测 kimi 的会话文件里每一行 cwd 都会踩中熵规则,
    /// 报告因此变成一屏自己的项目路径。
    #[test]
    fn 文件路径与_url_不算高熵凭据() {
        let tmp = tempfile::tempdir().unwrap();
        let f = put(
            tmp.path(),
            "notes.txt",
            "cwd=/Users/laibu/Documents/Code/miaomiao-camera-app\n\
             repo https://github.com/RUIIIOVO/agent-duster\n\
             ./build/out/Release/bundle-2f9a\n",
        );
        let r = scan_secrets(Some(tmp.path()), &[f]).unwrap();
        assert!(r.hits.is_empty(), "路径/URL 不该报成凭据: {:?}", r.hits);
    }

    /// 回归：任意 UTF-8 内容不得 panic。
    ///
    /// 现场是 `uninstall` 的凭据预检扫到 agent 自带的中文/带重音文本，
    /// `strip_auth_scheme` 按字节切 `&str` 切在了字符中间，CLI 直接 101。
    /// 扫描器的输入是任意文件，**任何按字节索引切 `&str` 的地方都是同一个坑**。
    #[test]
    fn 非_ascii_内容不_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = put(
            tmp.path(),
            "qoder/notes.md",
            "token: ăîșță-记录-凭据占位\n\
             说明：这一行没有任何凭据，只是中文散文。\n\
             authorization = \"Bearer ăbcdefghijklmnop\"\n\
             密码 = \"日本語のパスワードです\"\n\
             key: 🔑🔑🔑🔑🔑🔑🔑🔑🔑🔑\n",
        );

        let r = scan_secrets(Some(tmp.path()), &[f]).unwrap();
        assert_eq!(r.scanned_files, 1);
        // 第 3 行的 Bearer 前缀是 ASCII，剥掉之后值以多字节字符开头。
        let bearer = r.hits.iter().find(|h| h.line == 3).expect("第 3 行应命中");
        assert_eq!(bearer.kind, "bearer-token");
        assert_eq!(bearer.masked, mask("ăbcdefghijklmnop"));
    }

    // ─────────────────── doctor 框架 ───────────────────

    use duster_index::db::Index;
    use duster_index::upsert::{self, ResourceRow};

    /// 往假 home 的索引里塞一条资源行，返回索引路径。
    fn seed_index(home: &Path, kind: &str, key: &str, path: &Path) -> PathBuf {
        let db = home.join(".agent-duster").join("index.db");
        let idx = Index::open(&db).unwrap();
        upsert::upsert_agent(
            idx.conn(),
            &duster_model::AgentInfo {
                id: "fixture".to_string(),
                display_name: "Fixture".to_string(),
                root: home.join(".fixture"),
                version: None,
            },
            0,
        )
        .unwrap();
        upsert::upsert_resource(
            idx.conn(),
            &ResourceRow {
                agent_id: "fixture".to_string(),
                kind: kind.to_string(),
                scope: "global".to_string(),
                key: key.to_string(),
                path: path.display().to_string(),
                size: 0,
                mtime_ns: 0,
                hash_content: None,
                cheap_print: None,
                clean_level: None,
                reclaimable: None,
                install_bytes: None,
            },
        )
        .unwrap();
        // Index 持有单实例写锁，doctor 只读之前必须放手。
        drop(idx);
        db
    }

    fn opts(home: &Path) -> DoctorOptions {
        DoctorOptions {
            index_path: Some(home.join(".agent-duster").join("index.db")),
            home: Some(home.to_path_buf()),
            agents: Vec::new(),
            secrets: false,
            ping: false,
            checks: Vec::new(),
        }
    }

    /// `--check` 是真的不跑，不是跑完再筛：没选中的那几项连 checks_run
    /// 都不进（"跑了没发现"与"没跑"必须分得开），也不产生它们的 warning。
    #[test]
    fn 只跑选中的检查() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        put(home, ".codex/config.toml", "this is = not = toml\n");

        let r = doctor(&DoctorOptions {
            checks: vec![CHECK_CONFIG_SYNTAX.to_string()],
            secrets: true,
            ..opts(home)
        })
        .unwrap();

        assert_eq!(r.checks_run, vec![CHECK_CONFIG_SYNTAX.to_string()]);
        // 索引三项一个都没选中 -> 连"索引不存在"这条 warning 都不该出现：
        // 用户压根没要它们，报"跳过了"是无中生有。
        assert!(
            r.warnings.iter().all(|w| !w.contains("index database")),
            "{:?}",
            r.warnings
        );
    }

    /// 名字打错必须报错并列出候选。默默跑成一份空报告，用户会把
    /// 「我把名字拼错了」读成「一切正常」——那是这个参数最贵的失败模式。
    #[test]
    fn 检查名写错时报错并列出全部候选() {
        let tmp = tempfile::tempdir().unwrap();
        let err = doctor(&DoctorOptions {
            checks: vec!["sqlite_integrity".to_string()],
            ..opts(tmp.path())
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown check: sqlite_integrity"), "{msg}");
        for c in ALL_CHECKS {
            assert!(msg.contains(c), "{msg} 缺 {c}");
        }
    }

    #[test]
    fn 关掉的检查不进_checks_run() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // 埋一个真凭据，证明"没报"是因为没跑而不是因为没有。
        put(
            home,
            ".codex/auth.json",
            "{\"OPENAI_API_KEY\": \"sk-live-ABCDEFGHIJKL1234\"}\n",
        );

        let r = doctor(&opts(home)).unwrap();

        assert!(!r.checks_run.contains(&CHECK_SECRETS.to_string()));
        assert!(!r.checks_run.contains(&CHECK_MCP.to_string()));
        assert!(r.findings.iter().all(|f| f.check != CHECK_SECRETS));
        // 没被开关关掉的那一项照跑。
        assert!(r.checks_run.contains(&CHECK_CONFIG_SYNTAX.to_string()));
        // 索引不存在 -> 依赖它的三项**不进** checks_run，原因进 warnings。
        assert!(!r.checks_run.contains(&CHECK_SQLITE.to_string()));
        assert!(
            r.warnings
                .iter()
                .any(|w| w.contains("index database not found"))
        );
    }

    /// 文本里的与库内列里的都要报，且序列化结果里不许出现任何原文。
    #[test]
    fn secrets_同时覆盖文本与_sqlite_列且只输出掩码() {
        const FILE_TOKEN: &str = "sk-live-FILEabcdefghijkl9876";
        const DB_TOKEN: &str = "oc_access_ZZZZmnopqrstuvwx4321";

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        put(
            home,
            ".codex/auth.json",
            &format!("{{\"OPENAI_API_KEY\": \"{FILE_TOKEN}\"}}\n"),
        );
        let db_path = home.join(OPENCODE_DB);
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        {
            let c = rusqlite::Connection::open(&db_path).unwrap();
            c.execute_batch(&format!(
                "CREATE TABLE account(
                     id TEXT PRIMARY KEY, email TEXT NOT NULL, url TEXT NOT NULL,
                     access_token TEXT NOT NULL, refresh_token TEXT NOT NULL);
                 INSERT INTO account VALUES
                   ('acc1', 'a@b.c', 'https://x', '{DB_TOKEN}', '');"
            ))
            .unwrap();
        }

        let mut o = opts(home);
        o.secrets = true;
        let r = doctor(&o).unwrap();

        assert!(r.checks_run.contains(&CHECK_SECRETS.to_string()));
        let secret_findings: Vec<&Finding> = r
            .findings
            .iter()
            .filter(|f| f.check == CHECK_SECRETS)
            .collect();
        assert!(
            secret_findings
                .iter()
                .any(|f| f.subject.ends_with("auth.json:1")),
            "{secret_findings:?}"
        );
        assert!(
            secret_findings
                .iter()
                .any(|f| f.subject.contains("#account.access_token:acc1")),
            "{secret_findings:?}"
        );
        // 空串的 refresh_token 不该被报——那不是凭据，是"还没登录"。
        assert!(
            !secret_findings
                .iter()
                .any(|f| f.subject.contains("refresh_token"))
        );

        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains(FILE_TOKEN), "原文泄漏进了报告");
        assert!(!json.contains(DB_TOKEN), "库内原文泄漏进了报告");
        assert!(json.contains(&mask(DB_TOKEN)));
    }

    /// JSONC 现在能解析，所以命中 config-syntax 的必然是真语法错。
    #[test]
    fn config_syntax_只报真错不报带注释的_jsonc() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let broken = put(home, ".claude/settings.json", "{\"hooks\": }\n");
        let jsonc = put(
            home,
            ".config/opencode/opencode.jsonc",
            "{\n  // 带注释，还有尾逗号\n  \"mcp\": {},\n}\n",
        );

        let r = doctor(&opts(home)).unwrap();

        assert!(r.checks_run.contains(&CHECK_CONFIG_SYNTAX.to_string()));
        let subjects: Vec<&str> = r
            .findings
            .iter()
            .filter(|f| f.check == CHECK_CONFIG_SYNTAX)
            .map(|f| f.subject.as_str())
            .collect();
        assert!(
            subjects.contains(&broken.display().to_string().as_str()),
            "{subjects:?}"
        );
        assert!(
            !subjects.contains(&jsonc.display().to_string().as_str()),
            "{subjects:?}"
        );
        assert!(
            r.findings
                .iter()
                .any(|f| f.check == CHECK_CONFIG_SYNTAX && f.severity == Severity::Error)
        );
    }

    #[test]
    fn dangling_reference_报出被删掉的索引行并建议重扫() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let gone = put(
            home,
            ".fixture/skills/thing/SKILL.md",
            "---\nname: thing\n---\n",
        );
        let db = seed_index(home, "artifact", "thing", &gone);
        std::fs::remove_file(&gone).unwrap();

        let mut o = opts(home);
        o.index_path = Some(db);
        let r = doctor(&o).unwrap();

        assert!(r.checks_run.contains(&CHECK_DANGLING.to_string()));
        let f = r
            .findings
            .iter()
            .find(|f| f.check == CHECK_DANGLING)
            .expect("应报出悬空索引行");
        assert_eq!(f.subject, gone.display().to_string());
        assert_eq!(f.fix.as_deref(), Some("duster scan"));
        assert_eq!(f.severity, Severity::Warn);
    }

    /// 断链在磁盘上、索引里没有它的行，只有走一遍 agent 根才看得见。
    #[test]
    fn dangling_reference_捡出_agent_根下的断链() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let alive = put(home, ".codex/AGENTS.md", "# a\n");
        let broken = home.join(".codex/skills/ghost");
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(home.join("nowhere"), &broken).unwrap();
        let db = seed_index(home, "memory", "AGENTS.md", &alive);

        let mut o = opts(home);
        o.index_path = Some(db);
        let r = doctor(&o).unwrap();

        assert!(
            r.findings.iter().any(|f| f.check == CHECK_DANGLING
                && f.subject == broken.display().to_string()
                && f.detail.contains("resolves nowhere")),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn skill_metadata_报出缺失与无_name_的_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let no_name = home.join(".fixture/skills/anon");
        std::fs::create_dir_all(&no_name).unwrap();
        std::fs::write(no_name.join("SKILL.md"), "# just a heading\n").unwrap();
        let missing = home.join(".fixture/skills/bare");
        std::fs::create_dir_all(&missing).unwrap();

        let db = home.join(".agent-duster").join("index.db");
        {
            let idx = Index::open(&db).unwrap();
            for (key, p) in [("anon", &no_name), ("bare", &missing)] {
                upsert::upsert_resource(
                    idx.conn(),
                    &ResourceRow {
                        agent_id: "fixture".to_string(),
                        kind: "skill".to_string(),
                        scope: "global".to_string(),
                        key: key.to_string(),
                        path: p.display().to_string(),
                        size: 0,
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
        }

        let r = doctor(&opts(home)).unwrap();

        assert!(r.checks_run.contains(&CHECK_SKILL_METADATA.to_string()));
        let details: Vec<&str> = r
            .findings
            .iter()
            .filter(|f| f.check == CHECK_SKILL_METADATA)
            .map(|f| f.detail.as_str())
            .collect();
        assert_eq!(details.len(), 2, "{details:?}");
        assert!(details.iter().any(|d| d.contains("is missing")));
        assert!(details.iter().any(|d| d.contains("no `name`")));
    }

    /// 一个库炸了只变成一条 Error finding，其余检查照跑到底。
    #[test]
    fn 单项失败降级成_error_finding_其余检查照跑() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let corrupt = home.join(".fixture/logs.sqlite");
        std::fs::create_dir_all(corrupt.parent().unwrap()).unwrap();
        let mut bytes = b"SQLite format 3\0".to_vec();
        bytes.extend(std::iter::repeat_n(0xCDu8, 8192));
        std::fs::write(&corrupt, &bytes).unwrap();
        let db = seed_index(home, "artifact", "logs", &corrupt);

        let mut o = opts(home);
        o.index_path = Some(db);
        let r = doctor(&o).unwrap();

        let bad = r
            .findings
            .iter()
            .find(|f| f.check == CHECK_SQLITE && f.subject == corrupt.display().to_string())
            .expect("损坏的库应报出来");
        assert_eq!(bad.severity, Severity::Error);
        // 其余检查一项没少。
        for c in [
            CHECK_SKILL_METADATA,
            CHECK_CONFIG_SYNTAX,
            CHECK_DANGLING,
            CHECK_SQLITE,
        ] {
            assert!(r.checks_run.contains(&c.to_string()), "{:?}", r.checks_run);
        }
        // duster 自己的索引也在体检范围内，而且它是好的。
        assert!(r.findings.iter().all(|f| !f.subject.ends_with("index.db")));
    }

    /// 跑了没发现，也要在 checks_run 里留名——"查过了"本身就是信息。
    #[test]
    fn 干净的树上每项都留名且零_finding() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let ok = put(home, ".fixture/notes.md", "# fine\n");
        let db = seed_index(home, "memory", "notes", &ok);

        let mut o = opts(home);
        o.index_path = Some(db);
        o.secrets = true;
        let r = doctor(&o).unwrap();

        assert_eq!(
            r.checks_run,
            vec![
                CHECK_SECRETS.to_string(),
                CHECK_SKILL_METADATA.to_string(),
                CHECK_CONFIG_SYNTAX.to_string(),
                CHECK_DANGLING.to_string(),
                CHECK_SQLITE.to_string(),
            ]
        );
        assert!(r.findings.is_empty(), "{:?}", r.findings);
    }

    /// `--ping` 的端到端：从假 home 扫出一条 MCP 声明，ping 一个根本不存在的
    /// 二进制，应得到一条 Warn 而不是让整轮体检倒下。
    #[test]
    fn mcp_reachability_报出起不来的声明() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        put(
            home,
            ".claude.json",
            "{\"mcpServers\": {\"ghost\": \
             {\"command\": \"duster-no-such-binary-9f2c\", \"args\": []}}}\n",
        );
        let index_path = home.join(".agent-duster").join("index.db");
        crate::scan::scan(&crate::scan::ScanOptions {
            home: Some(home.to_path_buf()),
            index_path: Some(index_path.clone()),
            full: true,
        })
        .unwrap();

        let mut o = opts(home);
        o.index_path = Some(index_path);
        o.ping = true;
        let r = doctor(&o).unwrap();

        assert!(r.checks_run.contains(&CHECK_MCP.to_string()));
        let f = r
            .findings
            .iter()
            .find(|f| f.check == CHECK_MCP)
            .expect("起不来的 server 应报出来");
        assert!(f.subject.starts_with("ghost "), "{}", f.subject);
        assert_eq!(f.severity, Severity::Warn);
    }
}
