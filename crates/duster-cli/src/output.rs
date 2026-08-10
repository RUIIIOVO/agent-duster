//! CLI 输出契约：全部命令的退出码、JSON 信封、表格与字节格式化都收敛到这里。
//!
//! # stdout / stderr 约定
//!
//! - **stdout 只放结果**：人类模式下是表格/摘要，`--json` 模式下是一行紧凑 JSON 信封。
//!   下游可以放心 `duster ... | jq`，stdout 永远可解析。
//! - **stderr 放过程**：进度、提示、警告、错误的人话描述全部走 stderr。
//!   `--json` 模式下出错时，信封写 stdout（机器读），同一条错误的人话写 stderr（人读）。
//!
//! # 退出码表
//!
//! | 码 | 常量 | 含义 |
//! |----|------|------|
//! | 0 | [`EXIT_OK`] | 成功 |
//! | 1 | [`EXIT_ERROR`] | 一般错误（IO 失败、解析失败等） |
//! | 2 | [`EXIT_USAGE`] | 用法错误（clap 默认，参数/子命令不合法） |
//! | 3 | [`EXIT_PARTIAL`] | 部分成功（如 scan 某 agent 失败但整体继续，细节进 warnings） |
//! | 4 | [`EXIT_CONFIRM_DENIED`] | 需要确认被拒绝 / dry-run 仅预览未执行（留给 clean） |
//! | 5 | [`EXIT_LOCKED`] | 锁冲突（agent 正在运行 / 索引被其他进程占用） |
//!
//! # `--json` 信封 schema
//!
//! 所有命令统一输出下面这一层信封，命令私有数据只填 `data`：
//!
//! ```json
//! {
//!   "ok": true,
//!   "command": "scan",
//!   "data": {},
//!   "warnings": ["某 agent 解析失败已跳过"],
//!   "error": null
//! }
//! ```
//!
//! - `ok`：`error == null` 时为 `true`；有 warnings 不影响 `ok`。
//! - `command`：子命令名（`scan` / `status` / `clean` ...）。
//! - `data`：命令私有结构，成功时由各命令自定义；出错时为 `null`。
//! - `warnings`：非致命问题列表，永远是数组（可能为空）。
//! - `error`：`null` 或 `{"code": "机器可读短码", "message": "人话"}`。
//!   `code` 建议与退出码语义对应，如 `"partial"` / `"locked"` / `"io"`。

use serde::Serialize;
use std::io::Write;

// ---------------------------------------------------------------------------
// 退出码
// ---------------------------------------------------------------------------

/// 成功。
pub const EXIT_OK: i32 = 0;
/// 一般错误。
pub const EXIT_ERROR: i32 = 1;
/// 用法错误（clap 解析失败时的默认退出码，列在这里只为文档完整）。
pub const EXIT_USAGE: i32 = 2;
/// 部分成功：整体流程走完，但个别子项失败（细节写进 warnings）。
pub const EXIT_PARTIAL: i32 = 3;
/// 需要确认被拒绝 / dry-run 仅预览未执行。
pub const EXIT_CONFIRM_DENIED: i32 = 4;
/// 锁冲突：目标 agent 运行中或索引被其他进程占用。
pub const EXIT_LOCKED: i32 = 5;

// ---------------------------------------------------------------------------
// 输出模式
// ---------------------------------------------------------------------------

/// 输出模式：由全局 `--json` 旗标决定，命令内据此二选一，不做混合输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// 人类模式：stdout 表格/摘要，stderr 进度。
    Human,
    /// 机器模式：stdout 一行紧凑 JSON 信封。
    Json,
}

/// 把 `--json` 旗标翻译成 [`OutputMode`]，无状态小胶水。
pub fn is_json_mode(flag: bool) -> OutputMode {
    if flag { OutputMode::Json } else { OutputMode::Human }
}

// ---------------------------------------------------------------------------
// JSON 信封
// ---------------------------------------------------------------------------

/// 信封里的错误对象：`code` 机器可读短码，`message` 人话。
#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: &'a str,
    message: &'a str,
}

