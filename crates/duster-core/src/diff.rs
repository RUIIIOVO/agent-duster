//! 通用 diff 引擎。两处共用，**不许有第二份实现**：
//!
//! - `duster skill list` 的 DRIFTED 明细（今天在 `skill_ops` 里是手写的
//!   文件级摘要，落地后换成调这里）；
//! - 顶层 `duster diff <a> <b>`（任意两个资源并排看）。
//!
//! `duster mcp diff` 曾共用这里，随铺平表撤下——「几家声明一样吗」由
//! `mcp list` 的 STATE 列直接回答，不再需要第二条命令。
//!
//! 两处的差异只在**输入怎么变成可比对的行**，比对与渲染是同一件事。
//! 写第二遍的代价不是多几百行代码，是两处的 `-`/`+` 语义慢慢走散。

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use duster_fs::hash::hash_file;
use duster_fs::walk::{WalkOptions, walk_files};

/// 一处差异的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// 只在左侧存在。
    OnlyLeft,
    /// 只在右侧存在。
    OnlyRight,
    /// 两侧都有，内容不同。
    Changed,
    /// 两侧都有且相同。默认不进结果，`--full` 时才带上。
    Same,
}

/// 一条差异记录。`key` 的含义随输入而定：
/// 文件树是相对路径，MCP 是字段路径（`command` / `env.API_KEY`）。
#[derive(Debug, Clone, Serialize)]
pub struct DiffEntry {
    pub key: String,
    pub change: Change,
    /// 左侧内容；`OnlyRight` 时为 None。
    pub left: Option<String>,
    /// 右侧内容；`OnlyLeft` 时为 None。
    pub right: Option<String>,
    /// 行级明细。只在两侧都是文本、且 [`DiffOptions::line_level`] 打开时填充。
    /// 二进制或超大文件恒为 None——对着 3 MB 的 blob 做行 diff 没有意义。
    pub hunks: Option<Vec<Hunk>>,
}

/// 一段行级差异（统一 diff 的一个 hunk）。
#[derive(Debug, Clone, Serialize)]
pub struct Hunk {
    /// 左侧起始行（1 起）与行数。
    pub left_start: usize,
    pub left_len: usize,
    /// 右侧起始行（1 起）与行数。
    pub right_start: usize,
    pub right_len: usize,
    /// 行内容，前缀 ` ` / `-` / `+`，与 unified diff 一致。
    pub lines: Vec<String>,
}

/// 一次比较的结果。
#[derive(Debug, Clone, Serialize)]
pub struct Diff {
    /// 左右两侧的展示标签（agent id、路径……），渲染表头用。
    pub left_label: String,
    pub right_label: String,
    pub entries: Vec<DiffEntry>,
    /// 两侧完全一致。
    pub identical: bool,
    pub warnings: Vec<String>,
}

/// 比较选项。
#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// 把 `Same` 也放进结果。
    pub include_same: bool,
    /// 对文本内容做行级 diff。
    pub line_level: bool,
    /// 单个条目参与行级 diff 的字节上限；超过只报「内容不同」。
    pub line_level_cap: u64,
    /// 目录比较时剪掉的子目录名（`node_modules` 等）。
    pub prune_dirs: Vec<String>,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            include_same: false,
            line_level: true,
            // 256 KiB：够覆盖任何手写的配置与文档，挡掉编译产物和数据文件。
            line_level_cap: 256 * 1024,
            prune_dirs: crate::skill_ops::DEFAULT_INSTALL_DIRS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// 比较两棵目录树。`skill list` 的 DRIFTED 明细走这条。
///
/// 按相对路径配对；两侧内容哈希相同即 `Same`，否则 `Changed` 并（在
/// 阈值内且是有效 UTF-8 时）附行级 hunks。符号链接比链接目标字符串本身，
/// 不跟随——跟随会让两棵树的比较变成两个链接终点的比较，那是另一个问题。
///
/// [`DiffEntry::left`] / [`DiffEntry::right`] 在这里是**内容标识**而不是内容本身：
/// 常规文件写 `blake3:<hex>`，符号链接写 `symlink -> <target>`。整份内容不进
/// 结构体——一棵树可以有几百兆，真正要看的行级明细在 `hunks` 里，而
/// 只在一侧的文件也需要一个能跨副本对拍的身份（体积会撒谎，哈希不会）。
pub fn diff_trees(
    left: &Path,
    right: &Path,
    left_label: &str,
    right_label: &str,
    opts: &DiffOptions,
) -> Result<Diff> {
    let mut warnings: Vec<String> = Vec::new();
    let l = collect_tree(left, opts, &mut warnings)?;
    let r = collect_tree(right, opts, &mut warnings)?;

    // 两侧都是 BTreeMap，并集排序后产出：同一对目录跑两次，输出逐字节一致，
    // 两次结果本身可以再 diff。
    let mut keys: Vec<&str> = l.keys().map(String::as_str).collect();
    keys.extend(r.keys().map(String::as_str));
    keys.sort_unstable();
    keys.dedup();

    let mut entries: Vec<DiffEntry> = Vec::new();
    let mut identical = true;
    for key in keys {
        let entry = match (l.get(key), r.get(key)) {
            (Some(a), None) => DiffEntry {
                key: key.to_string(),
                change: Change::OnlyLeft,
                left: Some(describe(a, &mut warnings)),
                right: None,
                hunks: None,
            },
            (None, Some(b)) => DiffEntry {
                key: key.to_string(),
                change: Change::OnlyRight,
                left: None,
                right: Some(describe(b, &mut warnings)),
                hunks: None,
            },
            (Some(a), Some(b)) => compare_nodes(key, a, b, opts, &mut warnings),
            // 键来自两侧的并集，不可能两边都没有。
            (None, None) => unreachable!("key came from the union of both sides"),
        };
        if entry.change != Change::Same {
            identical = false;
            entries.push(entry);
        } else if opts.include_same {
            entries.push(entry);
        }
    }

    Ok(Diff {
        left_label: left_label.to_string(),
        right_label: right_label.to_string(),
        entries,
        identical,
        warnings,
    })
}

