//! M1 验收的四条机器守卫（todo.md「M1 验收」里的 `[std] fixtures 断言`）。
//!
//! 它们和各模块的单元测试**故意**有重叠。单元测试守的是"这个函数做对了没"，
//! 这四条守的是四个不能被悄悄改掉的产品承诺：
//!
//! 1. clean 是**真删**——没有回收站、没有副本。这一条防的是有人日后觉得
//!    "加个回收站更安全"就顺手加上，那会让"已回收 774 MB"变成一句谎话。
//! 2. 归档包解开后与删除前**逐字节一致**。归档是 prune 唯一的退路，
//!    往返一致是它唯一的机器守卫。
//! 3. 预估超阈值且用户没表态时**拒绝执行**。
//! 4. `install` 路径在 clean / prune 计划里**恒为空集**。这是
//!    "duster 永不卸载用户软件"唯一的机器守卫。
//!
//! 全部注入假 home、显式索引路径、固定时钟与显式导出目录：
//! 真实 `$HOME`、真实时钟、真实 `~/agent-duster-exports` 一次都不碰。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use duster_core::plan::{self, PlanOptions};
use duster_core::{clean, prune, scan};
use tempfile::TempDir;

/// 固定的「现在」（2025-10-09T07:33:20Z）。所有陈旧判定都相对它算，
/// 断言因此不会随日历慢慢烂掉。
const NOW_MS: i64 = 1_760_000_000_000;
const DAY_MS: i64 = 86_400_000;

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

/// 一个假 home，形状刚好让内置的 `codex` / `claude-code` 清单探测为已安装，
/// 且每类资源都有真东西：
///
/// - `~/.codex/logs_2.sqlite` —— 真 SQLite 库，灌满再删剩几行，留下大量空闲页（l0）
/// - `~/.codex/cache/blob.bin` —— 可再生缓存（l1）
/// - `~/.codex/sessions/2025/01/01/rollout-*.jsonl` —— 一场会话
/// - `~/.codex/plugins/payload.bin` —— 软件本体（install）
/// - `~/.claude/skills/demo/` —— 一个 skill，内含 `node_modules/big.bin`
///   （资源**内部**的 install 子路径）
struct Fixture {
    _tmp: TempDir,
    home: PathBuf,
    index: PathBuf,
    exports: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().to_path_buf();

        // ── codex ────────────────────────────────────────────────────────
        let codex = home.join(".codex");
        write(&codex.join("config.toml"), b"# empty\n");
        write(&codex.join("AGENTS.md"), b"# memory\n");
        write(&codex.join("cache/blob.bin"), &vec![b'c'; 40_960]);
        write(&codex.join("plugins/payload.bin"), &vec![b'p'; 8_192]);
        write(
            &codex.join("sessions/2025/01/01/rollout-2025-01-01T00-00-00-aaaa.jsonl"),
            SESSION.as_bytes(),
        );
        bloated_sqlite(&codex.join("logs_2.sqlite"));

        // ── claude-code ──────────────────────────────────────────────────
        let claude = home.join(".claude");
        write(&claude.join("CLAUDE.md"), b"# memory\n");
        write(&claude.join("cache/warm.bin"), &vec![b'w'; 16_384]);
        let skill = claude.join("skills/demo");
        write(&skill.join("SKILL.md"), b"---\nname: demo\n---\nbody\n");
        write(&skill.join("notes.md"), b"user content\n");
        // skill 目录**内部**的软件本体：清单声明了 install_paths，
        // 它既不该算进 skill 体积，也不该进归档包，更不该被任何计划碰到。
        write(&skill.join("node_modules/big.bin"), &vec![b'n'; 65_536]);

        let index = home.join(".agent-duster/index.db");
        scan::scan(&scan::ScanOptions {
            home: Some(home.clone()),
            index_path: Some(index.clone()),
            full: false,
        })
        .expect("scan 夹具");

        let exports = tmp.path().join("exports");
        Self {
            _tmp: tmp,
            home,
            index,
            exports,
        }
    }

    fn plan_opts(&self, days: Option<u32>, now_ms: i64) -> PlanOptions {
        PlanOptions {
            index_path: Some(self.index.clone()),
            home: Some(self.home.clone()),
            agents: Vec::new(),
            older_than_days: days,
            keep_generations: false,
            now_ms: Some(now_ms),
        }
    }

    /// 假 home 下每个文件的 (相对路径, BLAKE3, 字节数)。
    /// 索引库自身与它的 `-wal`/`-shm` 排除在外：只读打开都会动它们，
    /// 把它算进"释放了多少"就等于拿噪声当结果。
    fn snapshot(&self) -> BTreeMap<PathBuf, (blake3::Hash, u64)> {
        let mut out = BTreeMap::new();
        collect(&self.home, &self.home, &mut out);
        out.retain(|rel, _| !rel.starts_with(".agent-duster"));
        out
    }
}

