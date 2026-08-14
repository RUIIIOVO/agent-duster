//! `duster diff <a> <b>`：任意两处并排看。
//!
//! 引擎在 `duster_core::diff`（线性空间 Myers，hunk 与 `git diff -U3` 逐字
//! 对齐），这里只做三件事：把两个路径变成引擎认识的输入、渲染、映射退出码。
//!
//! # 渲染归这里，且只有这一份
//!
//! [`render_diff`] 是**全 CLI 唯一**的 diff 渲染器。`duster mcp diff` 曾直接
//! 调它；那条命令随铺平表撤下后，只剩顶层 `duster diff` 一个入口——`-`/`+`
//! 的读法全 CLI 只有这一种。
//!
//! # 正文顶格，摘要缩进
//!
//! 全 CLI 的人类输出都套两格缩进，diff 正文是唯一的例外：它是 unified diff，
//! 读者的眼睛（和 `patch`）都按顶格解析，缩进两格就既不能复制去打补丁，
//! 也和满世界的 `git diff` 对不上。所以 `---`/`+++`、条目行、hunk 顶格走，
//! 只有最后那行摘要回到两格。
//!
//! # 两种形状，靠空 key 分辨
//!
//! - **目录对目录**：一个条目一行（`+`/`-`/`~` + 相对路径 + 内容标识），
//!   行级明细缩两格挂在它下面；
//! - **文件对文件**：整份内容就是这一对，没有"哪个条目"可言，于是条目的
//!   `key` 为空串，hunk 直接顶格打——输出与 `git diff --no-index` 逐行一致。
//!
//! 空 key 这条约定同时是给 [`render_diff`] 的调用方看的：
//! `diff_values` 产出的条目 key 是字段路径，永远非空，走的是第一种形状。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use console::Style;

use duster_core::diff::{Change, Diff, DiffEntry, DiffOptions, Hunk, diff_text, diff_trees};

use crate::output::{EXIT_OK, OutputMode, display_width, emit_json, human_bytes, muted};
use crate::{fail, plural, render_warnings};

/// `duster diff` 的参数。
#[derive(Args)]
pub struct DiffArgs {
    /// Left side: a file, or a directory
    #[arg(value_name = "A")]
    pub a: PathBuf,
    /// Right side: must be the same kind as the left side
    #[arg(value_name = "B")]
    pub b: PathBuf,
    /// Also list the entries that are identical on both sides
    #[arg(long)]
    pub include_same: bool,
    /// Only say which entries differ, without the line-by-line detail
    #[arg(long)]
    pub no_line_level: bool,
}

/// 比较两个路径。
///
/// 退出码只有「跑成了」与「跑不成」两档：**发现差异不是失败**。
/// `git diff` 用退出码 1 报"有差异"，duster 的 1 是"出错了"（见
/// `output.rs` 的退出码表），沿用 git 的约定会让脚本把一次成功的比较
/// 当成 IO 失败。要机器判定是否一致，读 `--json` 的 `identical`。
pub fn run(mode: OutputMode, args: &DiffArgs) -> i32 {
    let opts = DiffOptions {
        include_same: args.include_same,
        line_level: !args.no_line_level,
        ..DiffOptions::default()
    };
    let diff = match build(&args.a, &args.b, &opts) {
        Ok(d) => d,
        Err(e) => return fail(mode, "diff", &e),
    };
    match mode {
        OutputMode::Json => emit_json("diff", &diff, &diff.warnings),
        OutputMode::Human => render_diff(&diff),
    }
    EXIT_OK
}