/// 树里的一个可比条目。目录不入表（目录本身没有内容），
/// 符号链接记链接目标字符串本身——不跟随。
#[derive(Debug)]
enum Node {
    File { path: PathBuf, len: u64 },
    Link { target: String },
}

/// 遍历一侧，收 `相对路径 -> 条目`。单条读不动降级成 warning：
/// 一条坏链不该让整次比较失败，但也不能悄悄消失。
fn collect_tree(
    root: &Path,
    opts: &DiffOptions,
    warnings: &mut Vec<String>,
) -> Result<BTreeMap<String, Node>> {
    let wopts = WalkOptions {
        prune_dirs: opts.prune_dirs.clone(),
        ..Default::default()
    };
    let mut out: BTreeMap<String, Node> = BTreeMap::new();
    walk_files(root, &wopts, |p, meta| {
        let key = rel_key(root, p);
        if meta.is_symlink() {
            match fs::read_link(p) {
                Ok(t) => {
                    out.insert(
                        key,
                        Node::Link {
                            target: t.to_string_lossy().into_owned(),
                        },
                    );
                }
                Err(e) => warnings.push(format!("failed to read symlink {}: {e}", p.display())),
            }
        } else if meta.is_file() {
            out.insert(
                key,
                Node::File {
                    path: p.to_path_buf(),
                    len: meta.len(),
                },
            );
        } else {
            // 管道 / socket 之类：没有可比的内容，报出来而不是假装不存在。
            warnings.push(format!("skipped non-regular entry: {}", p.display()));
        }
    })
    .with_context(|| format!("failed to walk directory: {}", root.display()))?;
    Ok(out)
}

/// 相对 `root` 的展示键（`/` 分隔）。脱不掉前缀就退化为文件名，不 panic。
fn rel_key(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or_else(|_| Path::new(p.file_name().unwrap_or(OsStr::new(""))))
        .to_string_lossy()
        .replace('\\', "/")
}

/// 一个条目的内容标识。文件读不动时给出人话，并记一条 warning——
/// 「读不出来」和「内容为空」必须分得清。
fn describe(node: &Node, warnings: &mut Vec<String>) -> String {
    match node {
        Node::Link { target } => format!("symlink -> {target}"),
        Node::File { path, .. } => match hash_hex(path) {
            Ok(h) => h,
            Err(e) => {
                warnings.push(format!("failed to hash {}: {e:#}", path.display()));
                "(unreadable)".to_string()
            }
        },
    }
}

/// 文件内容哈希的展示形态。
fn hash_hex(path: &Path) -> Result<String> {
    Ok(format!("blake3:{}", hash_file(path)?.to_hex()))
}

/// 两侧都有的一个键：定 `Same` / `Changed`，够条件时附行级明细。
fn compare_nodes(
    key: &str,
    a: &Node,
    b: &Node,
    opts: &DiffOptions,
    warnings: &mut Vec<String>,
) -> DiffEntry {
    let (change, hunks) = match (a, b) {
        (Node::Link { target: ta }, Node::Link { target: tb }) => {
            // 比目标字符串，不 stat、不跟随：两条指向同一个位置的链接是同一件
            // 东西，链接终点变了是终点的事。
            let change = if ta == tb {
                Change::Same
            } else {
                Change::Changed
            };
            (change, None)
        }
        (
            Node::File {
                path: pa, len: la, ..
            },
            Node::File {
                path: pb, len: lb, ..
            },
        ) => match (hash_file(pa), hash_file(pb)) {
            (Ok(x), Ok(y)) if x == y => (Change::Same, None),
            (Ok(_), Ok(_)) => (Change::Changed, file_hunks(pa, pb, *la, *lb, opts)),
            (x, y) => {
                // 读不动的一侧不能算「相同」：宁可报成不同并说明原因。
                for (p, r) in [(pa, x), (pb, y)] {
                    if let Err(e) = r {
                        warnings.push(format!("failed to hash {}: {e:#}", p.display()));
                    }
                }
                (Change::Changed, None)
            }
        },
        // 一侧文件一侧链接：类型都不同，谈不上内容相同。
        _ => (Change::Changed, None),
    };
    DiffEntry {
        key: key.to_string(),
        change,
        left: Some(describe(a, warnings)),
        right: Some(describe(b, warnings)),
        hunks,
    }
}

