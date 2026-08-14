//! 一份**只读**的健康报告：[`self_check`] 查 duster 自己。
//!
//! `duster doctor` 与 `brew doctor` / `flutter doctor` 同义：报告的对象是
//! 这个工具本身（索引库、适配器清单、home 与导出目录、版本），不是它管着
//! 的那一堆 agent 数据。查**你的 agent 有什么毛病**的那一类检查（明文凭据、
//! 断链、坏配置、MCP 起不起得来）已随上一轮改造整体撤下——`duster status`
//! 不再捎带它们，退出码也只留 0/失败。
//!
//! 唯一的例外是 [`check_sqlite`]：**agent 的 SQLite 库读不读得动**留在自检里。
//! 归属理由是它查的是**环境事实**，不查无从得知——opencode 的会话在
//! `opencode.db` 里、omp 的在 `history.db` 里、codex 的记忆在
//! `memories_1.sqlite` 里，库一坏，`session show` / `memory list` 当场全瞎，
//! 而 duster 不可能知道它坏了，除非真的打开看一眼。这与检查 adapters / index
//! 同属「这台机器现在什么样」，所以归自检，不归任何 `status` 旗标——
//! `status` 已经没有旗标了。
//!
//! [`self_check`] **绝不建库、绝不写任何东西**——「这台机器上还没有索引」
//! 正是它要报告的现状之一，顺手建一个就把结论抹掉了；何况一份会改变现状的
//! 自检报告，用户下次就不敢跑它了。
//!
//! # 一条框架层的规矩
//!
//! **单项失败绝不中断整轮**。一项检查报错就变成一条 [`Severity::Error`] 的
//! [`Finding`] 外加一条 warning，其余照跑——体检报告死在第一个问题上就不是
//! 报告了。[`DoctorReport::checks_run`] 只记真的跑过的：跑了没发现是信息，
//! 没跑却装作跑过是撒谎。
//!
//! # secrets：明文凭据扫描
//!
//! [`scan_secrets`] 不再属于任何体检命令——上一轮把它连同整个他检一起撤了。
//! 它现在还活着，是因为 `duster uninstall` 的预检要靠它：删掉的文件里如果
//! 躺着明文凭据，光删文件不撤销 token 等于没卸干净，预检先扫一遍、把命中
//! 报出来让用户去 platform 撤销。三条不可协商的约束不变：
//!
//! 1. **掩码输出**。命中的值一律只显示前 4 后 4，中间打码。
//!    一个扫描凭据的工具把凭据原样打进终端，等于自己变成了泄露源
//!    （终端有 scrollback、CI 有日志、截图会发群里）。
//! 2. **结果不落盘、不入索引**。`index.db` 是可丢弃的派生物，会被同步、
//!    会被备份；凭据不能借道它扩散到别处。
//! 3. **只报告，不修改**。撤销 token 必须去 platform 做，duster 报出
//!    "在哪、是什么、建议去哪撤销"就到此为止。
//!
//! 判据：已知位置（[`known_secret_paths`]）优先——那是实测出来的确定命中点；
//! 熵检测作为补充，用于捞出未知位置里的高熵串，宁可多报几条让用户自己看。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use duster_adapter::manifest;
use duster_fs::walk::{WalkOptions, walk_files};
use duster_index::db::Index;
use duster_index::{foreign, maintenance, schema};

/// 一条发现的严重程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// 值得知道，但不用做什么。
    Info,
    /// 该看一眼。
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
    /// 出问题的东西：文件路径、库路径、或目录。
    pub subject: String,
    /// 人话描述。
    pub detail: String,
    /// 可照做的修法；没有明确修法为 None。
    pub fix: Option<String>,
}

/// 一轮自检的累加器。整体直接进 `--json` 信封的 `data`（自检报告在
/// [`SelfReport`] 里再包一层版本/索引位置）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct DoctorReport {
    pub findings: Vec<Finding>,
    /// **真的跑过**的检查名，按执行顺序。
    ///
    /// 跑了没发现的检查也在这里——那是信息（"这一项我查过了"）。
    /// 前置条件缺失而没跑的**不在**这里，原因进
    /// [`DoctorReport::warnings`]。
    pub checks_run: Vec<String>,
    pub warnings: Vec<String>,
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