/// 按两侧的形态选引擎入口。一边文件一边目录直接拒绝——
/// 「文件与目录的差异」没有任何一种读法是有意义的，猜一个出来只会误导。
fn build(a: &Path, b: &Path, opts: &DiffOptions) -> Result<Diff> {
    let ma = fs::metadata(a).with_context(|| format!("cannot read {}", a.display()))?;
    let mb = fs::metadata(b).with_context(|| format!("cannot read {}", b.display()))?;
    match (ma.is_dir(), mb.is_dir()) {
        (true, true) => diff_trees(a, b, &label(a), &label(b), opts),
        (false, false) => diff_files(a, b, opts),
        (left_is_dir, _) => {
            let (dir, file) = if left_is_dir { (a, b) } else { (b, a) };
            bail!(
                "cannot compare a directory with a file: {} is a directory, {} is a file. \
                 Pass two files or two directories.",
                dir.display(),
                file.display()
            )
        }
    }
}

/// 展示标签：路径原样。用户敲的是什么，表头就是什么。
fn label(p: &Path) -> String {
    p.display().to_string()
}

/// 两个文件：整份内容就是一个条目，`key` 留空。
///
/// 先比字节再谈行：只差一个结尾换行的两份文件在行级 diff 里切不出任何
/// hunk，若就此报"相同"，就是在用户明明看见 `cmp` 说不同的时候撒谎。
/// 所以字节不等一律是 `Changed`，切不出 hunk 就把原因写进 warnings。
fn diff_files(a: &Path, b: &Path, opts: &DiffOptions) -> Result<Diff> {
    let ba = fs::read(a).with_context(|| format!("cannot read {}", a.display()))?;
    let bb = fs::read(b).with_context(|| format!("cannot read {}", b.display()))?;
    let mut warnings: Vec<String> = Vec::new();

    if ba == bb {
        return Ok(Diff {
            left_label: label(a),
            right_label: label(b),
            entries: Vec::new(),
            identical: true,
            warnings,
        });
    }

    let cap = opts.line_level_cap;
    let hunks = if !opts.line_level {
        None
    } else if ba.len() as u64 > cap || bb.len() as u64 > cap {
        warnings.push(format!(
            "no line-by-line detail: one side is over the {} limit for line-level diff",
            human_bytes(cap)
        ));
        None
    } else {
        match (std::str::from_utf8(&ba), std::str::from_utf8(&bb)) {
            (Ok(ta), Ok(tb)) => match diff_text(ta, tb) {
                h if h.is_empty() => {
                    warnings.push(
                        "the two files differ in bytes but on no line — most likely a trailing \
                         newline or a CRLF/LF difference"
                            .to_string(),
                    );
                    None
                }
                h => Some(h),
            },
            _ => {
                warnings.push("no line-by-line detail: this is not UTF-8 text".to_string());
                None
            }
        }
    };

    Ok(Diff {
        left_label: label(a),
        right_label: label(b),
        entries: vec![DiffEntry {
            key: String::new(),
            change: Change::Changed,
            left: None,
            right: None,
            hunks,
        }],
        identical: false,
        warnings,
    })
}

/// 条目键列的最大对齐宽度。超过就不对齐了——为一条 80 字符的深路径把
/// 其余二十行推到屏幕外，是拿所有人的可读性换一行的整齐。
const KEY_PAD_MAX: usize = 44;

/// hunk 挂在条目下时的缩进。
const HUNK_INDENT: &str = "  ";

/// 渲染一次比较。顶层 `duster diff` 唯一入口。
///
/// 顶格打 unified diff 正文，最后一行摘要回到两格缩进（见模块文档）。
/// warnings 由这里统一落 stderr，调用方不要再打一遍。
pub(crate) fn render_diff(diff: &Diff) {
    println!();
    if diff.identical {
        println!(
            "  {}",
            muted().apply_to(format!(
                "No differences — {} and {} are the same",
                diff.left_label, diff.right_label
            ))
        );
        render_warnings(&diff.warnings);
        return;
    }

    println!(
        "{}",
        del_style().apply_to(format!("--- {}", diff.left_label))
    );
    println!(
        "{}",
        add_style().apply_to(format!("+++ {}", diff.right_label))
    );

    let pad = diff
        .entries
        .iter()
        .map(|e| display_width(&e.key))
        .max()
        .unwrap_or(0)
        .min(KEY_PAD_MAX);

    for e in &diff.entries {
        if e.key.is_empty() {
            // 文件对文件：hunk 顶格，输出与 `git diff --no-index` 逐行一致。
            match &e.hunks {
                Some(h) => render_hunks(h, ""),
                None => println!("  {}", muted().apply_to(no_detail_note(e))),
            }
            continue;
        }
        render_entry_line(e, pad);
        if let Some(h) = &e.hunks {
            render_hunks(h, HUNK_INDENT);
        }
    }

    render_summary(diff);
    render_warnings(&diff.warnings);
}