/// 行级明细的准入：两侧都在阈值内、都是有效 UTF-8，且真能切出 hunk。
///
/// 拿不到就返回 `None`，条目退回「内容不同」——对着二进制 blob 做行 diff
/// 只会刷屏。只差结尾换行这种「哈希不同但逐行相同」的情况同样给 `None`：
/// `Some(vec![])` 会被渲染成一个空 hunk，比没有更让人困惑。
fn file_hunks(a: &Path, b: &Path, la: u64, lb: u64, opts: &DiffOptions) -> Option<Vec<Hunk>> {
    if !opts.line_level || la > opts.line_level_cap || lb > opts.line_level_cap {
        return None;
    }
    let hunks = diff_text(&read_text(a)?, &read_text(b)?);
    if hunks.is_empty() { None } else { Some(hunks) }
}

/// 读成文本；读不动或不是 UTF-8 都返回 `None`（此时行级 diff 无意义）。
fn read_text(p: &Path) -> Option<String> {
    String::from_utf8(fs::read(p).ok()?).ok()
}

/// 比较两份结构化配置（同一个 MCP server 在两个 agent 里的声明）。
///
/// 把两侧展平成 `字段路径 -> 标量` 再逐键比对。展平而不是整体 JSON diff，
/// 是因为用户要问的是"哪个字段不一样"，不是"第几行不一样"——
/// 两个 agent 的配置本来就不同格式（一个 JSON 一个 TOML），行号毫无意义。
///
/// 展平与比较是**同一趟递归**：两侧在同一个路径上都是容器就往下走，
/// 都是标量就直接比。这样「一侧 `"npx"`、另一侧 `["npx","-y"]`」报的是
/// `command` 一条 Changed，而不是 `command` 只在左 + `command.0`/`command.1`
/// 只在右三条——后者字面上也算展平后逐键比对，但把一个字段的改动
/// 说成三处，用户读不出发生了什么。
///
/// 条目顺序是递归顺序（对象按键字典序、数组按下标），不再整体按 key 排：
/// 按 key 排会把 `args.10` 排到 `args.2` 前面。
pub fn diff_values(
    left: &serde_json::Value,
    right: &serde_json::Value,
    left_label: &str,
    right_label: &str,
    opts: &DiffOptions,
) -> Diff {
    let mut entries: Vec<DiffEntry> = Vec::new();
    compare_json("", left, right, &mut entries);
    let identical = entries.iter().all(|e| e.change == Change::Same);
    if !opts.include_same {
        entries.retain(|e| e.change != Change::Same);
    }
    Diff {
        left_label: left_label.to_string(),
        right_label: right_label.to_string(),
        entries,
        identical,
        // 内存里的两棵 Value 没有「读不动」这回事：解析失败在调用方就拦下了。
        warnings: Vec::new(),
    }
}