/// 把路径包成可直接粘贴执行的 shell 参数。
///
/// 路径必须是绝对的：用户很可能贴在别的 cwd 下执行，相对路径会作用到别处去；
/// 含空白或 shell 元字符的用单引号包一层，保证整条命令原样粘贴就能跑。
fn shell_arg(p: &Path) -> String {
    let s = p.to_string_lossy();
    let needs_quotes = s
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '\\' | '$' | '`'));
    if needs_quotes {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.into_owned()
    }
}

/// 断链的修法：一条可直接粘贴执行的 `rm <绝对路径>`。
///
/// duster 自己永不代删——医生开药方，不动手术。
fn rm_fix(p: &Path) -> String {
    format!("rm {}", shell_arg(p))
}

/// 目录写不进去的修法。同样只给命令、不代改权限位：改别人 home 底下的
/// 权限是能把人锁在门外的操作，得由他自己按那一下回车。
fn chmod_fix(p: &Path) -> String {
    format!("chmod u+w {}", shell_arg(p))
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

// ─────────────────── 自检：duster 自己 ───────────────────

/// 自检的检查名。他检那六个已经整体撤下（见模块文档），只剩这一张表；
/// 名字写在这里一处，`checks_run` 与每条 [`Finding::check`] 共用同一份字面量，
/// 报告里的分组顺序也不会走散。
const CHECK_INDEX_DB: &str = "index-db";
const CHECK_ADAPTERS: &str = "adapters";
const CHECK_HOME: &str = "home";
const CHECK_SQLITE: &str = "sqlite";
const CHECK_VERSION: &str = "version";

/// 自检的全部检查名，**按 [`self_check`] 的执行顺序**。
pub const SELF_CHECKS: [&str; 5] = [
    CHECK_INDEX_DB,
    CHECK_ADAPTERS,
    CHECK_HOME,
    CHECK_SQLITE,
    CHECK_VERSION,
];

/// 一轮自检的结果。整体直接进 `--json` 信封的 `data`。
#[derive(Debug, Clone, Default, Serialize)]
pub struct SelfReport {
    pub findings: Vec<Finding>,
    /// **真的跑过**的检查名，按执行顺序；口径同 [`DoctorReport::checks_run`]。
    pub checks_run: Vec<String>,
    pub warnings: Vec<String>,
    /// duster 自己的版本。
    pub version: String,
    /// 索引库该在的位置——**不管它在不在**。报告要能回答「我该去哪找它」。
    pub index_path: String,
    /// 索引库字节数；`None` = 库还不存在，那不是错误（见 [`self_check`]）。
    pub index_bytes: Option<u64>,
}

/// 跑一轮**自检**：duster 自己装好没有。
///
/// 这才是 `duster doctor` 该做的事，与 `brew doctor` / `flutter doctor` 同义：
/// 报告的对象是这个工具本身（索引库、适配器清单、home 与导出目录、版本、
/// 以及 agent 的 SQLite 库读不读得动——见 [`check_sqlite`] 的归属理由）。
///
/// **绝不建库、绝不写任何东西。**「这台机器上还没有索引」正是自检要报告的
/// 现状之一，顺手建一个就把结论抹掉了；何况一份会改变现状的自检报告，
/// 用户下次就不敢跑它了。
///
/// `index_path` 同时指明了「这是谁的机器」：库按约定住在
/// `<home>/.agent-duster/index.db`，反推出的 home 决定 `adapters` 与 `home`
/// 两项查哪里，口径与 [`crate::freshness::ensure_fresh`] 一致。形状对不上
/// （测试夹具、从别的机器拷回来的库）才回落到真实主目录。
pub fn self_check(index_path: Option<&Path>) -> Result<SelfReport> {
    let path = match index_path {
        Some(p) => p.to_path_buf(),
        None => duster_fs::path::expand_tilde("~/.agent-duster/index.db"),
    };
    let home = match crate::freshness::home_of_index(&path) {
        Some(h) => h,
        None => resolve_home(None)?,
    };

    // 借 DoctorReport 当累加器：自检与撤掉前的他检共用同一条框架规矩
    // （单项失败降级成一条 Error finding、其余照跑），没必要再写一遍 run_check。
    let mut acc = DoctorReport::default();
    let mut bytes: Option<u64> = None;
    run_check(&mut acc, CHECK_INDEX_DB, |f, _| {
        check_index_db(&path, &mut bytes, f)
    });
    run_check(&mut acc, CHECK_ADAPTERS, |f, _| check_adapters(&home, f));
    run_check(&mut acc, CHECK_HOME, |f, _| check_home(&home, f));
    run_check(&mut acc, CHECK_SQLITE, |f, _| check_sqlite(&home, f));
    run_check(&mut acc, CHECK_VERSION, |f, _| check_version(f));

    Ok(SelfReport {
        findings: acc.findings,
        checks_run: acc.checks_run,
        warnings: acc.warnings,
        version: env!("CARGO_PKG_VERSION").to_string(),
        index_path: path.display().to_string(),
        index_bytes: bytes,
    })
}

/// 自检 1：索引库。在不在、多大、schema 是不是这一代、内容坏没坏。
///
/// **库不存在不是错误，是一条 info。**[`crate::freshness::ensure_fresh`]
/// 会在任何一条命令里顺手把它建出来，用户根本不需要知道「索引」这个词；
/// 报成 error 等于给一件已经自动化掉的事发工单。
///
/// 不去探 `.lock` 到底有没有被人占着：探法只有一种——自己去抢一次
/// `BEGIN EXCLUSIVE`——而写锁的 `busy_timeout` 是 0，隔壁正在跑的
/// `duster scan` 会当场撞锁失败。一条只读的诊断命令不该有本事弄挂一条
/// 正在干活的命令，所以这里只看那个文件在不在。
fn check_index_db(path: &Path, bytes: &mut Option<u64>, findings: &mut Vec<Finding>) -> Result<()> {
    if !path.is_file() {
        findings.push(finding(
            CHECK_INDEX_DB,
            Severity::Info,
            path.display().to_string(),
            "nothing has been indexed yet; any duster command builds the index automatically"
                .to_string(),
            None,
        ));
        // 有锁旁路却没有库：上一次首建卡在抢锁那一步。它不占任何东西、
        // 下次照样能建，但它是那次失败留下的唯一痕迹，值得说一句。
        let lock = lock_sidecar(path);
        if lock.is_file() {
            let fix = rm_fix(&lock);
            findings.push(finding(
                CHECK_INDEX_DB,
                Severity::Info,
                lock.display().to_string(),
                "stray write-lock sidecar with no index database; a previous first build \
                 failed before creating it"
                    .to_string(),
                Some(fix.as_str()),
            ));
        }
        return Ok(());
    }

    let meta = std::fs::metadata(path)
        .with_context(|| format!("failed to stat index database: {}", path.display()))?;
    *bytes = Some(meta.len());

    // 自己的库坏了不用去翻备份：它是可丢弃的派生物，删掉重扫就回来了。
    let own_fix = "delete the index and run `duster scan`; it is a disposable derivative";
    match schema_version(path) {
        Ok(v) if v < schema::SCHEMA_VERSION => findings.push(finding(
            CHECK_INDEX_DB,
            Severity::Warn,
            path.display().to_string(),
            format!(
                "index schema is v{v}, this duster writes v{}",
                schema::SCHEMA_VERSION
            ),
            Some("duster scan --full"),
        )),
        Ok(v) if v > schema::SCHEMA_VERSION => findings.push(finding(
            CHECK_INDEX_DB,
            Severity::Error,
            path.display().to_string(),
            format!(
                "index schema is v{v}, newer than this duster understands (v{}) — \
                 a newer duster wrote it",
                schema::SCHEMA_VERSION
            ),
            Some("upgrade duster, or delete the index and let it rebuild"),
        )),
        Ok(_) => {}
        Err(e) => findings.push(finding(
            CHECK_INDEX_DB,
            Severity::Error,
            path.display().to_string(),
            format!("cannot read the index schema version: {e:#}"),
            Some(own_fix),
        )),
    }

    match maintenance::integrity_ok(path) {
        Ok(true) => {}
        Ok(false) => findings.push(finding(
            CHECK_INDEX_DB,
            Severity::Error,
            path.display().to_string(),
            "PRAGMA integrity_check reported problems".to_string(),
            Some(own_fix),
        )),
        Err(e) => findings.push(finding(
            CHECK_INDEX_DB,
            Severity::Error,
            path.display().to_string(),
            format!("integrity check could not run: {e:#}"),
            Some(own_fix),
        )),
    }
    Ok(())
}

/// 单实例写锁的旁路库：`<db>.lock`，口径见 `duster_index::db::Index::open`。
fn lock_sidecar(index_path: &Path) -> PathBuf {
    let mut p = index_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// 只读取出库上的 `PRAGMA user_version`，也就是 schema 的代数。
fn schema_version(path: &Path) -> Result<i64> {
    let idx = Index::open_readonly(path)?;
    let v: i64 = idx
        .conn()
        .query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(v)
}

/// 自检 2：适配器清单。内置的一份份、用户自己塞进
/// `<home>/.agent-duster/adapters` 的一份份。
///
/// 逐个文件解析而不是直接 `manifest::load_all`：后者撞上第一份坏文件就整体
/// 报错，用户手上有三份自定义清单时只会被告知其中一份的名字，修完再跑才
/// 知道还有下一份。**用户手写的 toml 是这整份自检里唯一他能自己动手修的
/// 东西**，一次把话说全才对得起这一项。
fn check_adapters(home: &Path, findings: &mut Vec<Finding>) -> Result<()> {
    let builtin = manifest::load_builtin().len();
    let dir = home.join(".agent-duster").join("adapters");
    let mut loaded = 0usize;
    if dir.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("failed to read adapter directory: {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        // 排序只为让两次运行的报告能直接 diff：read_dir 的顺序由文件系统定。
        files.sort();
        for f in &files {
            match manifest::load_user_file(f) {
                Ok(_) => loaded += 1,
                Err(e) => findings.push(finding(
                    CHECK_ADAPTERS,
                    Severity::Error,
                    f.display().to_string(),
                    format!("{e:#}"),
                    Some("fix the manifest, or move it out of the adapters directory"),
                )),
            }
        }
    }
    findings.push(finding(
        CHECK_ADAPTERS,
        Severity::Info,
        dir.display().to_string(),
        format!("{builtin} built-in adapters, {loaded} user manifests"),
        None,
    ));
    Ok(())
}

/// 自检 3：home，以及 duster 迟早要往里写东西的那两个目录。
///
/// HOME 定不下来时 duster 的每一条命令都无从谈起（索引、清单、导出全挂在
/// 它底下），所以这里报 error 而不是 warn。
fn check_home(home: &Path, findings: &mut Vec<Finding>) -> Result<()> {
    if !home.is_dir() {
        findings.push(finding(
            CHECK_HOME,
            Severity::Error,
            home.display().to_string(),
            "home directory does not exist; duster keeps its index, adapter manifests \
             and exports under it"
                .to_string(),
            Some("set HOME to an existing directory"),
        ));
        return Ok(());
    }
    check_writable(
        &home.join(".agent-duster"),
        "duster state directory",
        findings,
    );
    check_writable(
        &home.join("agent-duster-exports"),
        "export directory",
        findings,
    );
    Ok(())
}

/// 一个 duster 迟早要往里写东西的目录，现在写不写得进去。
///
/// 目录还不存在时改看父目录：真问题是「建得出来吗」而不是「在不在」——
/// `~/agent-duster-exports` 本来就是第一次导出时才建，报它不存在纯是噪声。
fn check_writable(dir: &Path, what: &str, findings: &mut Vec<Finding>) {
    if dir.exists() && !dir.is_dir() {
        let fix = rm_fix(dir);
        findings.push(finding(
            CHECK_HOME,
            Severity::Error,
            dir.display().to_string(),
            format!("{what} is occupied by something that is not a directory"),
            Some(fix.as_str()),
        ));
        return;
    }
    let (target, missing) = if dir.is_dir() {
        (dir, false)
    } else {
        match dir.parent() {
            Some(p) => (p, true),
            None => return,
        }
    };
    if writable(target) {
        return;
    }
    let fix = chmod_fix(target);
    findings.push(finding(
        CHECK_HOME,
        Severity::Error,
        target.display().to_string(),
        if missing {
            format!("{what} does not exist yet and this directory is not writable")
        } else {
            format!("{what} is not writable")
        },
        Some(fix.as_str()),
    ));
}

/// 当前用户写不写得进这个目录。
///
/// 只看属主写位。duster 碰的目录全在用户自己的 home 底下，真会发生的是
/// `chmod 500 ~/.agent-duster`（或从备份里恢复出一份坏权限），属主那一位
/// 就是决定权所在；为了「目录属于别人」这种在自己家里根本不成立的情形
/// 去引一个 libc 依赖，不划算。
fn writable(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(dir)
        .map(|m| m.permissions().mode() & 0o200 != 0)
        .unwrap_or(false)
}

/// 自检 4：agent 的 SQLite 库读不读得动。
///
/// 归属理由写在模块文档里：agent 库坏了是**环境事实**，不查无从得知——
/// opencode 的会话在 `opencode.db` 里、omp 的在 `history.db` 里、codex 的
/// 记忆在 `memories_1.sqlite` 里，库一坏 `session show` / `memory list`
/// 当场全瞎，而 duster 只有打开看一眼才知道。这与 adapters / index 两项
/// 同属「这台机器现在什么样」，所以留在自检里，不归 `status`——status
/// 已经没有旗标了。
///
/// 枚举源用**清单**（adapters 目录）而不是索引：索引库可能还没建，而自检
/// 绝不建库（见 [`self_check`]）；清单在任何一台装过 agent 的机器上都存在，
/// 声明即事实。同一个库可能被多家声明（`cc-switch.db` 谁都在用），按路径
/// 去重，但报错带上 agent id——用户得知道「谁的会话读不出来了」。
///
/// 判坏用 `PRAGMA quick_check`（`duster_index::maintenance::quick_check_ok`）：
/// 它是完整性检查的快版，几个 GB 的库也扛得住；打不开（被独占、加密、
/// 损坏到开不动）与 check 报了问题同样判坏——对「能不能读」这个结论而言，
/// 两者没有区别。
fn check_sqlite(home: &Path, findings: &mut Vec<Finding>) -> Result<()> {
    let manifests = manifest::load_all(Some(&home.join(".agent-duster").join("adapters")))?;
    let mut by_path: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for m in &manifests {
        for r in &m.resources {
            let p = expand(&r.path, home);
            if foreign::is_sqlite(&p) {
                by_path.entry(p).or_default().push(m.agent.id.clone());
            }
        }
    }
    let fix = Some("restore the file from a backup, or repair it with `sqlite3 .recover`");
    for (path, agents) in by_path {
        // 声明了却还没落盘的库（agent 装了没跑过）不算坏——「不存在」
        // 是别的检查（悬空引用那一套）的话题，这里只判"存在却读不动"。
        if !path.is_file() {
            continue;
        }
        let who = format!("agent {} ", agents.join(", "));
        match maintenance::quick_check_ok(&path)? {
            Some(true) => {}
            Some(false) => findings.push(finding(
                CHECK_SQLITE,
                Severity::Error,
                path.display().to_string(),
                format!("database is corrupted ({who}sessions/memory will be unreadable)"),
                fix,
            )),
            None => findings.push(finding(
                CHECK_SQLITE,
                Severity::Error,
                path.display().to_string(),
                format!(
                    "database is corrupted or unreadable ({who}sessions/memory will be unreadable)"
                ),
                fix,
            )),
        }
    }
    Ok(())
}

/// 自检 5：版本。**恒有一条输出。**
///
/// 别的几项都可能一条不报——那正是健康的样子。可一份一条都没有的报告，
/// 与「根本没查」长得一模一样，那是这类工具最容易骗人的地方（同一条道理
/// 见模块文档的框架规矩）。这一项负责让自检永远至少落下一行事实：
/// duster 是哪个版本、索引 schema 是第几代。
fn check_version(findings: &mut Vec<Finding>) -> Result<()> {
    findings.push(finding(
        CHECK_VERSION,
        Severity::Info,
        format!("agent-duster {}", env!("CARGO_PKG_VERSION")),
        format!("index schema v{}", schema::SCHEMA_VERSION),
        None,
    ));
    Ok(())
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
/// 有上限还能保证 secrets 扫描的耗时不被一个巨型文件拖爆。
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

    /// 库不存在是 **info** 而不是 error：`freshness::ensure_fresh` 会在任何
    /// 一条命令里顺手把它建出来，报成错等于给一件已经自动化掉的事发工单。
    /// 顺带钉死自检的底线——它一个字节都不许写。
    #[test]
    fn 自检_索引不存在时报_info_而不是_error() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join(".agent-duster").join("index.db");

        let r = self_check(Some(&index)).unwrap();

        assert_eq!(r.index_bytes, None, "库不在就没有体积可报");
        assert_eq!(r.index_path, index.display().to_string());
        let f = r
            .findings
            .iter()
            .find(|f| f.check == CHECK_INDEX_DB)
            .expect("该有一条说明库还不在");
        assert_eq!(f.severity, Severity::Info);
        assert!(
            r.findings.iter().all(|f| f.severity != Severity::Error),
            "{:?}",
            r.findings
        );
        assert!(!index.exists(), "自检绝不建库");
        assert!(!index.parent().unwrap().exists(), "连目录都不许建");
    }

    /// 用户自己塞进 adapters 的坏 toml，是这份自检里唯一他能自己动手修的
    /// 东西：每一份都要点名，还要说清错在哪。撞上第一份就整体报错的话，
    /// 他得修一个跑一次、修一个跑一次。
    #[test]
    fn 自检_坏的适配器清单逐个点名并说清错在哪() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // 一份语法就不对，一份语法对但缺 [probe] 段——两种坏法都要抓到。
        put(
            home,
            ".agent-duster/adapters/a-syntax.toml",
            "id = = \"no\"\n",
        );
        put(
            home,
            ".agent-duster/adapters/b-schema.toml",
            "[agent]\nid = \"demo\"\ndisplay_name = \"Demo\"\n",
        );

        let r = self_check(Some(&home.join(".agent-duster").join("index.db"))).unwrap();

        let bad: Vec<&Finding> = r
            .findings
            .iter()
            .filter(|f| f.check == CHECK_ADAPTERS && f.severity == Severity::Error)
            .collect();
        assert_eq!(bad.len(), 2, "两份都要点名：{bad:?}");
        assert!(
            bad[0].subject.ends_with("a-syntax.toml"),
            "{}",
            bad[0].subject
        );
        assert!(
            bad[1].subject.ends_with("b-schema.toml"),
            "{}",
            bad[1].subject
        );
        // 只说一句"解析不了"等于没说；错在哪必须落进 detail。
        assert!(bad[1].detail.contains("probe"), "{}", bad[1].detail);
        assert!(bad.iter().all(|f| f.fix.is_some()));
    }

    /// `<home>/.agent-duster` 不可写 → error，并给一条能直接粘贴的修法。
    /// 索引、清单、快照全在这个目录底下，它写不进去就没有一条命令能收尾。
    #[test]
    fn 自检_state_目录不可写时报_error() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join(".agent-duster");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).unwrap();

        let r = self_check(Some(&state.join("index.db"))).unwrap();

        // 先复权：TempDir 的 Drop 删不掉一个不可写的目录，断言失败时也不能漏。
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();

        let home_findings: Vec<&Finding> = r
            .findings
            .iter()
            .filter(|f| f.check == CHECK_HOME)
            .collect();
        // 导出目录只是还不存在，不该跟着报——「建得出来吗」才是那一项的问题。
        assert_eq!(home_findings.len(), 1, "{home_findings:?}");
        let f = home_findings[0];
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.subject, state.display().to_string());
        let fix = f.fix.as_deref().expect("要给修法");
        assert!(fix.starts_with("chmod u+w "), "{fix}");
        assert!(fix.contains(&state.display().to_string()), "{fix}");
    }

    /// version 这一项**恒有输出**。一份「什么都没发现」的自检报告与
    /// 「根本没查」长得一模一样时就没有价值了；这一项负责让前者永远
    /// 不可能发生。
    #[test]
    fn 自检_version_项恒有输出() {
        let tmp = tempfile::tempdir().unwrap();
        let r = self_check(Some(&tmp.path().join(".agent-duster/index.db"))).unwrap();

        // 五项一项不少地跑过，顺序即 SELF_CHECKS。
        let ran: Vec<String> = SELF_CHECKS.iter().map(|c| c.to_string()).collect();
        assert_eq!(r.checks_run, ran);

        let v = r
            .findings
            .iter()
            .find(|f| f.check == CHECK_VERSION)
            .expect("version 恒有一条");
        assert_eq!(v.severity, Severity::Info);
        assert!(
            v.subject.contains(env!("CARGO_PKG_VERSION")),
            "{}",
            v.subject
        );
        assert!(
            v.detail.contains(&format!("v{}", schema::SCHEMA_VERSION)),
            "{}",
            v.detail
        );
        assert_eq!(r.version, env!("CARGO_PKG_VERSION"));
    }

    /// 库在盘上、schema 是这一代：index-db 跑过却一条不报，体积照样进报告。
    /// 「查过、没事」与「压根没查」在结构上就得分得开。
    #[test]
    fn 自检_健康的索引不报问题但记下体积() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".agent-duster").join("index.db");
        drop(Index::open(&db).unwrap());

        let r = self_check(Some(&db)).unwrap();

        assert!(r.checks_run.contains(&CHECK_INDEX_DB.to_string()));
        assert!(
            r.findings.iter().all(|f| f.check != CHECK_INDEX_DB),
            "{:?}",
            r.findings
        );
        assert!(r.index_bytes.is_some_and(|b| b > 0), "{:?}", r.index_bytes);
    }

    /// 旧代 schema → warn 加一条能直接敲的重扫命令。不是 error：库还读得动，
    /// 只是解析规则换过代，重扫一遍就对齐了。
    #[test]
    fn 自检_旧代_schema_报_warn_并给出重扫命令() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join(".agent-duster").join("index.db");
        drop(Index::open(&db).unwrap());
        // 把 user_version 倒回上一代：这正是「用旧 duster 建过的库」的样子。
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION - 1)
            .unwrap();
        drop(conn);

        let r = self_check(Some(&db)).unwrap();

        let f = r
            .findings
            .iter()
            .find(|f| f.check == CHECK_INDEX_DB)
            .expect("旧库该报一条");
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.fix.as_deref(), Some("duster scan --full"));
    }

    /// 清单声明的 sqlite 库被人为写坏（截断）时，自检要报 corrupted，
    /// 并点名是哪个 agent 的会话/记忆会读不出来。
    ///
    /// 归属理由：agent 库坏了是**环境事实**，不查无从得知——库一坏，
    /// `session show` / `memory list` 当场全瞎，而 duster 只有打开看一眼
    /// 才知道。这与检查 adapters / index 同类，所以归自检。
    #[test]
    fn 自检_坏的_agent_sqlite_库报_corrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        // 清单声明一条 sqlite 资源（形状照抄 cc-switch.toml 的声明）。
        put(
            home,
            ".agent-duster/adapters/fixture.toml",
            "[agent]\n\
             id = \"fixture\"\n\
             display_name = \"Fixture\"\n\
             \n\
             [probe]\n\
             any_of = [\"~/.fixture\"]\n\
             \n\
             [[resource]]\n\
             kind = \"memory\"\n\
             scope = \"global\"\n\
             path = \"~/.fixture/memories.sqlite\"\n\
             mapper = \"stats-only\"\n",
        );

        // 文件头是 SQLite 魔数、内容是垃圾：is_sqlite 认它，open 读不出来。
        let db_file = home.join(".fixture/memories.sqlite");
        std::fs::create_dir_all(db_file.parent().unwrap()).unwrap();
        let mut bytes = b"SQLite format 3\0".to_vec();
        bytes.extend(std::iter::repeat_n(0xABu8, 8192));
        std::fs::write(&db_file, &bytes).unwrap();

        let index = home.join(".agent-duster").join("index.db");
        let r = self_check(Some(&index)).unwrap();

        assert!(r.checks_run.contains(&CHECK_SQLITE.to_string()));
        let f = r
            .findings
            .iter()
            .find(|f| f.check == CHECK_SQLITE)
            .expect("写坏的库应报出来");
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.subject, db_file.display().to_string());
        assert!(f.detail.contains("corrupted"), "{}", f.detail);
        assert!(f.detail.contains("fixture"), "要点名是哪个 agent: {}", f.detail);
        assert!(f.detail.contains("sessions/memory will be unreadable"), "{}", f.detail);
        assert!(f.fix.is_some());

        // 自检绝不建库：这个 home 里除了我们亲手写的清单与坏库，什么都没有。
        assert!(!index.exists());
    }

}