/// 一个条目一行：符号 + 键 + 内容标识。
fn render_entry_line(e: &DiffEntry, pad: usize) {
    let (sign, style) = match e.change {
        Change::OnlyLeft => ("-", del_style()),
        Change::OnlyRight => ("+", add_style()),
        Change::Changed => ("~", Style::new().yellow()),
        Change::Same => ("=", muted()),
    };
    let gap = pad.saturating_sub(display_width(&e.key));
    println!(
        "{} {}{} {}",
        style.apply_to(sign),
        style.apply_to(&e.key),
        " ".repeat(gap),
        muted().apply_to(identity(e))
    );
}

/// 条目的内容标识：只在一侧就是那一侧，两侧都有就是 `左 → 右`。
///
/// 除了把 blake3 摘要缩短之外**不做截断**：MCP 那条路径上这里放的是字段
/// 真值（`npx`、一个 URL、一段命令行），截掉的正好是用户要核对的那一截。
fn identity(e: &DiffEntry) -> String {
    match (&e.left, &e.right) {
        (Some(l), Some(r)) if l == r => short_id(l),
        (Some(l), Some(r)) => format!("{} → {}", short_id(l), short_id(r)),
        (Some(l), None) => short_id(l),
        (None, Some(r)) => short_id(r),
        (None, None) => String::new(),
    }
}

/// blake3 摘要的展示形态：留前 12 位。
///
/// 一个 64 位十六进制串在终端里占掉整行，而它对人的全部用处只有
/// 「这两个一样吗」——前 12 位（48 bit）已经远超肉眼对比的需要。
/// 其余形态（`symlink -> ...`、MCP 的字段值）原样透出。
fn short_id(s: &str) -> String {
    const PREFIX: &str = "blake3:";
    const KEEP: usize = 12;
    match s.strip_prefix(PREFIX) {
        Some(hex) if hex.len() > KEEP && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            format!("{PREFIX}{}…", &hex[..KEEP])
        }
        _ => s.to_string(),
    }
}

/// 有差异却没有行级明细时的一句解释。
///
/// 空着比说错更糟：用户看见 `--- / +++` 却一行 hunk 都没有，只会以为
/// duster 坏了。具体原因（超限 / 非文本 / 只差换行 / `--no-line-level`）
/// 在 warnings 里，这里只负责让那片空白有个说法。
fn no_detail_note(e: &DiffEntry) -> String {
    let what = identity(e);
    if what.is_empty() {
        "The two sides differ, but there is no line-by-line detail to show".to_string()
    } else {
        format!("The two sides differ, but there is no line-by-line detail to show: {what}")
    }
}

/// hunk 逐段打印。`indent` 是挂在条目下时的缩进，顶格时传空串。
fn render_hunks(hunks: &[Hunk], indent: &str) {
    let head = Style::new().cyan();
    for h in hunks {
        println!("{indent}{}", head.apply_to(hunk_header(h)));
        for line in &h.lines {
            let style = match line.as_bytes().first() {
                Some(b'+') => add_style(),
                Some(b'-') => del_style(),
                _ => muted(),
            };
            println!("{indent}{}", style.apply_to(line));
        }
    }
}