/// 同一路径上的两侧递归比较，结果按递归顺序 push 进 `out`。
fn compare_json(path: &str, l: &Value, r: &Value, out: &mut Vec<DiffEntry>) {
    match (l, r) {
        (Value::Object(a), Value::Object(b)) => {
            // serde_json 默认的 Map 是 BTreeMap，键序稳定。
            let mut keys: Vec<&str> = a.keys().map(String::as_str).collect();
            keys.extend(b.keys().map(String::as_str));
            keys.sort_unstable();
            keys.dedup();
            for k in keys {
                let p = child_path(path, k);
                match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => compare_json(&p, x, y, out),
                    (Some(x), None) => flatten_side(&p, x, Change::OnlyLeft, out),
                    (None, Some(y)) => flatten_side(&p, y, Change::OnlyRight, out),
                    (None, None) => unreachable!("key came from the union of both sides"),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for i in 0..a.len().max(b.len()) {
                let p = child_path(path, &i.to_string());
                match (a.get(i), b.get(i)) {
                    (Some(x), Some(y)) => compare_json(&p, x, y, out),
                    (Some(x), None) => flatten_side(&p, x, Change::OnlyLeft, out),
                    (None, Some(y)) => flatten_side(&p, y, Change::OnlyRight, out),
                    (None, None) => unreachable!("index is below the longer side's length"),
                }
            }
        }
        // 剩下的都是叶子对叶子，或者两侧结构不同（标量 vs 容器）。
        // 后者整体渲染成一条：这就是用户眼里的「这个字段被改了」。
        _ => out.push(DiffEntry {
            key: leaf_key(path),
            change: if l == r {
                Change::Same
            } else {
                Change::Changed
            },
            left: Some(render(l)),
            right: Some(render(r)),
            hunks: None,
        }),
    }
}

/// 只在一侧存在的子树：展平成叶子逐个报，空容器本身也算一个叶子——
/// 否则「左边有 `env: {}`、右边没有 `env`」会一条都报不出来。
fn flatten_side(path: &str, v: &Value, change: Change, out: &mut Vec<DiffEntry>) {
    match v {
        Value::Object(m) if !m.is_empty() => {
            for (k, x) in m {
                flatten_side(&child_path(path, k), x, change, out);
            }
        }
        Value::Array(a) if !a.is_empty() => {
            for (i, x) in a.iter().enumerate() {
                flatten_side(&child_path(path, &i.to_string()), x, change, out);
            }
        }
        _ => {
            let rendered = render(v);
            let (left, right) = match change {
                Change::OnlyLeft => (Some(rendered), None),
                _ => (None, Some(rendered)),
            };
            out.push(DiffEntry {
                key: leaf_key(path),
                change,
                left,
                right,
                hunks: None,
            });
        }
    }
}

/// `env` + `API_KEY` -> `env.API_KEY`；根层不带前导点。
fn child_path(path: &str, seg: &str) -> String {
    if path.is_empty() {
        seg.to_string()
    } else {
        format!("{path}.{seg}")
    }
}

/// 根本身就是标量时路径是空串，给它一个能打印的名字。
fn leaf_key(path: &str) -> String {
    if path.is_empty() {
        ".".to_string()
    } else {
        path.to_string()
    }
}

/// 标量的紧凑稳定渲染。
///
/// 字符串不加引号（`command` 的值就该显示成 `npx`），数字走
/// `serde_json::Number` 自己的格式（不会凭空多出 `.0` 或浮点尾巴），
/// 容器（只在结构不同的那一支出现）走紧凑 JSON，键序由 Map 保证稳定。
fn render(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// 比较两段文本，产出统一 diff 的 hunks。
///
/// 经典 Myers 最短编辑脚本即可；上下文行数固定 3 行（与 `git diff` 一致，
/// 用户的眼睛已经被训练成这个节奏）。
///
/// 用的是线性空间版（Myers 1986 §4b：中间蛇 + 分治），不是记录整张 V 表的
/// 教科书版：256 KiB 的两份文本可以有上万行，V 表快照是 O(D²)，
/// 最坏能吃掉几个 GB，而分治版只要 O(N)。
/// 结尾换行的有无不体现为 hunk（按行切分后两侧相同就没有可报的行差异）。
pub fn diff_text(left: &str, right: &str) -> Vec<Hunk> {
    if left == right {
        return Vec::new();
    }
    let a = split_lines(left);
    let b = split_lines(right);
    let mut script: Vec<Step> = Vec::with_capacity(a.len() + b.len());
    diff_rec(&a, &b, 0, 0, &mut script);
    build_hunks(&a, &b, &script)
}

/// 统一 diff 的上下文行数。与 `git diff` 一致。
const CONTEXT: usize = 3;

/// 空串是零行，不是一行空行；其余按 `lines()` 切（结尾换行不产生末尾空行）。
fn split_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.lines().collect()
    }
}

