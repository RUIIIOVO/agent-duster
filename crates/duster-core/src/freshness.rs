//! 索引新鲜度:让「索引」这个概念从用户面前消失。
//!
//! 在此之前十几处代码在对用户说「先去跑一遍 `duster scan`」——索引是实现
//! 细节,却被做成了用户的义务:他得先知道有这么个东西、先记得去建、还得
//! 自己判断它旧没旧。这个模块把那份义务收回来,分两级:
//!
//! - [`ensure_exists`]:**用例层的硬前提**。要读索引的用例入口自己保证
//!   「库在盘上」,库不在就建一次,绝不回头要用户去敲命令。已经有库就
//!   原样返回,一次扫描都不跑。
//! - [`ensure_fresh`]:**外壳的策略**。再跑一遍增量扫描把库对齐磁盘。
//!   「哪条命令值得为新鲜度多等这一下」是 UX 判断,不是库层的判断:
//!   冷启动全量建库实测 4.14 秒(11 agent / 634 会话 / 26,399 轮),
//!   增量 0.52 秒,而纯读一次 `status` 只要 0.014 秒——一次性命令等得起,
//!   交互式菜单里来回翻页显然等不起。所以这里只把能力摆出来,由调用方
//!   按命令决定调不调。
//!
//! 两者都**绝不因为另一个 duster 实例正在写而失败**。`status` 一直是
//! `open_readonly`:扫描进行中也能看。这个性质比「数据一定最新」重要得多,
//! 撞上写锁就跳过扫描([`Freshness::SkippedLocked`]),读照常进行。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use duster_index::db::LockBusy;

use crate::scan::{ScanOptions, scan};

/// 一次 [`ensure_fresh`] 究竟做了什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// 库原本不在盘上,这一轮从零建起。首次使用走这条路。
    Built,
    /// 库已存在,增量扫描带回了新东西(有会话被重解析,或解析规则换了代)。
    Refreshed,
    /// 另一个 duster 实例正握着写锁,本轮跳过扫描。**不是错误**:
    /// 读路径全是 `open_readonly`,别人写的时候照样看得见。
    SkippedLocked,
    /// 增量扫描跑完了,一场会话都不需要重解析——库本来就跟磁盘对得上。
    UpToDate,
}

/// 保证索引库存在,返回解析后的库路径。
///
/// 「库还没建」不是用户的错,更不该是他要先学会的概念:直接建一次。
/// 建库要几秒,但这是第一次用 duster 的必经成本——报错让他自己去敲一遍
/// scan,只是把同样这几秒挪到他手动重来之后,中间白白多一次失败。
///
/// **只管有没有,不管新不新**:库已经在盘上就原样返回。要不要顺手重扫,
/// 见 [`ensure_fresh`] 的取舍说明。
pub fn ensure_exists(index_path: Option<&Path>) -> Result<PathBuf> {
    let path = resolve(index_path);
    if path.is_file() {
        return Ok(path);
    }
    // 判据取 [`Freshness`] 而不是回头再 stat 一次:撞锁失败的那次
    // `Index::open` 已经把空文件创出来了,再 stat 只会看见一个还没迁移过
    // schema 的壳子,然后把「别人正在建」误报成一个 `no such table`。
    if ensure_fresh(Some(&path))? == Freshness::SkippedLocked {
        bail!(
            "another duster instance is still building its first index at {}; \
             try again in a moment",
            path.display()
        );
    }
    Ok(path)
}