/// 统一信封。字段顺序即输出顺序（serde_json 开了 preserve_order）。
#[derive(Debug, Serialize)]
struct Envelope<'a, T: Serialize> {
    ok: bool,
    command: &'a str,
    data: Option<&'a T>,
    warnings: &'a [String],
    error: Option<ErrorBody<'a>>,
}

/// 渲染成功信封为一行紧凑 JSON（不含换行）。
fn render_json<T: Serialize>(command: &str, data: &T, warnings: &[String]) -> String {
    serde_json::to_string(&Envelope {
        ok: true,
        command,
        data: Some(data),
        warnings,
        error: None,
    })
    .expect("信封序列化不应失败：data 由命令自身定义且不含非法值")
}

/// 渲染错误信封为一行紧凑 JSON（不含换行）。
fn render_json_error(command: &str, code: &str, message: &str) -> String {
    serde_json::to_string(&Envelope::<'_, ()> {
        ok: false,
        command,
        data: None,
        warnings: &[],
        error: Some(ErrorBody { code, message }),
    })
    .expect("错误信封序列化不应失败")
}

/// 成功输出：把命令私有数据装进统一信封，写 stdout 一行紧凑 JSON。
pub fn emit_json<T: Serialize>(command: &str, data: &T, warnings: &[String]) {
    println!("{}", render_json(command, data, warnings));
}

/// 错误输出：信封写 stdout（机器读），同一条错误的人话同时写 stderr（人读）。
pub fn emit_json_error(command: &str, code: &str, message: &str) {
    println!("{}", render_json_error(command, code, message));
    let _ = writeln!(std::io::stderr(), "duster {command}: 错误[{code}] {message}");
}

// ---------------------------------------------------------------------------
// 表格
// ---------------------------------------------------------------------------

/// 判断字符是否按「东亚宽字符」占两列。
///
/// 近似实现：只收录常用东亚宽/全角区间（CJK 统一表意及扩展、假名、谚文、
/// 全角标点/字母、CJK 兼容），外加常用 emoji 区。不追求 UAX #11 全表精确，
/// 但覆盖中文/日文/韩文列对齐的实际需求。
fn is_wide_char(c: char) -> bool {
    matches!(u32::from(c),
        0x1100..=0x115F          // 谚文字母（初声）
        | 0x2E80..=0x303E        // CJK 部首、康熙部首、CJK 符号与标点
        | 0x3041..=0x33FF        // 平/片假名、注音、谚文兼容、CJK 括号/兼容
        | 0x3400..=0x4DBF        // CJK 扩展 A
        | 0x4E00..=0x9FFF        // CJK 统一表意
        | 0xA000..=0xA4CF        // 彝文
        | 0xAC00..=0xD7A3        // 谚文音节
        | 0xF900..=0xFAFF        // CJK 兼容表意
        | 0xFE30..=0xFE4F        // CJK 兼容形式
        | 0xFF00..=0xFF60        // 全角 ASCII、全角标点
        | 0xFFE0..=0xFFE6        // 全角符号（￥ 等）
        | 0x1F300..=0x1FAFF      // 常用 emoji（近似按宽 2）
        | 0x20000..=0x2FFFD      // CJK 扩展 B–F
        | 0x30000..=0x3FFFD      // CJK 扩展 G
    )
}

/// 字符串的终端显示宽度（东亚宽字符按 2，其余按 1，近似值）。
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| if is_wide_char(c) { 2 } else { 1 }).sum()
}