/// 编辑脚本的一步。`li` / `ri` 是这一步发生时两侧的 0 起行号。
#[derive(Debug, Clone, Copy)]
struct Step {
    kind: StepKind,
    li: usize,
    ri: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepKind {
    Keep,
    Del,
    Ins,
}

/// 分治求编辑脚本：剥公共前后缀 -> 求中间蛇 -> 左右两半递归。
fn diff_rec(a: &[&str], b: &[&str], ai: usize, bi: usize, out: &mut Vec<Step>) {
    // 公共前缀直接记 Keep：能剥掉的部分不必进 Myers。
    let mut head = 0;
    while head < a.len() && head < b.len() && a[head] == b[head] {
        out.push(Step {
            kind: StepKind::Keep,
            li: ai + head,
            ri: bi + head,
        });
        head += 1;
    }
    let (a, b, ai, bi) = (&a[head..], &b[head..], ai + head, bi + head);

    let mut tail = 0;
    while tail < a.len() && tail < b.len() && a[a.len() - 1 - tail] == b[b.len() - 1 - tail] {
        tail += 1;
    }
    let (ca, cb) = (&a[..a.len() - tail], &b[..b.len() - tail]);

    if ca.is_empty() {
        for j in 0..cb.len() {
            out.push(Step {
                kind: StepKind::Ins,
                li: ai,
                ri: bi + j,
            });
        }
    } else if cb.is_empty() {
        for i in 0..ca.len() {
            out.push(Step {
                kind: StepKind::Del,
                li: ai + i,
                ri: bi,
            });
        }
    } else {
        match middle_snake(ca, cb) {
            // 中间蛇必然把问题切小（证明见 `middle_snake`），这里再挡一道：
            // 万一切不动，退化成「整段删 + 整段插」也好过挂死在递归里。
            // 左半严格更小（xs+ys < 总长）且右半严格更小（xe+ye > 0）。
            Some((xs, ys, xe, ye)) if xs + ys < ca.len() + cb.len() && xe + ye > 0 => {
                diff_rec(&ca[..xs], &cb[..ys], ai, bi, out);
                for t in 0..(xe - xs) {
                    out.push(Step {
                        kind: StepKind::Keep,
                        li: ai + xs + t,
                        ri: bi + ys + t,
                    });
                }
                diff_rec(&ca[xe..], &cb[ye..], ai + xe, bi + ye, out);
            }
            _ => {
                for i in 0..ca.len() {
                    out.push(Step {
                        kind: StepKind::Del,
                        li: ai + i,
                        ri: bi,
                    });
                }
                for j in 0..cb.len() {
                    out.push(Step {
                        kind: StepKind::Ins,
                        li: ai + ca.len(),
                        ri: bi + j,
                    });
                }
            }
        }
    }

    let (ta, tb) = (ai + ca.len(), bi + cb.len());
    for t in 0..tail {
        out.push(Step {
            kind: StepKind::Keep,
            li: ta + t,
            ri: tb + t,
        });
    }
}

/// 最优编辑路径中点处的那条「蛇」（一段纯对角线），返回其两端
/// `(x_start, y_start, x_end, y_end)`（`a`/`b` 的局部下标）。
///
/// 正向从 `(0,0)` 推进，反向在**翻转后的序列**上同样正向推进（省得把
/// 递推式再反着写一遍），两侧在同一条对角线上相遇即中点：反向的
/// `u = n - x`、`v = m - y`，其对角线 `c = u - v` 对应正向的 `k = delta - c`。
///
/// 调用方靠它切小问题，所以必须保证「切得动」：正向返回时
/// `x_start + y_start = d ≥ 1`（`d = 0` 的重合已被两侧的判据排除），
/// 反向返回时 `u + v ≥ 1`，两种情况下左右子问题都严格更小。
fn middle_snake(a: &[&str], b: &[&str]) -> Option<(usize, usize, usize, usize)> {
    let n = a.len() as i64;
    let m = b.len() as i64;
    let delta = n - m;
    let odd = delta % 2 != 0;
    let max_d = (n + m + 1) / 2;
    // 下标基准：k 最远用到 ±(max_d + 1)。
    let off = (max_d + 1) as usize;
    let mut vf = vec![0i64; (2 * max_d + 3) as usize];
    let mut vb = vec![0i64; (2 * max_d + 3) as usize];

    for d in 0..=max_d {
        // 正向一轮。
        let mut k = -d;
        while k <= d {
            let i = (off as i64 + k) as usize;
            let mut x = if k == -d || (k != d && vf[i - 1] < vf[i + 1]) {
                vf[i + 1]
            } else {
                vf[i - 1] + 1
            };
            let mut y = x - k;
            let (x0, y0) = (x, y);
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            vf[i] = x;
            // delta 为奇数时，重合总是先在正向被看到；反向此刻只推进到 d-1，
            // 故判据是 `|delta - k| < d`。
            if odd && (delta - k).abs() < d {
                let j = (off as i64 + delta - k) as usize;
                if x >= n - vb[j] {
                    return Some((x0 as usize, y0 as usize, x as usize, y as usize));
                }
            }
            k += 2;
        }

        // 反向一轮（翻转序列上的正向）。
        let mut c = -d;
        while c <= d {
            let i = (off as i64 + c) as usize;
            let mut u = if c == -d || (c != d && vb[i - 1] < vb[i + 1]) {
                vb[i + 1]
            } else {
                vb[i - 1] + 1
            };
            let mut v = u - c;
            let (u0, v0) = (u, v);
            while u < n && v < m && a[(n - 1 - u) as usize] == b[(m - 1 - v) as usize] {
                u += 1;
                v += 1;
            }
            vb[i] = u;
            // delta 为偶数时在反向看到；正向本轮已经推进到 d。
            if !odd && (delta - c).abs() <= d {
                let j = (off as i64 + delta - c) as usize;
                if vf[j] >= n - u {
                    return Some((
                        (n - u) as usize,
                        (m - v) as usize,
                        (n - u0) as usize,
                        (m - v0) as usize,
                    ));
                }
            }
            c += 2;
        }
    }
    None
}

/// 编辑脚本 -> 统一 diff 的 hunks。
///
/// 改动点各自向外扩 [`CONTEXT`] 行；两段上下文挨上（中间恰好 ≤ 2×CONTEXT 行
/// 没变）就并成一个 hunk，中间还剩空隙才分开——`git diff` 就是这个节奏。
fn build_hunks(a: &[&str], b: &[&str], script: &[Step]) -> Vec<Hunk> {
    let last = match script.len() {
        0 => return Vec::new(),
        n => n - 1,
    };
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (i, s) in script.iter().enumerate() {
        if s.kind == StepKind::Keep {
            continue;
        }
        let (lo, hi) = (i.saturating_sub(CONTEXT), (i + CONTEXT).min(last));
        match ranges.last_mut() {
            Some(prev) if lo <= prev.1 + 1 => prev.1 = prev.1.max(hi),
            _ => ranges.push((lo, hi)),
        }
    }

    ranges
        .into_iter()
        .map(|(lo, hi)| {
            let mut lines = Vec::with_capacity(hi - lo + 1);
            let (mut left_len, mut right_len) = (0usize, 0usize);
            for s in &script[lo..=hi] {
                match s.kind {
                    StepKind::Keep => {
                        lines.push(format!(" {}", a[s.li]));
                        left_len += 1;
                        right_len += 1;
                    }
                    StepKind::Del => {
                        lines.push(format!("-{}", a[s.li]));
                        left_len += 1;
                    }
                    StepKind::Ins => {
                        lines.push(format!("+{}", b[s.ri]));
                        right_len += 1;
                    }
                }
            }
            let first = &script[lo];
            Hunk {
                // 某一侧一行都没有时起始行记 0（`@@ -0,0 +1,3 @@`），与 git 同。
                left_start: if left_len == 0 {
                    first.li
                } else {
                    first.li + 1
                },
                left_len,
                right_start: if right_len == 0 {
                    first.ri
                } else {
                    first.ri + 1
                },
                right_len,
                lines,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 造一棵树：`(相对路径, 内容)`。
    fn tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
        }
    }

    /// 10 行文本，第 `n` 行（1 起）换成 `repl`。
    fn ten_lines(replace: &[(usize, &str)]) -> String {
        let mut v: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
        for (n, repl) in replace {
            v[n - 1] = (*repl).to_string();
        }
        v.join("\n")
    }

    #[test]
    fn 两棵相同的树没有条目且判定为一致() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        tree(&a, &[("SKILL.md", "same"), ("sub/x.txt", "same too")]);
        tree(&b, &[("SKILL.md", "same"), ("sub/x.txt", "same too")]);

        let r = diff_trees(&a, &b, "left", "right", &DiffOptions::default()).unwrap();
        assert!(r.identical);
        assert!(r.entries.is_empty(), "{:?}", r.entries);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);

        // include_same 打开时同样的两棵树要把 Same 全列出来，但仍判一致。
        let opts = DiffOptions {
            include_same: true,
            ..Default::default()
        };
        let full = diff_trees(&a, &b, "left", "right", &opts).unwrap();
        assert!(full.identical);
        assert_eq!(full.entries.len(), 2);
        assert!(full.entries.iter().all(|e| e.change == Change::Same));
    }