/// 把索引对齐磁盘:库不在就建,库在就跑一次增量扫描。
///
/// 调用方给了库路径,扫描 home 就按它反推(见 [`home_of_index`]),口径与
/// `duster mcp` 反查清单时一致:指定一个索引就等于指定了「这是谁的机器」。
/// 没给路径就交给 [`crate::scan`] 自己解析真实 home——HOME 缺失时该由它
/// 报那句人话错误,而不是在这里把字面量 `~` 当成一个目录使。
///
/// # 为什么撞锁不算失败
///
/// 单实例写锁只挡写,不挡读。用户在一个终端跑着 `duster scan`、在另一个
/// 终端敲 `duster status`,他要的是看一眼,不是被告知"有人在写,你等着"。
/// 所以这里认 [`LockBusy`] **这个类型**(不是它的文案——文案随时会改)
/// 并咽下去,返回 [`Freshness::SkippedLocked`]。
///
/// # Refreshed 与 UpToDate 的分界
///
/// 分界画在**会话重解析**上:scan 只对会话做增量短路(cheap print 没变
/// 就不重解析),其余资源每轮无条件 upsert,压根拿不到「变没变」的信号。
/// 所以「一场会话都没重解析、解析规则也没换代」就是这台机器上能观测到的
/// 最强的「无事发生」——会话是数据量的绝对大头,它全短路了,这一轮扫描
/// 就没做任何实质工作。
pub fn ensure_fresh(index_path: Option<&Path>) -> Result<Freshness> {
    let home = index_path.and_then(home_of_index);
    let path = resolve(index_path);
    let existed = path.is_file();

    let report = match scan(&ScanOptions {
        home,
        index_path: Some(path),
        full: false,
    }) {
        Ok(r) => r,
        Err(e) if is_lock_busy(&e) => return Ok(Freshness::SkippedLocked),
        Err(e) => return Err(e),
    };

    if !existed {
        return Ok(Freshness::Built);
    }
    let worked = report.rules_changed || report.agents.iter().any(|a| a.sessions_indexed > 0);
    Ok(if worked {
        Freshness::Refreshed
    } else {
        Freshness::UpToDate
    })
}

/// 索引库路径口径:`None` = `~/.agent-duster/index.db`,与每一处只读入口一致。
fn resolve(index_path: Option<&Path>) -> PathBuf {
    match index_path {
        Some(p) => p.to_path_buf(),
        None => duster_fs::path::expand_tilde("~/.agent-duster/index.db"),
    }
}

/// 从索引路径反推 home:库按约定住在 `<home>/.agent-duster/index.db`。
///
/// 形状对不上就返回 `None`——那说明这个路径**证明不了** home 是谁
/// (测试夹具、从别的机器拷回来的库),由调用方决定怎么兜底,而不是在这里
/// 悄悄换成本机真实 home。
pub(crate) fn home_of_index(index_path: &Path) -> Option<PathBuf> {
    if let Some(dir) = index_path.parent()
        && dir.file_name() == Some(OsStr::new(".agent-duster"))
        && let Some(home) = dir.parent()
    {
        return Some(home.to_path_buf());
    }
    None
}

/// 错误链里是否有 [`LockBusy`]。
///
/// 认类型不认字符串:提示语随时会改,而 `LockBusy` 是 duster-index 明确
/// 给出的可识别信号,只有它能证明「是别人占着锁」而不是别的打开失败。
fn is_lock_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<LockBusy>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use duster_index::db::Index;
    use duster_index::query::{self, ResourceFilter};
    use tempfile::TempDir;

    /// 只声明会话的迷你 agent:probe 指向 `~/.fresh-fake`。
    /// 会话是唯一走增量短路的资源类,四个返回态全靠它区分。
    const MANIFEST: &str = r#"
[agent]
id = "fresh-fake"
display_name = "Fresh Fake"

[probe]
any_of = ["~/.fresh-fake"]