/// 手写等宽列表格：列宽按内容自适应，CJK 按显示宽度 2 对齐。
///
/// 渲染格式：表头一行、`-` 分隔线一行、数据行若干；列间两个空格，
/// 每行末尾不留补齐空格。整体供人类模式写 stdout。
pub struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// 以表头建表。列数由表头决定，行短于表头的列按空串补齐。
    pub fn new<S: Into<String>>(header: Vec<S>) -> Self {
        Self {
            header: header.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
        }
    }

    /// 追加一行。多出表头的列会被忽略。
    pub fn push_row<S: Into<String>>(&mut self, row: Vec<S>) {
        let mut cells: Vec<String> = row.into_iter().map(Into::into).collect();
        cells.truncate(self.header.len());
        self.rows.push(cells);
    }

    /// 渲染为多行字符串（含末尾换行的行序列，最后一行无多余换行）。
    pub fn render(&self) -> String {
        let cols = self.header.len();
        // 各列取内容最大显示宽度。
        let mut widths = vec![0usize; cols];
        for (i, h) in self.header.iter().enumerate() {
            widths[i] = widths[i].max(display_width(h));
        }
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(display_width(cell));
            }
        }

        let mut out = String::new();
        Self::render_line(&mut out, &self.header, &widths);
        // 分隔线：每列等宽的 '-'，列间两空格。
        let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        out.push('\n');
        Self::render_line(&mut out, &sep, &widths);
        for row in &self.rows {
            out.push('\n');
            Self::render_line(&mut out, row, &widths);
        }
        out
    }

    /// 渲染单行：按显示宽度补空格，行尾不留补齐。
    fn render_line(out: &mut String, cells: &[String], widths: &[usize]) {
        let last = widths.len().saturating_sub(1);
        for (i, width) in widths.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            out.push_str(cell);
            if i < last {
                let pad = width.saturating_sub(display_width(cell));
                for _ in 0..pad {
                    out.push(' ');
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 字节格式化
// ---------------------------------------------------------------------------

/// 1024 进制的人类可读字节数，如 `0 B` / `1023 B` / `1.5 GB`。
///
/// 单位以上保留一位小数，`.0` 会被去掉（`1024` → `1 KB`）。
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let s = format!("{value:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s} {}", UNITS[unit])
}

// ---------------------------------------------------------------------------
// 测试：只守护可观察契约（信封 schema、CJK 对齐、字节边界）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn json_envelope_schema() {
        #[derive(Serialize)]
        struct Data {
            count: u32,
        }
        let line = render_json("scan", &Data { count: 3 }, &["跳过 foo".to_string()]);
        assert!(!line.contains('\n'), "信封必须是一行紧凑 JSON");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], Value::Bool(true));
        assert_eq!(v["command"], "scan");
        assert_eq!(v["data"]["count"], 3);
        assert_eq!(v["warnings"][0], "跳过 foo");
        assert!(v["error"].is_null());
        // 字段顺序即契约顺序（preserve_order）。
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["ok", "command", "data", "warnings", "error"]);
    }

    #[test]
    fn json_error_envelope_schema() {
        let line = render_json_error("clean", "locked", "索引被占用");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ok"], Value::Bool(false));
        assert_eq!(v["command"], "clean");
        assert!(v["data"].is_null());
        assert_eq!(v["warnings"].as_array().unwrap().len(), 0);
        assert_eq!(v["error"]["code"], "locked");
        assert_eq!(v["error"]["message"], "索引被占用");
    }

    #[test]
    fn table_cjk_alignment() {
        let mut t = Table::new(vec!["名称", "size"]);
        t.push_row(vec!["中文技能包", "12"]);
        t.push_row(vec!["ascii", "3456"]);
        let rendered = t.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 4);
        // 每行第二列起始处的显示列必须一致（中文列不错位）。
        let col_starts: Vec<usize> = [
            (lines[0], "size"),
            (lines[2], "12"),
            (lines[3], "3456"),
        ]
        .iter()
        .map(|(line, needle)| {
            let idx = line.rfind(needle).unwrap();
            display_width(&line[..idx])
        })
        .collect();
        assert!(
            col_starts.windows(2).all(|w| w[0] == w[1]),
            "第二列起始显示列不一致: {col_starts:?}\n{rendered}"
        );
        // 行尾不留补齐空格。
        assert!(rendered.lines().all(|l| !l.ends_with(' ')));
    }

    #[test]
    fn human_bytes_boundaries() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1 KB");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(1_610_612_736), "1.5 GB"); // 1.5 GiB
    }

    #[test]
    fn output_mode_glue() {
        assert_eq!(is_json_mode(true), OutputMode::Json);
        assert_eq!(is_json_mode(false), OutputMode::Human);
    }
}