    #[test]
    fn 单侧文件与内容不同的文件各归各位() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        tree(
            &a,
            &[("both.txt", "A"), ("onlyA.txt", "x"), ("keep.txt", "k")],
        );
        tree(
            &b,
            &[("both.txt", "B"), ("onlyB.txt", "y"), ("keep.txt", "k")],
        );

        let r = diff_trees(&a, &b, "left", "right", &DiffOptions::default()).unwrap();
        assert!(!r.identical);
        assert_eq!(r.entries.len(), 3, "{:?}", r.entries);

        let by = |k: &str| r.entries.iter().find(|e| e.key == k).unwrap();
        assert_eq!(by("both.txt").change, Change::Changed);
        assert!(by("both.txt").left.is_some() && by("both.txt").right.is_some());

        assert_eq!(by("onlyA.txt").change, Change::OnlyLeft);
        assert!(by("onlyA.txt").left.is_some());
        assert!(by("onlyA.txt").right.is_none());

        assert_eq!(by("onlyB.txt").change, Change::OnlyRight);
        assert!(by("onlyB.txt").left.is_none());
        assert!(by("onlyB.txt").right.is_some());

        // 键有序：两次运行的结果本身可以对拍。
        let keys: Vec<&str> = r.entries.iter().map(|e| e.key.as_str()).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn 文本改动带三行上下文而大二进制不做行级() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        tree(&a, &[("t.txt", &ten_lines(&[]))]);
        tree(&b, &[("t.txt", &ten_lines(&[(5, "CHANGED")]))]);
        // 1 MB 且非 UTF-8：超过默认 256 KiB 上限。
        fs::write(a.join("blob.bin"), vec![0xffu8; 1024 * 1024]).unwrap();
        fs::write(b.join("blob.bin"), vec![0xfeu8; 1024 * 1024]).unwrap();