[[resource]]
kind = "session"
scope = "global"
path = "~/.fresh-fake/projects"
mapper = "native/claude-session"
"#;

    /// 造假 home:一份清单 + 一场会话。返回索引**该在**的位置(还没建)。
    fn seed_home(home: &Path) -> PathBuf {
        let adapters = home.join(".agent-duster").join("adapters");
        fs::create_dir_all(&adapters).unwrap();
        fs::write(adapters.join("fresh-fake.toml"), MANIFEST).unwrap();
        add_session(home, "session-1.jsonl");
        home.join(".agent-duster").join("index.db")
    }

    /// 再放一场会话(内容取自真实 fixture,解析器认得)。
    fn add_session(home: &Path, name: &str) {
        let proj = home.join(".fresh-fake/projects/-Users-tester-demo");
        fs::create_dir_all(&proj).unwrap();
        let sample = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/sessions/claude-basic.jsonl");
        fs::copy(&sample, proj.join(name)).unwrap();
    }

    /// 库里 `kind = "session"` 的行数:用来证明某一轮到底扫没扫。
    fn session_rows(index: &Path) -> usize {
        let idx = Index::open_readonly(index).unwrap();
        query::list_resources(
            idx.conn(),
            &ResourceFilter {
                kinds: vec!["session".to_string()],
                ..Default::default()
            },
        )
        .unwrap()
        .len()
    }

    #[test]
    fn 库不存在时自动建库并返回built() {
        let tmp = TempDir::new().unwrap();
        let index = seed_home(tmp.path());
        assert!(!index.is_file(), "前提:这一刻还没有库");

        assert_eq!(ensure_fresh(Some(&index)).unwrap(), Freshness::Built);
        assert!(index.is_file(), "建库是 Built 的全部意义");
        assert_eq!(session_rows(&index), 1);
    }

    #[test]
    fn 有新会话时返回refreshed() {
        let tmp = TempDir::new().unwrap();
        let index = seed_home(tmp.path());
        ensure_fresh(Some(&index)).unwrap();

        add_session(tmp.path(), "session-2.jsonl");
        assert_eq!(ensure_fresh(Some(&index)).unwrap(), Freshness::Refreshed);
        assert_eq!(session_rows(&index), 2, "新会话必须真的进库");
    }

    #[test]
    fn 无事可做时返回uptodate() {
        let tmp = TempDir::new().unwrap();
        let index = seed_home(tmp.path());
        ensure_fresh(Some(&index)).unwrap();

        // 磁盘一个字节没动:cheap print 全部命中,没有会话需要重解析。
        assert_eq!(ensure_fresh(Some(&index)).unwrap(), Freshness::UpToDate);
        assert_eq!(session_rows(&index), 1);
    }

    /// 硬要求:另一个实例握着写锁时,刷新**跳过**而不是报错——
    /// 读路径是 `open_readonly`,扫描进行中也必须看得见。
    #[test]
    fn 写锁被别人占着时返回skippedlocked而不是报错() {
        let tmp = TempDir::new().unwrap();
        let index = seed_home(tmp.path());
        ensure_fresh(Some(&index)).unwrap();

        let held = Index::open(&index).unwrap(); // 模拟另一个 duster 实例
        assert_eq!(
            ensure_fresh(Some(&index)).unwrap(),
            Freshness::SkippedLocked
        );
        // 锁不挡读:这正是宁可跳过扫描也不报错的理由。
        assert_eq!(session_rows(&index), 1);

        drop(held);
        assert_eq!(ensure_fresh(Some(&index)).unwrap(), Freshness::UpToDate);
    }

    #[test]
    fn ensure_exists_库不在就建一个() {
        let tmp = TempDir::new().unwrap();
        let index = seed_home(tmp.path());

        let got = ensure_exists(Some(&index)).unwrap();
        assert_eq!(got, index);
        assert!(index.is_file());
    }

    /// 已有库就原样返回:盘上多了一场会话它也不管。
    /// 「要不要重扫」是调用方的策略,不是这个函数的职责。
    #[test]
    fn ensure_exists_已有库时不扫描() {
        let tmp = TempDir::new().unwrap();
        let index = seed_home(tmp.path());
        ensure_fresh(Some(&index)).unwrap();
        add_session(tmp.path(), "session-2.jsonl");

        ensure_exists(Some(&index)).unwrap();
        assert_eq!(session_rows(&index), 1, "没扫,所以第二场会话不该在库里");
    }

    /// 首次建库撞上别人的锁:必须给人话,而不是把那次失败的 `Index::open`
    /// 顺手创出来的空壳当成「库有了」交出去(交出去的下一站是 `no such table`)。
    ///
    /// 直接占旁路 `<db>.lock`,不碰 index.db 本身——`Index::open` 会创建
    /// index.db,用它占锁就造不出「库还不存在但锁已被占」这个局面。
    #[test]
    fn ensure_exists_首次建库撞锁时给人话而不是空库() {
        let tmp = TempDir::new().unwrap();
        let index = seed_home(tmp.path());
        let held = rusqlite::Connection::open(format!("{}.lock", index.display())).unwrap();
        held.busy_timeout(std::time::Duration::ZERO).unwrap();
        held.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let err = ensure_exists(Some(&index)).unwrap_err();
        assert!(
            err.to_string().contains("another duster instance"),
            "{err:#}"
        );
    }

    #[test]
    fn 库路径反推home() {
        assert_eq!(
            home_of_index(Path::new("/u/bob/.agent-duster/index.db")),
            Some(PathBuf::from("/u/bob"))
        );
        // 形状对不上不猜:宁可返回 None 让调用方兜底。
        assert_eq!(home_of_index(Path::new("/tmp/copied.db")), None);
    }
}