const SESSION: &str = concat!(
    r#"{"type":"session_meta","payload":{"id":"aaaa","cwd":"/w","cli_version":"1"}}"#,
    "\n",
    r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"hello duster acceptance"}]}"#,
    "\n",
    r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"acknowledged"}]}"#,
    "\n",
);

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (blake3::Hash, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            collect(root, &p, out);
        } else if meta.is_file() {
            let bytes = fs::read(&p).unwrap_or_default();
            out.insert(
                p.strip_prefix(root).unwrap().to_path_buf(),
                (blake3::hash(&bytes), meta.len()),
            );
        }
    }
}

/// 造一个「文件很大、活数据很少」的 SQLite 库：灌 300 行 4 KiB blob 再删掉
/// 绝大多数。页进 freelist、文件不缩——这正是 l0 存在的那一档。
fn bloated_sqlite(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("PRAGMA auto_vacuum = NONE; CREATE TABLE logs(x BLOB);")
        .unwrap();
    let blob = vec![0u8; 4096];
    for _ in 0..300 {
        conn.execute("INSERT INTO logs(x) VALUES (?1)", [&blob])
            .unwrap();
    }
    conn.execute("DELETE FROM logs WHERE rowid > 5", [])
        .unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    drop(conn);
}

/// 索引里全部**声明为 `install` 资源**的路径。这些是 clean / prune
/// 一个字节都不许碰的东西。
///
/// 内嵌 install 子路径（skill 里的 `node_modules`）不在这份名单里：
/// 它的规则不同，见下方测试的文档注释。
fn declared_install_roots(fx: &Fixture) -> Vec<PathBuf> {
    let idx = duster_index::db::Index::open_readonly(&fx.index).unwrap();
    let roots: Vec<PathBuf> = duster_index::query::list_resources(
        idx.conn(),
        &duster_index::query::ResourceFilter {
            agents: Vec::new(),
            kinds: vec!["install".to_string()],
            clean_levels: Vec::new(),
        },
    )
    .unwrap()
    .into_iter()
    .map(|r| PathBuf::from(r.path))
    .collect();
    assert!(
        !roots.is_empty(),
        "夹具必须真的有 install 资源，否则这条断言什么也没证明"
    );
    roots
}

// ---------------------------------------------------------------------------
// 1. clean 是真删
// ---------------------------------------------------------------------------

/// clean 执行后：计划里的路径确实消失，回收字节数与磁盘实测一致，
/// 且**整个假 home 与导出目录里不存在任何一份被删内容的副本**。
///
/// 最后一条是这个文件里最重要的断言。没有它，日后任何人加一个
/// "先挪进回收站" 都能让全套单元测试继续绿，而用户的磁盘一个字节没释放。
#[test]
fn clean_是真删_不留任何副本() {
    let fx = Fixture::new();
    let before = fx.snapshot();

    let plan = plan::plan_clean(&fx.plan_opts(None, NOW_MS)).unwrap();
    let targets: Vec<PathBuf> = plan.actionable().map(|i| i.path.clone()).collect();
    assert!(!targets.is_empty(), "夹具里必须有可清的东西");

    // 被删内容的指纹：清完之后这些指纹不许在任何地方再出现。
    let doomed: Vec<blake3::Hash> = before
        .iter()
        .filter(|(rel, _)| {
            let abs = fx.home.join(rel);
            targets.iter().any(|t| abs == *t || abs.starts_with(t))
                && rel.file_name() != Some("logs_2.sqlite".as_ref())
        })
        .map(|(_, (h, _))| *h)
        .collect();
    assert!(!doomed.is_empty(), "至少要有一个整文件被删掉");

    let report = clean::clean(&clean::CleanOptions {
        index_path: Some(fx.index.clone()),
        home: Some(fx.home.clone()),
        agents: Vec::new(),
        dry_run: false,
        yes: true,
    })
    .unwrap();
    assert!(report.executed);

    let after = fx.snapshot();
    let freed_on_disk: u64 =
        before.values().map(|(_, n)| n).sum::<u64>() - after.values().map(|(_, n)| n).sum::<u64>();
    assert_eq!(
        report.buckets.reclaimed, freed_on_disk,
        "报出来的回收量必须等于磁盘上真实少掉的字节数"
    );

    // 「消失」只对删除类动作成立。l0 的 VACUUM 与日志截断都是**原地改写**——
    // 文件必须还在，只是变小了。这正是 clean 不可能有回收站的原因：
    // 根本不存在"一个可以被移走的文件"。
    for item in plan.actionable() {
        match item.action {
            plan::Action::RemoveFile | plan::Action::RemoveDir | plan::Action::RemoveSidecar => {
                assert!(
                    !item.path.exists(),
                    "删除类动作执行后路径必须消失: {}",
                    item.path.display()
                );
            }
            plan::Action::Vacuum | plan::Action::TruncateFile => {
                let now = fs::metadata(&item.path)
                    .unwrap_or_else(|e| panic!("原地改写不该让 {} 消失: {e}", item.path.display()))
                    .len();
                let was = before[&item.path.strip_prefix(&fx.home).unwrap().to_path_buf()].1;
                assert!(
                    now < was,
                    "{} 原地改写后应该变小: {was} → {now}",
                    item.path.display()
                );
            }
            other => panic!("clean 不该排出 {other:?} 这种动作"),
        }
    }

    // 没有回收站：被删内容不在假 home 里，也不在导出目录里。
    for (rel, (hash, _)) in &after {
        assert!(
            !doomed.contains(hash),
            "被删内容以副本形式活了下来: {}",
            rel.display()
        );
    }
    assert!(
        !fx.exports.exists() && !fx.home.join("agent-duster-exports").exists(),
        "clean 不归档，导出目录一个都不该出现"
    );
}