/// `@@ -1,4 +1,6 @@`。单行的一侧省掉 `,1`，与 `git diff` 逐字一致。
fn hunk_header(h: &Hunk) -> String {
    format!(
        "@@ -{} +{} @@",
        span(h.left_start, h.left_len),
        span(h.right_start, h.right_len)
    )
}

/// 一侧的 `起始[,行数]`。
fn span(start: usize, len: usize) -> String {
    if len == 1 {
        start.to_string()
    } else {
        format!("{start},{len}")
    }
}

/// 收尾一行：总数在前，分类明细随后。
///
/// 为零的类别不出现：`0 only in /Users/…/skills/stitch-skill` 这种句子
/// 又长又没有信息，读者要找的是"有哪几类差异"，不是"哪几类没有"。
/// `Same` 只在 `--include-same` 时才可能非零，所以同一条规则就够。
fn render_summary(diff: &Diff) {
    // 文件对文件只有一个空 key 条目，"1 difference · 1 changed" 是废话。
    if diff.entries.len() == 1 && diff.entries[0].key.is_empty() {
        return;
    }
    let count = |c: Change| diff.entries.iter().filter(|e| e.change == c).count();
    let parts: Vec<String> = [
        (count(Change::Changed), "changed".to_string()),
        (
            count(Change::OnlyLeft),
            format!("only in {}", diff.left_label),
        ),
        (
            count(Change::OnlyRight),
            format!("only in {}", diff.right_label),
        ),
        (count(Change::Same), "identical".to_string()),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    println!();
    println!(
        "  {} {}",
        // `--include-same` 下 Same 也在 entries 里，但它显然不是一处差异。
        console::style(plural(
            diff.entries.len() - count(Change::Same),
            "difference"
        ))
        .bold(),
        muted().apply_to(format!("· {}", parts.join(" · ")))
    );
}

/// 新增侧的配色。
fn add_style() -> Style {
    Style::new().green()
}

/// 删除侧的配色。
fn del_style() -> Style {
    Style::new().red()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 造一棵树：`(相对路径, 内容)`。
    fn tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
    }

    /// 目录对目录：三类条目各一行，符号与键都在，行级明细挂在改动那条下面。
    #[test]
    fn 树比较_每类差异各一行且明细挂在条目下() {
        let tmp = TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        tree(&a, &[("SKILL.md", "name: foo\nbody\n"), ("gone.md", "x\n")]);
        tree(&b, &[("SKILL.md", "name: bar\nbody\n"), ("new.md", "y\n")]);

        let diff = build(&a, &b, &DiffOptions::default()).unwrap();
        assert!(!diff.identical);
        let keys: Vec<(&str, Change)> = diff
            .entries
            .iter()
            .map(|e| (e.key.as_str(), e.change))
            .collect();
        assert_eq!(
            keys,
            [
                ("SKILL.md", Change::Changed),
                ("gone.md", Change::OnlyLeft),
                ("new.md", Change::OnlyRight),
            ]
        );

        // 条目行：符号 + 键 + 内容标识（blake3 已缩短，不占满一行）。
        let changed = &diff.entries[0];
        let id = identity(changed);
        assert!(id.contains(" → "), "{id}");
        assert!(
            id.split(" → ").all(|s| s.len() < 24),
            "blake3 摘要应缩短: {id}"
        );

        // 行级明细：改动那条有 hunk，只在一侧的没有（无从比起）。
        let hunks = changed.hunks.as_ref().expect("changed 应带行级明细");
        assert_eq!(hunk_header(&hunks[0]), "@@ -1,2 +1,2 @@");
        assert_eq!(hunks[0].lines, ["-name: foo", "+name: bar", " body"]);
        assert!(diff.entries[1].hunks.is_none());
    }

    /// `--no-line-level` 只要条目行，不要 hunk；`--include-same` 把相同的也带上。
    #[test]
    fn 树比较_旗标控制明细与相同项() {
        let tmp = TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        tree(&a, &[("x.md", "1\n"), ("same.md", "s\n")]);
        tree(&b, &[("x.md", "2\n"), ("same.md", "s\n")]);

        let terse = build(
            &a,
            &b,
            &DiffOptions {
                line_level: false,
                ..DiffOptions::default()
            },
        )
        .unwrap();
        assert_eq!(terse.entries.len(), 1);
        assert!(terse.entries[0].hunks.is_none());

        let full = build(
            &a,
            &b,
            &DiffOptions {
                include_same: true,
                ..DiffOptions::default()
            },
        )
        .unwrap();
        let same: Vec<&str> = full
            .entries
            .iter()
            .filter(|e| e.change == Change::Same)
            .map(|e| e.key.as_str())
            .collect();
        assert_eq!(same, ["same.md"]);
    }

    /// 文件对文件：条目 key 为空（hunk 顶格打），hunk 头与 git 同形。
    #[test]
    fn 文件比较_空_key_且_hunk_头与_git_同形() {
        let tmp = TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a.md"), tmp.path().join("b.md"));
        fs::write(&a, "one\n").unwrap();
        fs::write(&b, "two\n").unwrap();

        let diff = build(&a, &b, &DiffOptions::default()).unwrap();
        assert_eq!(diff.entries.len(), 1);
        assert!(diff.entries[0].key.is_empty());
        let h = diff.entries[0].hunks.as_ref().unwrap();
        // 单行的一侧省掉 `,1`，与 `git diff` 逐字一致。
        assert_eq!(hunk_header(&h[0]), "@@ -1 +1 @@");
    }

    /// 只差一个结尾换行：字节不同就必须报不同，并说清为什么没有行级明细。
    /// 报"相同"是在用户明明能用 `cmp` 看出区别的时候撒谎。
    #[test]
    fn 文件比较_只差结尾换行也算不同并解释原因() {
        let tmp = TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::write(&a, "x\n").unwrap();
        fs::write(&b, "x").unwrap();

        let diff = build(&a, &b, &DiffOptions::default()).unwrap();
        assert!(!diff.identical);
        assert!(diff.entries[0].hunks.is_none());
        assert!(
            diff.warnings.iter().any(|w| w.contains("trailing newline")),
            "{:?}",
            diff.warnings
        );
        // 那片空白要有个说法，否则用户会以为 duster 坏了。
        assert!(no_detail_note(&diff.entries[0]).contains("no line-by-line detail"));
    }

    /// 逐字节相同 = identical，条目为空（渲染成一行"没有差异"）。
    #[test]
    fn 文件比较_完全相同时_identical() {
        let tmp = TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        fs::write(&a, "same\n").unwrap();
        fs::write(&b, "same\n").unwrap();
        let diff = build(&a, &b, &DiffOptions::default()).unwrap();
        assert!(diff.identical);
        assert!(diff.entries.is_empty());
    }

    /// 一边文件一边目录：拒绝，并说清哪边是哪种。
    #[test]
    fn 文件与目录混比_报错并指名两侧形态() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("d");
        let file = tmp.path().join("f");
        fs::create_dir(&dir).unwrap();
        fs::write(&file, "x").unwrap();
        let err = build(&dir, &file, &DiffOptions::default()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("is a directory"), "{msg}");
        assert!(msg.contains("is a file"), "{msg}");
        // 反过来传也要说清同样的事实。
        let back = format!(
            "{:#}",
            build(&file, &dir, &DiffOptions::default()).unwrap_err()
        );
        assert!(
            back.contains("is a directory") && back.contains("is a file"),
            "{back}"
        );
    }

    /// 路径不存在时报的是"读不了这个路径"，不是一句泛泛的 IO 错。
    #[test]
    fn 路径不存在时报出是哪一个() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        let err = build(&missing, tmp.path(), &DiffOptions::default()).unwrap_err();
        assert!(format!("{err:#}").contains("nope"));
    }
}