        let r = diff_trees(&a, &b, "left", "right", &DiffOptions::default()).unwrap();
        let by = |k: &str| r.entries.iter().find(|e| e.key == k).unwrap();

        assert_eq!(by("blob.bin").change, Change::Changed);
        assert!(by("blob.bin").hunks.is_none(), "大二进制不该做行级 diff");

        let hunks = by("t.txt").hunks.as_ref().unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0].lines,
            vec![
                " line 2", " line 3", " line 4", "-line 5", "+CHANGED", " line 6", " line 7",
                " line 8",
            ]
        );
        assert_eq!((hunks[0].left_start, hunks[0].left_len), (2, 7));
        assert_eq!((hunks[0].right_start, hunks[0].right_len), (2, 7));
    }

    #[test]
    fn node_modules_的差异默认不可见() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        tree(
            &a,
            &[("SKILL.md", "same"), ("node_modules/dep/index.js", "v1")],
        );
        tree(
            &b,
            &[("SKILL.md", "same"), ("node_modules/dep/index.js", "v2")],
        );

        let r = diff_trees(&a, &b, "left", "right", &DiffOptions::default()).unwrap();
        assert!(r.identical, "{:?}", r.entries);

        // 不剪枝就看得见：证明「看不见」来自剪枝而不是根本没走到。
        let opts = DiffOptions {
            prune_dirs: Vec::new(),
            ..Default::default()
        };
        let raw = diff_trees(&a, &b, "left", "right", &opts).unwrap();
        assert_eq!(raw.entries.len(), 1);
        assert_eq!(raw.entries[0].key, "node_modules/dep/index.js");
    }

    #[test]
    #[cfg(unix)]
    fn 符号链接比的是目标字符串() {
        use std::os::unix::fs::symlink;

        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        tree(&a, &[("real.txt", "content")]);
        tree(&b, &[("real.txt", "content")]);
        symlink("real.txt", a.join("same.lnk")).unwrap();
        symlink("real.txt", b.join("same.lnk")).unwrap();
        symlink("real.txt", a.join("moved.lnk")).unwrap();
        // 目标字符串不同，但指向的内容完全一样：跟随的话会判成 Same，
        // 这里要的正是「链接本身变了」。
        symlink("./real.txt", b.join("moved.lnk")).unwrap();

        let opts = DiffOptions {
            include_same: true,
            ..Default::default()
        };
        let r = diff_trees(&a, &b, "left", "right", &opts).unwrap();
        let by = |k: &str| r.entries.iter().find(|e| e.key == k).unwrap();
        assert_eq!(by("same.lnk").change, Change::Same);
        assert_eq!(by("moved.lnk").change, Change::Changed);
        assert_eq!(by("moved.lnk").left.as_deref(), Some("symlink -> real.txt"));
        assert_eq!(
            by("moved.lnk").right.as_deref(),
            Some("symlink -> ./real.txt")
        );
        assert!(!r.identical);
    }

    #[test]
    fn diff_values_报字段路径而不是行号() {
        let left = json!({
            "command": "npx",
            "env": { "API_KEY": "abc", "SHARED": "1" },
            "enabled": true,
        });
        let right = json!({
            "command": ["npx", "-y"],
            "env": { "SHARED": "1" },
            "enabled": true,
        });

        let r = diff_values(&left, &right, "a", "b", &DiffOptions::default());
        assert!(!r.identical);
        let by = |k: &str| r.entries.iter().find(|e| e.key == k).unwrap();

        // 标量 vs 数组：报成 `command` 这一条，不是 command / command.0 / command.1 三条。
        assert_eq!(by("command").change, Change::Changed);
        assert_eq!(by("command").left.as_deref(), Some("npx"));
        assert_eq!(by("command").right.as_deref(), Some(r#"["npx","-y"]"#));

        assert_eq!(by("env.API_KEY").change, Change::OnlyLeft);
        assert_eq!(by("env.API_KEY").left.as_deref(), Some("abc"));
        assert!(by("env.API_KEY").right.is_none());

        // 相同的字段默认不进结果。
        assert_eq!(r.entries.len(), 2, "{:?}", r.entries);

        let opts = DiffOptions {
            include_same: true,
            ..Default::default()
        };
        let full = diff_values(&left, &right, "a", "b", &opts);
        assert!(!full.identical);
        assert!(
            full.entries
                .iter()
                .any(|e| e.key == "enabled" && e.change == Change::Same)
        );

        // 完全相同的两侧判一致。
        let same = diff_values(&left, &left, "a", "b", &DiffOptions::default());
        assert!(same.identical);
        assert!(same.entries.is_empty());
    }

    #[test]
    fn diff_values_数组按下标配对() {
        let left = json!({ "args": ["a", "b", "c"] });
        let right = json!({ "args": ["a", "B"] });
        let r = diff_values(&left, &right, "l", "r", &DiffOptions::default());
        let keys: Vec<&str> = r.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["args.1", "args.2"]);
        assert_eq!(r.entries[0].change, Change::Changed);
        assert_eq!(r.entries[1].change, Change::OnlyLeft);
    }

    #[test]
    fn diff_text_两处相隔的改动产出两个_hunk() {
        let mut left: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        let mut right = left.clone();
        right[4] = "FIRST EDIT".to_string();
        right[24] = "SECOND EDIT".to_string();
        let hunks = diff_text(&left.join("\n"), &right.join("\n"));
        assert_eq!(hunks.len(), 2, "{hunks:#?}");
        assert!(hunks[0].lines.contains(&"+FIRST EDIT".to_string()));
        assert!(hunks[1].lines.contains(&"+SECOND EDIT".to_string()));
        assert_eq!(hunks[0].left_start, 2);
        assert_eq!(hunks[1].left_start, 22);

        // 两处改动之间恰好 2×CONTEXT 行没变：上下文接上，并成一个 hunk。
        left = (1..=30).map(|i| format!("line {i}")).collect();
        right = left.clone();
        right[9] = "X".to_string();
        right[16] = "Y".to_string(); // 中间 6 行不变
        assert_eq!(diff_text(&left.join("\n"), &right.join("\n")).len(), 1);
    }

    #[test]
    fn diff_text_的空输入与相同输入不产出空_hunk() {
        assert!(diff_text("", "").is_empty());
        assert!(diff_text("a\nb\n", "a\nb\n").is_empty());
        // 只差结尾换行：逐行看没有差异，不该造一个 hunk 出来。
        assert!(diff_text("a\nb", "a\nb\n").is_empty());

        // 空 -> 非空：git 的 `@@ -0,0 +1,2 @@`。
        let h = diff_text("", "x\ny\n");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].left_start, h[0].left_len), (0, 0));
        assert_eq!((h[0].right_start, h[0].right_len), (1, 2));
        assert_eq!(h[0].lines, vec!["+x", "+y"]);

        // 非空 -> 空。
        let h = diff_text("x\ny\n", "");
        assert_eq!((h[0].left_start, h[0].left_len), (1, 2));
        assert_eq!((h[0].right_start, h[0].right_len), (0, 0));
        assert_eq!(h[0].lines, vec!["-x", "-y"]);
    }

    #[test]
    fn diff_text_求的是最短编辑脚本() {
        // 中间插入一段：Myers 应该只报插入，不该把后面的行全部重写。
        let left = "a\nb\nc";
        let right = "a\nX\nY\nb\nc";
        let h = diff_text(left, right);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].lines, vec![" a", "+X", "+Y", " b", " c"]);

        // 交错改动：删一行插一行，两侧长度不同（delta 为奇数的分支）。
        let h = diff_text("a\nb\nc\nd", "a\nc\nd\ne");
        let joined = h[0].lines.join("|");
        assert_eq!(joined, " a|-b| c| d|+e", "{joined}");

        // 完全不同的两段：一段删一段插，行数守恒。
        let h = diff_text("1\n2\n3", "7\n8\n9\n0");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].lines.iter().filter(|l| l.starts_with('-')).count(), 3);
        assert_eq!(h[0].lines.iter().filter(|l| l.starts_with('+')).count(), 4);
    }

    #[test]
    fn diff_text_的分块与_git_diff_u3_逐字对齐() {
        // 期望值是 `git diff --no-index -U3` 在同一对文件上的真实输出
        // （2026-08-11 实测）：40 行文本，改一行 / 删一行 / 插一行 / 再改一行。
        // 这条测试锁的是「节奏」——hunk 怎么切、上下文取几行、行号怎么算。
        let left: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        let mut right = left.clone();
        right[2] = "line 3 EDITED".to_string();
        right.remove(10);
        right.insert(20, "INSERTED".to_string());
        right[35] = "line 36 EDITED".to_string();

        let h = diff_text(
            &format!("{}\n", left.join("\n")),
            &format!("{}\n", right.join("\n")),
        );
        let heads: Vec<(usize, usize, usize, usize)> = h
            .iter()
            .map(|x| (x.left_start, x.left_len, x.right_start, x.right_len))
            .collect();
        assert_eq!(
            heads,
            vec![(1, 6, 1, 6), (8, 7, 8, 6), (19, 6, 18, 7), (33, 7, 33, 7)],
            "{h:#?}"
        );
        assert_eq!(h[1].lines.iter().filter(|l| *l == "-line 11").count(), 1);
        assert_eq!(h[2].lines.iter().filter(|l| *l == "+INSERTED").count(), 1);
    }
}