// ---------------------------------------------------------------------------
// 2. 归档包往返一致
// ---------------------------------------------------------------------------

/// prune 把不可再生内容删掉之前先打包。解开归档包后，每个文件都必须与
/// 删除前逐字节相同——这是归档机制唯一的机器守卫。
#[test]
fn prune_归档包解开后与删除前逐字节一致() {
    let fx = Fixture::new();
    let before = fx.snapshot();

    // 时钟往后拨一年：夹具里的东西全部超期。
    let now = NOW_MS + 365 * DAY_MS;
    let plan = plan::plan_prune(&fx.plan_opts(Some(30), now)).unwrap();
    let archived: Vec<PathBuf> = plan
        .items
        .iter()
        .filter(|i| i.archived)
        .map(|i| i.path.clone())
        .collect();
    assert!(!archived.is_empty(), "夹具里必须有要归档的东西");

    let report = prune::prune(&prune::PruneOptions {
        index_path: Some(fx.index.clone()),
        home: Some(fx.home.clone()),
        agents: Vec::new(),
        older_than_days: 30,
        keep_generations: false,
        archive: Some(true),
        export_dir: Some(fx.exports.clone()),
        dry_run: false,
        yes: true,
        json: false,
        now_ms: Some(now),
    })
    .unwrap();
    assert!(report.executed);

    let pack = PathBuf::from(report.archive_path.expect("必须产出归档包"));
    assert!(pack.is_file(), "归档包要真的落在盘上: {}", pack.display());

    let out = TempDir::new().unwrap();
    duster_fs::archive::extract_to(&pack, out.path()).unwrap();

    let mut checked = 0usize;
    for (rel, (hash, _)) in &before {
        let abs = fx.home.join(rel);
        if !archived.iter().any(|a| abs == *a || abs.starts_with(a)) {
            continue;
        }
        let restored = out.path().join(rel);
        let bytes =
            fs::read(&restored).unwrap_or_else(|e| panic!("归档包里少了 {}: {e}", rel.display()));
        assert_eq!(
            blake3::hash(&bytes),
            *hash,
            "归档包里的 {} 与删除前不一致",
            rel.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "一个文件都没校验到,这条断言等于没写");
}

// ---------------------------------------------------------------------------
// 3. 超阈值未表态即拒绝
// ---------------------------------------------------------------------------

/// 归档预估超过 200 MB 时不自动打包：用户必须显式说 `--archive` 或
/// `--no-archive`。没表态就拒绝执行，且一个字节都不许动。
#[test]
fn prune_预估超阈值时不自动归档且未表态即拒绝() {
    let fx = Fixture::new();
    // 稀疏文件：set_len 不占真实磁盘，但 metadata().len() 是真的。
    let big = fx.home.join(".claude/skills/demo/huge.bin");
    let f = fs::File::create(&big).unwrap();
    f.set_len(duster_fs::archive::AUTO_ARCHIVE_LIMIT + 1)
        .unwrap();
    drop(f);
    scan::scan(&scan::ScanOptions {
        home: Some(fx.home.clone()),
        index_path: Some(fx.index.clone()),
        full: false,
    })
    .unwrap();

    let now = NOW_MS + 365 * DAY_MS;
    let base = prune::PruneOptions {
        index_path: Some(fx.index.clone()),
        home: Some(fx.home.clone()),
        agents: Vec::new(),
        older_than_days: 30,
        keep_generations: false,
        archive: None,
        export_dir: Some(fx.exports.clone()),
        dry_run: false,
        yes: true,
        json: false,
        now_ms: Some(now),
    };

    let before = fx.snapshot();
    let err = prune::prune(&base).expect_err("未表态必须拒绝");
    let msg = format!("{err:#}");
    assert!(msg.contains("--archive"), "错误必须点名 --archive: {msg}");
    assert!(
        msg.contains("--no-archive"),
        "错误必须点名 --no-archive: {msg}"
    );
    assert!(!fx.exports.exists(), "拒绝执行时不许留下归档目录");
    assert_eq!(fx.snapshot(), before, "拒绝执行时一个文件都不许动");

    // 表态之后放行：--no-archive 直接删，不打包。
    let report = prune::prune(&prune::PruneOptions {
        archive: Some(false),
        ..base
    })
    .unwrap();
    assert!(report.executed);
    assert!(report.archive_path.is_none(), "--no-archive 不该产出归档包");
}

// ---------------------------------------------------------------------------
// 4. install 恒为空集
// ---------------------------------------------------------------------------

/// clean 与 prune 的可执行项里，永远不许出现落在 `install` 路径下的东西。
///
/// 这是「duster 永不卸载用户软件」唯一的机器守卫。install 行本身照样出现在
/// 计划里（标 not cleanable，让用户看见那几个 GB 为什么不动），
/// 但它绝不能是**可执行**的那一类。
///
/// 内嵌 install 子路径（skill 目录里的 `node_modules`）的界线要说清楚，
/// 因为它和声明成资源的 install **不是同一条规则**：
/// - 它永远不能自己成为一个计划项——没有哪条命令会单独去删某个 skill 的
///   `node_modules`；
/// - `clean` 一个字节都不碰它；
/// - 但 `prune` 删掉整个陈旧 skill 时，它随目录一起走。留下一个没有
///   `SKILL.md` 的 `node_modules` 才是更糟的结果。归档包按设计不含它
///   （体积会被撑爆），所以计划的 impact 里明写了这一点。
#[test]
fn install_目录在_clean_与_prune_计划里恒为空集() {
    let fx = Fixture::new();
    let declared = declared_install_roots(&fx);
    let embedded = fx.home.join(".claude/skills/demo/node_modules");
    let now = NOW_MS + 365 * DAY_MS;

    let plans = [
        ("clean", plan::plan_clean(&fx.plan_opts(None, now)).unwrap()),
        (
            "prune",
            plan::plan_prune(&fx.plan_opts(Some(30), now)).unwrap(),
        ),
    ];

    for (verb, p) in &plans {
        for item in p.actionable() {
            for root in &declared {
                assert!(
                    item.path != *root && !item.path.starts_with(root),
                    "{verb} 计划把软件本体排进了可执行项: {} 落在 {} 之下",
                    item.path.display(),
                    root.display()
                );
            }
            assert_ne!(
                item.path, embedded,
                "{verb} 不该把内嵌 install 子路径单列成一项"
            );
        }
        // 反面:install 行必须**出现**,只是不可执行。看不见它,
        // 用户就不知道那几个 GB 去哪了。
        let shown = p.items.iter().filter(|i| !i.cleanable).count();
        assert!(shown > 0, "{verb} 计划必须列出 not cleanable 的软件本体");
    }

    // 执行一遍再复核:计划正确不等于执行正确。
    clean::clean(&clean::CleanOptions {
        index_path: Some(fx.index.clone()),
        home: Some(fx.home.clone()),
        agents: Vec::new(),
        dry_run: false,
        yes: true,
    })
    .unwrap();
    assert!(embedded.is_dir(), "clean 不许碰 skill 里的 node_modules");

    prune::prune(&prune::PruneOptions {
        index_path: Some(fx.index.clone()),
        home: Some(fx.home.clone()),
        agents: Vec::new(),
        older_than_days: 30,
        keep_generations: false,
        archive: Some(true),
        export_dir: Some(fx.exports.clone()),
        dry_run: false,
        yes: true,
        json: false,
        now_ms: Some(now),
    })
    .unwrap();
    for root in &declared {
        assert!(
            root.exists(),
            "clean/prune 执行后声明为 install 的资源必须原封不动: {}",
            root.display()
        );
    }
}
