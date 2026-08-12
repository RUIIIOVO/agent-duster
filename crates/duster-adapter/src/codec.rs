//! Codec:纯格式读写,与 agent 无关。读方向支持 json / jsonc / toml,
//! 写方向支持对 json / jsonc / toml 的保守原地改写。
//!
//! 职责边界:只认识文件格式,不认识任何 agent 的目录约定。
//! 读取一律经 [`read_to_string_capped`] 防御超大文件。
//!
//! JSON 一律走 [`strip_json_comments`] 预处理:真实世界里
//! `~/.copilot/config.json`、`~/.cursor/argv.json`、`~/.qoder/argv.json`
//! 开头就是 `//` 注释,扩展名却是 `.json`,直接喂给 serde_json 必然失败。
//!
//! 写方向的四个入口([`set_json_pointer`] / [`remove_json_pointer`] /
//! [`set_toml_path`] / [`remove_toml_path`])一律「原文进、新文本出」,不碰
//! 文件系统:M2 的写契约要求先过 `guard::check`、再 `snapshot_file`、最后才
//! 原子写,这个顺序只有调用方能保证,所以 IO 必须留在 codec 之外。
//! 「保守」的含义是字节级的:没被点名的注释、缩进、键序、引号风格、换行符、
//! 末尾换行、BOM 一个都不许动。

use std::borrow::Cow;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

/// 解析后的配置文档。按格式分列,不做 trait object 抹平——
/// 调用方(mapper)本来就要区分格式做不同下钻。
#[derive(Debug, Clone)]
pub enum Doc {
    /// JSON 文档(serde_json 已开 preserve_order,键序天然保留)。
    Json(serde_json::Value),
    /// TOML 文档。
    Toml(toml::Value),
}

/// 无扩展名嗅探时的默认大小上限(8 MiB),足够覆盖任何正常配置文件。
const DEFAULT_CAP: u64 = 8 * 1024 * 1024;

/// 读取并解析一个配置文件。
///
/// 按扩展名分派:`.json` / `.jsonc` -> JSON,`.toml` -> TOML;
/// 无扩展名或不认识的扩展名 -> 先试 JSON 再试 TOML,
/// 两者都失败时把两个解析错误合并成一条人话报错。
///
/// `.jsonc` 单列而不是落进嗅探分支:嗅探失败会吐出一条同时抱怨
/// JSON 和 TOML 的两头报错,对一个明摆着是 JSONC 的文件毫无帮助。
///
/// 三条 JSON 路径(`.json` / `.jsonc` / 嗅探)一律经
/// [`strip_json_comments`],所以带注释、带尾逗号的现实配置都能读。
pub fn read_file(path: &Path) -> anyhow::Result<Doc> {
    let raw = read_to_string_capped(path, DEFAULT_CAP)?;
    // BOM 在这里一次性剥掉:Windows 编辑器写出来的 json/toml 都可能带,
    // 两个 parser 都会把它当成非法起始字符。`mapper/skill.rs` 的
    // `parse_skill_md` 对 Markdown 做的是同一件事。
    let text = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("json") | Some("jsonc") => {
            let v = parse_json_lenient(text)
                .with_context(|| format!("failed to parse JSON: {}", path.display()))?;
            Ok(Doc::Json(v))
        }
        Some("toml") => {
            let v = toml::from_str(text)
                .with_context(|| format!("failed to parse TOML: {}", path.display()))?;
            Ok(Doc::Toml(v))
        }
        _ => {
            // 嗅探:先 JSON 后 TOML。
            let json_err = match parse_json_lenient(text) {
                Ok(v) => return Ok(Doc::Json(v)),
                Err(e) => e,
            };
            let toml_err = match toml::from_str(text) {
                Ok(v) => return Ok(Doc::Toml(v)),
                Err(e) => e,
            };
            bail!(
                "unrecognized format for {}: JSON parse failed ({json_err}); TOML parse failed ({toml_err})",
                path.display()
            );
        }
    }
}

/// 按 JSONC 宽松规则解析一段 JSON 文本。
///
/// 只做「剥注释 + 容忍尾逗号」这一层,其余交给 serde_json:
/// 真正畸形的文件仍旧由 serde_json 报错,报错文案不变。
fn parse_json_lenient(text: &str) -> serde_json::Result<serde_json::Value> {
    serde_json::from_str(&strip_json_comments(text))
}

/// 剥掉 JSONC 的 `//` 行注释与 `/* … */` 块注释,并容忍 `}` / `]` 前的尾逗号。
///
/// **字符串感知**:`"http://example.com"` 里的 `//` 不是注释,
/// `"/*.rs"` 里的 `/*` 也不是;转义引号 `\"` 不会让扫描器错位。
///
/// **行结构守恒**:行注释本来就不含换行,直接删;块注释按其内部换行
/// 数量补回等量 `\n`。这样 serde_json 报的行号仍然指向原文件的那一行——
/// 指错行的报错比没有行号更糟,它会把人骗去看无辜的一行。
/// (列号只在被改写的那一行会偏,行号永远准。)
///
/// **零拷贝**:绝大多数配置文件根本没有注释,这种情况下返回
/// [`Cow::Borrowed`],一次分配都不做。
pub fn strip_json_comments(src: &str) -> Cow<'_, str> {
    let bytes = src.as_bytes();
    // 惰性构造:只有真的要删东西时才分配。`copied` 是「已搬进 out 的前缀」边界。
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    let mut i = 0usize;

    // 按字节扫描是安全的:UTF-8 多字节序列的每个字节都 >= 0x80,
    // 不可能与这里比较的任何 ASCII 字符相等,切片也永远落在字符边界上。
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = skip_string(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                // 换行留在原地,行号不变。
                drop_span(src, &mut out, &mut copied, start, i, 0);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start = i;
                let mut newlines = 0usize;
                i += 2;
                loop {
                    if i >= bytes.len() {
                        // 未闭合的块注释:吃到文件尾,让 serde_json 去抱怨文档被截断。
                        break;
                    }
                    if bytes[i] == b'\n' {
                        newlines += 1;
                    }
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                drop_span(src, &mut out, &mut copied, start, i, newlines);
            }
            b',' => {
                // 尾逗号:VS Code 系(cursor / qoder 的 argv.json 就是它生成的)允许。
                // 判定要跨过后面的空白和注释,否则 `,\n// x\n}` 会漏判。
                if matches!(next_significant(bytes, i + 1), Some(b'}') | Some(b']')) {
                    drop_span(src, &mut out, &mut copied, i, i + 1, 0);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    match out {
        Some(mut s) => {
            s.push_str(&src[copied..]);
            Cow::Owned(s)
        }
        None => Cow::Borrowed(src),
    }
}

/// 把 `src[start..end]` 从输出里抹掉,替换成 `newlines` 个换行。
/// 首次调用时才真正分配缓冲区。
fn drop_span(
    src: &str,
    out: &mut Option<String>,
    copied: &mut usize,
    start: usize,
    end: usize,
    newlines: usize,
) {
    let buf = out.get_or_insert_with(|| String::with_capacity(src.len()));
    buf.push_str(&src[*copied..start]);
    for _ in 0..newlines {
        buf.push('\n');
    }
    *copied = end;
}

/// 给定字符串字面量起始双引号的下标,返回闭合双引号之后的下标。
/// 未闭合时返回 `bytes.len()`。
fn skip_string(bytes: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            // 反斜杠吃掉下一个字节,所以 `\"` 不会被当成收尾引号。
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// 从 `i` 开始跳过空白与注释,返回下一个有意义的字节。
/// 仅用于尾逗号判定,不需要处理字符串——注释与空白之后遇到的第一个
/// 非空白字节就是答案,哪怕它是个双引号也直接返回。
fn next_significant(bytes: &[u8], mut i: usize) -> Option<u8> {
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            c => return Some(c),
        }
    }
    None
}

/// JSON Pointer 下钻(RFC 6901),直接复用 `Value::pointer`。
///
/// 例:`/mcpServers/foo/command`;空指针 `""` 返回整棵树。
pub fn json_pointer<'a>(v: &'a serde_json::Value, ptr: &str) -> Option<&'a serde_json::Value> {
    v.pointer(ptr)
}

/// TOML 点路径下钻,例 `mcp.servers.foo`。
///
/// 限制:按 `.` 简单切分,**不支持**带引号的段(如 `a."b.c".d`)——
/// 键名本身含 `.` 时无法表达。数组段按十进制下标解释(如 `deps.0.name`)。
/// 空路径返回整棵树。
pub fn toml_path<'a>(v: &'a toml::Value, dotted: &str) -> Option<&'a toml::Value> {
    if dotted.is_empty() {
        return Some(v);
    }
    let mut cur = v;
    for seg in dotted.split('.') {
        cur = match cur {
            toml::Value::Table(t) => t.get(seg)?,
            toml::Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

// ---------------------------------------------------------------------------
// 写方向:保守原地改写
//
// 四个入口一律「原文进、新文本出」,不碰文件系统:快照与原子写归调用方
// (`duster_fs::snapshot` / `duster_fs::atomic::write_atomic`)。把 IO 挡在外面,
// 测试才能廉价地按字节断言「没被要求改的地方一个字节都没动」。
//
// TOML 走 `toml_edit`(它就是为保格式而生的);JSON 不走 serde_json 的序列化——
// 那会把整个文档重排,注释、缩进、键序全丢。JSON 这边自己扫出目标值在原文里的
// 字节区间,然后只在那一段做拼接。注释在目标区间之外,天然不受影响。
// ---------------------------------------------------------------------------

/// 原文里的字节区间,半开 `[start, end)`。
type Span = (usize, usize);

/// 文件的排版风格:缩进单位与换行符。插入的新行必须跟着原文走,
/// 否则一个 tab 缩进的 `~/.cursor/argv.json` 会被掺进空格行。
struct Style {
    /// 一级缩进,`"\t"` 或 N 个空格。
    unit: String,
    /// 换行符,`"\n"` 或 `"\r\n"`。
    le: String,
}

impl Style {
    /// 从原文嗅探风格。缩进取「第一处缩进是 tab 就用 tab,否则取所有缩进行的
    /// 最小正空格数」;探不到任何缩进行时退回 2 空格(JSON 世界的多数派)。
    fn detect(text: &str) -> Self {
        let le = if is_crlf(text) { "\r\n" } else { "\n" };
        let mut min_spaces: Option<usize> = None;
        for line in text.lines() {
            let body = line.trim_start_matches([' ', '\t']);
            if body.is_empty() {
                continue; // 纯空白行不算缩进证据。
            }
            let ws = &line[..line.len() - body.len()];
            if ws.is_empty() {
                continue;
            }
            if ws.starts_with('\t') {
                return Style {
                    unit: "\t".to_string(),
                    le: le.to_string(),
                };
            }
            min_spaces = Some(min_spaces.map_or(ws.len(), |m: usize| m.min(ws.len())));
        }
        Style {
            unit: " ".repeat(min_spaces.unwrap_or(2)),
            le: le.to_string(),
        }
    }
}

/// 在 JSON 文档里写入(或创建)一个 JSON Pointer 指向的值,返回新文本。
///
/// - 目标已存在:只替换值那一段字节,键名、逗号、行尾注释、缩进全部原样保留。
/// - 目标不存在:沿路补出缺失的中间容器(一律建对象——数字段名在这里当普通键,
///   凭空造数组是猜),再把新成员追加到最深的那个已存在容器末尾。
/// - 走穿标量(`/a/b` 而 `a` 是字符串)报错并点名出事的那一段路径。
/// - 空指针 `""` 指的是整篇文档,替换它等于重写全文,与「保守」矛盾,直接拒绝。
///
/// 新值里的对象按原文的缩进单位展开;全是标量的数组保持单行
/// (`"args": ["-y", "pkg"]` 是人手写配置的常态,拆成多行纯属添乱)。
pub fn set_json_pointer(src: &str, ptr: &str, value: &serde_json::Value) -> Result<String> {
    let (bom, text) = split_bom(src);
    let tokens = parse_pointer(ptr)?;
    if tokens.is_empty() {
        bail!("refusing to replace the whole document at JSON pointer \"\"");
    }
    // 先按读方向的宽松规则验一遍:畸形文件的报错文案与 `read_file` 保持一致,
    // 而且后面的扫描器可以假定文档是合法的。
    parse_json_lenient(text)
        .with_context(|| format!("failed to parse JSON before setting pointer {ptr}"))?;
    let root = Spanner::new(text).document()?;
    let style = Style::detect(text);

    let (node, consumed) = walk(&root, &tokens, ptr)?;
    let out = if consumed == tokens.len() {
        // 命中:只换值。父容器决定新值是展开还是压成一行。
        let (vs, ve) = node.span();
        let parent = walk_exact(&root, &tokens[..tokens.len() - 1], ptr)?;
        let multiline = is_multiline(text, parent.span());
        let base = line_indent(text, vs);
        let mut rendered = String::new();
        render_json(value, &style, &base, multiline, &mut rendered);
        apply_edits(text, &[(vs, ve, rendered)])
    } else {
        let rest = &tokens[consumed..];
        match node {
            JNode::Object { span, members } => {
                // 缺的中间层逐层套出来:`/a/b/c` 只缺 b、c 时补 `{"c": value}`。
                let mut nested = value.clone();
                for tok in rest[1..].iter().rev() {
                    nested =
                        serde_json::Value::Object(std::iter::once((tok.clone(), nested)).collect());
                }
                let items: Vec<Span> = members.iter().map(|m| m.span).collect();
                let key = &rest[0];
                insert_into(text, *span, &items, &style, |child| {
                    let mut s = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                    s.push_str(": ");
                    render_json(&nested, &style, child, true, &mut s);
                    s
                })
            }
            JNode::Array { span, elems } => {
                // 数组只允许在末尾追加(`-` 或恰好等于长度的下标)。往中间塞会
                // 悄悄挪动别人的下标,那不是「保守改写」。
                let appends = rest.len() == 1
                    && (rest[0] == "-" || rest[0].parse::<usize>() == Ok(elems.len()));
                if !appends {
                    bail!(
                        "JSON pointer {ptr}: {} is an array of {} elements; {:?} is out of range \
                         (only appending with '-' or index {} is allowed)",
                        render_ptr(&tokens[..consumed]),
                        elems.len(),
                        rest[0],
                        elems.len()
                    );
                }
                let items: Vec<Span> = elems.iter().map(|e| e.span()).collect();
                insert_into(text, *span, &items, &style, |child| {
                    let mut s = String::new();
                    render_json(value, &style, child, true, &mut s);
                    s
                })
            }
            // `walk` 撞到标量就已经报错了,这里到不了。改写用户配置的库不该
            // 用 panic 兜底不变量,所以还是报个错回去。
            JNode::Scalar { .. } => bail!(
                "JSON pointer {ptr}: {} is a scalar, cannot descend into {:?}",
                render_ptr(&tokens[..consumed]),
                rest[0]
            ),
        }
    };
    Ok(format!("{bom}{}", finish(text, out)))
}

/// 删掉 JSON Pointer 指向的成员或数组元素,返回新文本。
///
/// 路径本来就不存在时原样返回输入(**逐字节相同**,连拼接都不做)——删一个
/// 已经不在的东西是成功,不是错误。但「走穿标量」(`/a/b/c` 而 `a.b` 是个
/// 数字)不算不存在,那是路径本身讲不通,照样报错点名:静默成功会把调用方的
/// bug 一路藏到用户的配置文件里。
///
/// 删除是按「整条」算的:成员独占若干行时连同行首缩进、行尾逗号、行尾 `//`
/// 注释和换行一起抹掉;它是容器里最后一个成员时,前一个成员的逗号也跟着走,
/// 免得留下一个严格 JSON 不接受的尾逗号。
///
/// **压在成员上一行的独立注释会留下**,只有跟在值后面的同行注释跟着走。
/// 这与 [`remove_toml_path`] 不同——TOML 里「表头上方那段注释」是 `toml_edit`
/// 明确归属给这张表的装饰,删表就会带走它;JSON 没有这个归属概念,谁也说不准
/// 那条 `//` 是在讲这个键还是讲下一个键,所以宁可留下一条孤儿注释,也不去删
/// 用户手写、且删了就找不回来的东西。
pub fn remove_json_pointer(src: &str, ptr: &str) -> Result<String> {
    let (bom, text) = split_bom(src);
    let tokens = parse_pointer(ptr)?;
    if tokens.is_empty() {
        bail!("refusing to remove the whole document at JSON pointer \"\"");
    }
    parse_json_lenient(text)
        .with_context(|| format!("failed to parse JSON before removing pointer {ptr}"))?;
    let root = Spanner::new(text).document()?;
    // 先整条走一遍:`walk` 撞见标量会报错,只是没走完则说明目标不存在。
    let (_, consumed) = walk(&root, &tokens, ptr)?;
    if consumed != tokens.len() {
        return Ok(src.to_string());
    }
    let parent = walk_exact(&root, &tokens[..tokens.len() - 1], ptr)?;
    let last = &tokens[tokens.len() - 1];
    let (items, k) = match parent {
        JNode::Object { members, .. } => {
            let k = members
                .iter()
                .position(|m| m.key == *last)
                .with_context(|| format!("JSON pointer {ptr}: key {last:?} vanished mid-edit"))?;
            (members.iter().map(|m| m.span).collect::<Vec<_>>(), k)
        }
        JNode::Array { elems, .. } => {
            let k = last
                .parse::<usize>()
                .ok()
                .filter(|k| *k < elems.len())
                .with_context(|| format!("JSON pointer {ptr}: {last:?} is not a valid index"))?;
            (elems.iter().map(|e| e.span()).collect::<Vec<_>>(), k)
        }
        JNode::Scalar { .. } => bail!(
            "JSON pointer {ptr}: {} is a scalar, nothing to remove from it",
            render_ptr(&tokens[..tokens.len() - 1])
        ),
    };
    let edits: Vec<(usize, usize, String)> = cut_item(text, &items, k, parent.span())
        .into_iter()
        .map(|(s, e)| (s, e, String::new()))
        .collect();
    Ok(format!("{bom}{}", finish(text, apply_edits(text, &edits))))
}

/// 在 TOML 文档里写入(或创建)一个点路径指向的值,返回新文本。
///
/// 全程走 `toml_edit`:注释、空行、键序、引号风格、行内表 vs 表头的排布,
/// 凡是没被点名的字节一律原样落回。`~/.codex/config.toml` 里 MCP 表周围
/// 那些手写注释一旦丢了,用户既会发现也没法找回。
///
/// - 目标是已存在的标量或数组:就地换值,连 `= ` 两侧的空白和行尾 `#` 注释
///   都留在原处。
/// - 目标不存在:缺失的中间层补成隐式表,新表按 `[a.b]` 表头写出;新表里再嵌
///   套的表写成行内表,这样键序与原始 `toml::Value` 完全一致。
/// - 走穿标量报错并点名路径。
///
/// 点路径的限制沿用 [`toml_path`]:按 `.` 简单切分,**不支持**带引号的段,
/// 键名本身含 `.` 时无法表达。这种写法会被明确拒绝,而不是去改错的键。
pub fn set_toml_path(src: &str, dotted: &str, value: &toml::Value) -> Result<String> {
    let (bom, text) = split_bom(src);
    let segs = parse_dotted(dotted)?;
    if segs.is_empty() {
        bail!("refusing to replace the whole document at empty TOML path");
    }
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("failed to parse TOML before setting path {dotted}"))?;
    toml_apply_table(doc.as_table_mut(), &segs, 0, &TomlOp::Set(value), dotted)?;
    Ok(format!("{bom}{}", finish(text, doc.to_string())))
}

/// 删掉点路径指向的键、表、数组元素,返回新文本。
///
/// 路径不存在时原样返回输入(逐字节相同,不经 `toml_edit` 重新渲染)。
/// 「走穿标量」(`model.deeper` 而 `model` 是个字符串)不算不存在——
/// 那是路径本身讲不通,照样报错点名。
/// 删一张表会连同它头上那段属于它的注释一起走——那段注释描述的正是被删的东西;
/// 兄弟表、兄弟表的注释和它们的先后顺序都不动。
pub fn remove_toml_path(src: &str, dotted: &str) -> Result<String> {
    let (bom, text) = split_bom(src);
    let segs = parse_dotted(dotted)?;
    if segs.is_empty() {
        bail!("refusing to remove the whole document at empty TOML path");
    }
    // 探路走的是读方向 [`toml_path`] 那套规则,只是把「不存在」和「路径讲不通」
    // 分开:前者是成功,后者是调用方的 bug。
    let probe: toml::Value = toml::from_str(text)
        .with_context(|| format!("failed to parse TOML before removing path {dotted}"))?;
    match toml_probe(&probe, &segs) {
        TomlProbe::Hit => {}
        TomlProbe::Missing => return Ok(src.to_string()),
        TomlProbe::Scalar(depth) => bail!(
            "TOML path {dotted}: {} is a scalar, cannot descend into {:?}",
            render_dotted(&segs[..depth]),
            segs[depth]
        ),
    }
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("failed to parse TOML before removing path {dotted}"))?;
    toml_apply_table(doc.as_table_mut(), &segs, 0, &TomlOp::Remove, dotted)?;
    Ok(format!("{bom}{}", finish(text, doc.to_string())))
}

// --- 共用小工具 ------------------------------------------------------------

/// 把可能存在的 BOM 切出来。写方向必须把它原样贴回去:
/// 读方向剥 BOM 是为了让解析器工作,不是为了让它从文件里消失。
fn split_bom(src: &str) -> (&str, &str) {
    match src.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", src),
    }
}

/// 原文是否通篇 CRLF(有换行,且每个 `\n` 前面都压着 `\r`)。
fn is_crlf(s: &str) -> bool {
    let b = s.as_bytes();
    let mut seen = false;
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            if i == 0 || b[i - 1] != b'\r' {
                return false;
            }
            seen = true;
        }
    }
    seen
}

/// 给每个裸 `\n` 补上 `\r`。只在原文通篇 CRLF 时调用:
/// 这时原样保留的部分本来就是 CRLF(补了个寂寞),真正被改写的只有新插入的行。
/// JSON 串里的裸控制字符非法,TOML 里的裸 CR 只会作为 CRLF 的一半出现,
/// 所以全局扫一遍不会伤到字符串内容。
fn to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 32 + 8);
    let b = s.as_bytes();
    for (i, ch) in s.char_indices() {
        if ch == '\n' && (i == 0 || b[i - 1] != b'\r') {
            out.push('\r');
        }
        out.push(ch);
    }
    out
}

/// 收尾:统一换行风格,并让末尾换行的有无跟原文一致。
/// `toml_edit` 渲染新内容时只会吐 `\n`,末尾换行也可能被它补上或抹掉。
fn finish(src: &str, out: String) -> String {
    let crlf = is_crlf(src);
    let mut out = if crlf { to_crlf(&out) } else { out };
    match (src.ends_with('\n'), out.ends_with('\n')) {
        (true, false) if !out.is_empty() => out.push_str(if crlf { "\r\n" } else { "\n" }),
        (false, true) => {
            out.truncate(out.len() - 1);
            if out.ends_with('\r') {
                out.truncate(out.len() - 1);
            }
        }
        _ => {}
    }
    out
}

/// `pos` 所在行的行首下标。
fn line_start(text: &str, pos: usize) -> usize {
    text[..pos].rfind('\n').map_or(0, |i| i + 1)
}

/// `pos` 所在行的行首缩进(只取空格与 tab)。
fn line_indent(text: &str, pos: usize) -> String {
    let ls = line_start(text, pos);
    let seg = &text[ls..pos];
    let n = seg.len() - seg.trim_start_matches([' ', '\t']).len();
    seg[..n].to_string()
}

/// 区间里有没有换行——用来判断容器是展开写的还是挤在一行里。
fn is_multiline(text: &str, span: Span) -> bool {
    text[span.0..span.1].contains('\n')
}

/// 按升序、互不重叠的 `(start, end, 替换文本)` 列表拼出新文本。
fn apply_edits(text: &str, edits: &[(usize, usize, String)]) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut at = 0usize;
    for (s, e, rep) in edits {
        out.push_str(&text[at..*s]);
        out.push_str(rep);
        at = *e;
    }
    out.push_str(&text[at..]);
    out
}

// --- JSON:指针与路径 ------------------------------------------------------

/// 拆 RFC 6901 指针:`""` -> 空路径,否则必须以 `/` 开头,
/// 每段还原 `~1` -> `/`、`~0` -> `~`(顺序不能反,否则 `~01` 会被解成 `/`)。
fn parse_pointer(ptr: &str) -> Result<Vec<String>> {
    if ptr.is_empty() {
        return Ok(Vec::new());
    }
    let Some(rest) = ptr.strip_prefix('/') else {
        bail!("invalid JSON pointer {ptr:?}: must be empty or start with '/'");
    };
    Ok(rest
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect())
}

/// 把一段 token 前缀还原成人能读的指针文本,专供报错点名用。
fn render_ptr(tokens: &[String]) -> String {
    if tokens.is_empty() {
        return "the document root".to_string();
    }
    let mut s = String::new();
    for t in tokens {
        s.push('/');
        s.push_str(&t.replace('~', "~0").replace('/', "~1"));
    }
    s
}

/// 沿 tokens 下钻,返回 (停下来的节点, 已消耗的 token 数)。
/// 消耗数小于 tokens 长度 = 从这里开始缺路径;走穿标量则直接报错点名。
fn walk<'n>(root: &'n JNode, tokens: &[String], ptr: &str) -> Result<(&'n JNode, usize)> {
    let mut cur = root;
    for (i, tok) in tokens.iter().enumerate() {
        match cur {
            JNode::Object { members, .. } => match members.iter().find(|m| m.key == *tok) {
                Some(m) => cur = &m.value,
                None => return Ok((cur, i)),
            },
            JNode::Array { elems, .. } => {
                match tok.parse::<usize>().ok().and_then(|k| elems.get(k)) {
                    Some(e) => cur = e,
                    None => return Ok((cur, i)),
                }
            }
            JNode::Scalar { .. } => bail!(
                "JSON pointer {ptr}: {} is a scalar, cannot descend into {tok:?}",
                render_ptr(&tokens[..i])
            ),
        }
    }
    Ok((cur, tokens.len()))
}

/// 必须整条走通的下钻。调用方已知路径存在时用它,走不通说明扫描树与
/// serde_json 的解析结果打架了。
fn walk_exact<'n>(root: &'n JNode, tokens: &[String], ptr: &str) -> Result<&'n JNode> {
    let (node, consumed) = walk(root, tokens, ptr)?;
    if consumed != tokens.len() {
        bail!(
            "JSON pointer {ptr}: {} does not exist",
            render_ptr(&tokens[..=consumed.min(tokens.len() - 1)])
        );
    }
    Ok(node)
}

// --- JSON:原文区间扫描 ----------------------------------------------------

/// JSON 值在原文里的位置。只记区间,不记内容——内容原文里有。
#[derive(Debug)]
enum JNode {
    Object { span: Span, members: Vec<JMember> },
    Array { span: Span, elems: Vec<JNode> },
    Scalar { span: Span },
}

impl JNode {
    fn span(&self) -> Span {
        match self {
            JNode::Object { span, .. } | JNode::Array { span, .. } | JNode::Scalar { span } => {
                *span
            }
        }
    }
}

/// 对象成员。`span` 从键的起始引号一直到值的最后一个字节,
/// 中间的冒号和空白都算它的——删除时要整条端走。
#[derive(Debug)]
struct JMember {
    key: String,
    span: Span,
    value: JNode,
}

/// JSONC 感知的区间扫描器。它不校验文档合法性(调用方已经用 serde_json 验过),
/// 只负责把每个值在原文里的字节区间量出来。
struct Spanner<'a> {
    src: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> Spanner<'a> {
    fn new(src: &'a str) -> Self {
        Spanner {
            src,
            b: src.as_bytes(),
            i: 0,
        }
    }

    /// 扫完整篇文档的根值。
    fn document(mut self) -> Result<JNode> {
        self.value()
    }

    /// 跳过空白与 JSONC 注释。
    fn trivia(&mut self) {
        loop {
            match self.b.get(self.i) {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.i += 1,
                Some(b'/') if self.b.get(self.i + 1) == Some(&b'/') => {
                    while self.i < self.b.len() && self.b[self.i] != b'\n' {
                        self.i += 1;
                    }
                }
                Some(b'/') if self.b.get(self.i + 1) == Some(&b'*') => {
                    self.i += 2;
                    while self.i < self.b.len() {
                        if self.b[self.i] == b'*' && self.b.get(self.i + 1) == Some(&b'/') {
                            self.i += 2;
                            break;
                        }
                        self.i += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn value(&mut self) -> Result<JNode> {
        self.trivia();
        match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => {
                let s = self.i;
                self.i = skip_string(self.b, s);
                Ok(JNode::Scalar { span: (s, self.i) })
            }
            Some(_) => {
                // 数字 / true / false / null:吃到分隔符为止。`/` 也算分隔符,
                // 这样 `1// c` 不会把注释吞进字面量。
                let s = self.i;
                while self.i < self.b.len()
                    && !matches!(
                        self.b[self.i],
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n' | b'/'
                    )
                {
                    self.i += 1;
                }
                if self.i == s {
                    bail!("malformed JSON: unexpected byte at offset {s}");
                }
                Ok(JNode::Scalar { span: (s, self.i) })
            }
            None => bail!("malformed JSON: unexpected end of input"),
        }
    }

    fn object(&mut self) -> Result<JNode> {
        let start = self.i;
        self.i += 1; // '{'
        let mut members = Vec::new();
        loop {
            self.trivia();
            match self.b.get(self.i) {
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                Some(b'"') => {
                    let ks = self.i;
                    self.i = skip_string(self.b, ks);
                    let ke = self.i;
                    // 键名可能带转义,交给 serde_json 还原,免得自己写一遍解码。
                    let key: String = serde_json::from_str(&self.src[ks..ke])
                        .with_context(|| format!("malformed JSON object key at offset {ks}"))?;
                    self.trivia();
                    if self.b.get(self.i) != Some(&b':') {
                        bail!("malformed JSON: expected ':' after key at offset {ks}");
                    }
                    self.i += 1;
                    let value = self.value()?;
                    let end = value.span().1;
                    members.push(JMember {
                        key,
                        span: (ks, end),
                        value,
                    });
                    self.trivia();
                    if self.b.get(self.i) == Some(&b',') {
                        self.i += 1;
                    }
                }
                _ => bail!("malformed JSON object at offset {}", self.i),
            }
        }
        Ok(JNode::Object {
            span: (start, self.i),
            members,
        })
    }

    fn array(&mut self) -> Result<JNode> {
        let start = self.i;
        self.i += 1; // '['
        let mut elems = Vec::new();
        loop {
            self.trivia();
            match self.b.get(self.i) {
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                Some(_) => {
                    elems.push(self.value()?);
                    self.trivia();
                    if self.b.get(self.i) == Some(&b',') {
                        self.i += 1;
                    }
                }
                None => bail!("malformed JSON: unterminated array at offset {start}"),
            }
        }
        Ok(JNode::Array {
            span: (start, self.i),
            elems,
        })
    }
}

// --- JSON:渲染、插入、删除 ------------------------------------------------

/// 把一个 `serde_json::Value` 渲染成文本。
///
/// `pretty` 为假时压成单行(原容器本来就是一行,展开它属于重排版)。
/// 全标量数组无论如何都保持单行:`"args": ["-y", "pkg"]` 才是人写出来的样子。
fn render_json(v: &serde_json::Value, style: &Style, base: &str, pretty: bool, out: &mut String) {
    match v {
        serde_json::Value::Object(map) if pretty && !map.is_empty() => {
            let inner = format!("{base}{}", style.unit);
            out.push('{');
            for (n, (k, val)) in map.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push_str(&style.le);
                out.push_str(&inner);
                push_json_str(k, out);
                out.push_str(": ");
                render_json(val, style, &inner, true, out);
            }
            out.push_str(&style.le);
            out.push_str(base);
            out.push('}');
        }
        serde_json::Value::Array(items)
            if pretty
                && !items.is_empty()
                && items.iter().any(|e| e.is_object() || e.is_array()) =>
        {
            let inner = format!("{base}{}", style.unit);
            out.push('[');
            for (n, val) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                }
                out.push_str(&style.le);
                out.push_str(&inner);
                render_json(val, style, &inner, true, out);
            }
            out.push_str(&style.le);
            out.push_str(base);
            out.push(']');
        }
        other => render_json_compact(other, out),
    }
}

/// 单行渲染。逗号后带一个空格——`["-y", "pkg"]` 才是人手写配置的样子,
/// `serde_json::to_string` 那种一个空格都不留的形态只出现在机器产物里。
fn render_json_compact(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::Object(map) => {
            out.push('{');
            for (n, (k, val)) in map.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                push_json_str(k, out);
                out.push_str(": ");
                render_json_compact(val, out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (n, val) in items.iter().enumerate() {
                if n > 0 {
                    out.push_str(", ");
                }
                render_json_compact(val, out);
            }
            out.push(']');
        }
        // 标量:`serde_json` 的转义规则就是权威,自己再写一遍只会写错。
        // `Value` 的序列化不可能失败(数字不会是 NaN),真出事也宁可留一个
        // 显眼的 null,而不是把 panic 甩到调用方脸上。
        scalar => out.push_str(&serde_json::to_string(scalar).unwrap_or_else(|_| "null".into())),
    }
}

/// 把一个键名写成带引号、按 JSON 规则转义的字面量。
fn push_json_str(s: &str, out: &mut String) {
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()));
}

/// 往容器末尾插入一个条目,返回新文本。
///
/// `render` 拿到子层缩进后吐出条目文本(对象成员含 `"key": ` 前缀,数组元素不含)。
/// 缩进从最后一个已有条目所在行学来;容器是空的就在闭合符号那行的行首插一行。
/// 原文若用尾逗号风格(`… , }`),插入后依旧留尾逗号。
fn insert_into(
    text: &str,
    span: Span,
    items: &[Span],
    style: &Style,
    render: impl FnOnce(&str) -> String,
) -> String {
    let multiline = is_multiline(text, span);
    let child = if !multiline {
        String::new()
    } else if let Some(last) = items.last() {
        line_indent(text, last.0)
    } else {
        format!("{}{}", line_indent(text, span.0), style.unit)
    };
    let item = render(&child);
    let b = text.as_bytes();

    let Some(last) = items.last() else {
        // 空容器。
        let close = span.1 - 1; // '}' / ']'
        if !multiline {
            return format!("{}{}{}", &text[..close], item, &text[close..]);
        }
        // 插在闭合符号那行的行首,而不是紧贴开括号——`{ // 注释` 这种写法下,
        // 贴着开括号插会把新条目怼到注释前面去。
        let ls = line_start(text, close);
        return format!(
            "{}{}{}{}{}",
            &text[..ls],
            child,
            item,
            style.le,
            &text[ls..]
        );
    };

    // 找出「已有的尾逗号」和「同行的行尾注释」,插入点要落在它们之后。
    let mut p = last.1;
    let mut q = p;
    while matches!(b.get(q), Some(b' ' | b'\t')) {
        q += 1;
    }
    let had_comma = b.get(q) == Some(&b',');
    if had_comma {
        p = q + 1;
    }
    let mut q = p;
    while matches!(b.get(q), Some(b' ' | b'\t')) {
        q += 1;
    }
    if b.get(q) == Some(&b'/') && b.get(q + 1) == Some(&b'/') {
        while q < b.len() && b[q] != b'\n' && b[q] != b'\r' {
            q += 1;
        }
        p = q;
    }

    let sep = if multiline {
        format!("{}{child}", style.le)
    } else {
        " ".to_string()
    };
    let mut out = String::with_capacity(text.len() + item.len() + sep.len() + 2);
    out.push_str(&text[..last.1]);
    // 逗号必须补在原值紧后面,不能跑到行尾注释后头去。
    if !had_comma {
        out.push(',');
    }
    out.push_str(&text[last.1..p]);
    out.push_str(&sep);
    out.push_str(&item);
    if had_comma {
        out.push(',');
    }
    out.push_str(&text[p..]);
    out
}

/// 算出「删掉容器里第 `k` 个条目」要抹掉的区间(升序、互不重叠)。
///
/// 条目独占整行时连行首缩进和行尾换行一起删,不留空行;它是最后一个条目时
/// 还要顺手抹掉前一个条目后面的逗号,否则会剩下严格 JSON 不认的尾逗号。
fn cut_item(text: &str, items: &[Span], k: usize, container: Span) -> Vec<Span> {
    let b = text.as_bytes();
    let item = items[k];

    // 前导:整行只有它就从行首删起。
    let ls = line_start(text, item.0);
    let line_based = text[ls..item.0].bytes().all(|c| c == b' ' || c == b'\t');
    let from = if line_based { ls } else { item.0 };

    // 尾随:值 -> 逗号 -> 行尾注释 -> 换行。
    let mut p = item.1;
    let mut q = p;
    while matches!(b.get(q), Some(b' ' | b'\t')) {
        q += 1;
    }
    if b.get(q) == Some(&b',') {
        p = q + 1;
    }
    let mut q = p;
    while matches!(b.get(q), Some(b' ' | b'\t')) {
        q += 1;
    }
    if b.get(q) == Some(&b'/') && b.get(q + 1) == Some(&b'/') {
        while q < b.len() && b[q] != b'\n' && b[q] != b'\r' {
            q += 1;
        }
        p = q;
    }
    if line_based {
        if text[p..].starts_with("\r\n") {
            p += 2;
        } else if b.get(p) == Some(&b'\n') {
            p += 1;
        }
    } else {
        // 单行容器:把逗号后面那点空格也带走,`{"a": 1, "b": 2}` 才不会剩双空格。
        let mut q = p;
        while matches!(b.get(q), Some(b' ' | b'\t')) {
            q += 1;
        }
        if q < container.1 {
            p = q;
        }
    }

    let mut cuts = Vec::with_capacity(2);
    // 末位条目:前一个条目的逗号也得走。
    if k + 1 == items.len() && k > 0 {
        let prev_end = items[k - 1].1;
        let mut c = prev_end;
        while matches!(b.get(c), Some(b' ' | b'\t')) {
            c += 1;
        }
        if b.get(c) == Some(&b',') {
            let mut e = c + 1;
            while e < from && matches!(b.get(e), Some(b' ' | b'\t')) {
                e += 1;
            }
            cuts.push((c, e));
        }
    }
    cuts.push((from, p));
    cuts
}

// --- TOML:点路径改写 ------------------------------------------------------

/// 点路径上的终端动作。`set` 与 `remove` 只有最后一步不同,下钻逻辑共用一套。
enum TomlOp<'a> {
    Set(&'a toml::Value),
    Remove,
}

/// 点路径探路的三种结局。删除方向必须区分「不存在」(成功,原样返回)和
/// 「路径讲不通」(报错),把两者都当成 miss 会把调用方的 bug 咽下去。
enum TomlProbe {
    Hit,
    Missing,
    /// 在第 N 段上撞见了标量。
    Scalar(usize),
}

/// 沿点路径探一遍。下钻规则与读方向的 [`toml_path`] 完全一致:
/// 表按键查,数组按十进制下标查,其余一律是标量。
fn toml_probe(root: &toml::Value, segs: &[String]) -> TomlProbe {
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        let next = match cur {
            toml::Value::Table(t) => t.get(seg),
            // 数组上给了个非数字段,或者下标越界:这个元素确实不在,算 miss。
            toml::Value::Array(a) => seg.parse::<usize>().ok().and_then(|k| a.get(k)),
            _ => return TomlProbe::Scalar(i),
        };
        match next {
            Some(n) => cur = n,
            None => return TomlProbe::Missing,
        }
    }
    TomlProbe::Hit
}

/// 拆点路径。空段(`a..b`、`a.`)和带引号的段一律拒绝:
/// 后者是「键名含 `.`」的常见写法,这里表达不了,宁可报错也不去改错的键。
fn parse_dotted(dotted: &str) -> Result<Vec<String>> {
    if dotted.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for seg in dotted.split('.') {
        if seg.is_empty() {
            bail!("invalid TOML path {dotted:?}: empty segment");
        }
        if seg.contains('"') || seg.contains('\'') {
            bail!(
                "invalid TOML path {dotted:?}: dotted paths cannot express quoted keys or keys \
                 containing '.', and segment {seg:?} looks quoted"
            );
        }
        out.push(seg.to_string());
    }
    Ok(out)
}

/// 新造的中间层:隐式表,只有真在里面放了直属键值才会印出表头。
fn implicit_table() -> toml_edit::Item {
    let mut t = toml_edit::Table::new();
    t.set_implicit(true);
    toml_edit::Item::Table(t)
}

/// `toml::Value` -> `toml_edit::Value`。表一律变行内表:
/// 这样键序与原始值完全一致,不会被表头/子表的渲染顺序打乱。
fn to_edit_value(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::String(s) => toml_edit::Value::from(s.as_str()),
        toml::Value::Integer(i) => toml_edit::Value::from(*i),
        toml::Value::Float(f) => toml_edit::Value::from(*f),
        toml::Value::Boolean(b) => toml_edit::Value::from(*b),
        toml::Value::Datetime(d) => toml_edit::Value::from(*d),
        toml::Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for e in a {
                arr.push(to_edit_value(e));
            }
            toml_edit::Value::Array(arr)
        }
        toml::Value::Table(t) => {
            let mut it = toml_edit::InlineTable::new();
            for (k, val) in t {
                it.insert(k, to_edit_value(val));
            }
            toml_edit::Value::InlineTable(it)
        }
    }
}

/// `toml::Value` -> `toml_edit::Item`。顶层是表就写成 `[header]` 段
/// (`[mcp_servers.foo]` 才是 codex 配置里的既有写法),其余走值。
fn to_edit_item(v: &toml::Value) -> toml_edit::Item {
    match v {
        toml::Value::Table(t) => {
            let mut tbl = toml_edit::Table::new();
            for (k, val) in t {
                tbl.insert(k, toml_edit::Item::Value(to_edit_value(val)));
            }
            toml_edit::Item::Table(tbl)
        }
        other => toml_edit::Item::Value(to_edit_value(other)),
    }
}

fn toml_apply_item(
    item: &mut toml_edit::Item,
    segs: &[String],
    depth: usize,
    op: &TomlOp<'_>,
    dotted: &str,
) -> Result<()> {
    match item {
        toml_edit::Item::Table(t) => toml_apply_table(t, segs, depth, op, dotted),
        toml_edit::Item::Value(v) => toml_apply_value(v, segs, depth, op, dotted),
        toml_edit::Item::ArrayOfTables(aot) => {
            let idx = toml_index(segs, depth, dotted, aot.len())?;
            if depth + 1 == segs.len() {
                match op {
                    TomlOp::Remove => {
                        aot.remove(idx);
                        Ok(())
                    }
                    TomlOp::Set(_) => bail!(
                        "TOML path {dotted}: refusing to replace array-of-tables element \
                         {} wholesale; edit its keys instead",
                        render_dotted(&segs[..=depth])
                    ),
                }
            } else {
                let t = aot.get_mut(idx).with_context(|| {
                    format!("TOML path {dotted}: index {idx} vanished mid-edit")
                })?;
                toml_apply_table(t, segs, depth + 1, op, dotted)
            }
        }
        toml_edit::Item::None => bail!(
            "TOML path {dotted}: {} is empty, cannot descend",
            render_dotted(&segs[..depth])
        ),
    }
}

fn toml_apply_table(
    t: &mut toml_edit::Table,
    segs: &[String],
    depth: usize,
    op: &TomlOp<'_>,
    dotted: &str,
) -> Result<()> {
    let key = &segs[depth];
    if depth + 1 == segs.len() {
        match op {
            TomlOp::Remove => {
                t.remove(key);
            }
            TomlOp::Set(v) => {
                // 原地换标量/数组:键、`=` 两侧空白、行尾 `#` 注释统统留在原处。
                let in_place =
                    !v.is_table() && matches!(t.get(key), Some(toml_edit::Item::Value(_)));
                if in_place {
                    if let Some(toml_edit::Item::Value(old)) = t.get_mut(key) {
                        let decor = old.decor().clone();
                        let mut nv = to_edit_value(v);
                        *nv.decor_mut() = decor;
                        *old = nv;
                    }
                } else {
                    t.insert(key, to_edit_item(v));
                }
                // 隐式表刚多了直属键值就必须显形,不然渲染时表头会被吞掉,
                // 这个键会挂到上一张表名下——静默改错位置比报错糟得多。
                if depth > 0 && !v.is_table() {
                    t.set_implicit(false);
                }
            }
        }
        return Ok(());
    }
    let next = match op {
        TomlOp::Remove => match t.get_mut(key) {
            Some(next) => next,
            None => return Ok(()),
        },
        TomlOp::Set(_) => t.entry(key).or_insert(implicit_table()),
    };
    toml_apply_item(next, segs, depth + 1, op, dotted)
}

fn toml_apply_value(
    v: &mut toml_edit::Value,
    segs: &[String],
    depth: usize,
    op: &TomlOp<'_>,
    dotted: &str,
) -> Result<()> {
    match v {
        toml_edit::Value::InlineTable(t) => {
            let key = &segs[depth];
            if depth + 1 == segs.len() {
                match op {
                    TomlOp::Remove => {
                        t.remove(key);
                    }
                    TomlOp::Set(new) => {
                        if let Some(old) = t.get_mut(key) {
                            let decor = old.decor().clone();
                            let mut nv = to_edit_value(new);
                            *nv.decor_mut() = decor;
                            *old = nv;
                        } else {
                            t.insert(key, to_edit_value(new));
                        }
                    }
                }
                return Ok(());
            }
            match op {
                TomlOp::Remove => {}
                TomlOp::Set(_) if t.get(key).is_none() => {
                    // 行内表里补出来的中间层只能继续是行内表。
                    t.insert(
                        key,
                        toml_edit::Value::InlineTable(toml_edit::InlineTable::new()),
                    );
                }
                TomlOp::Set(_) => {}
            }
            match t.get_mut(key) {
                Some(next) => toml_apply_value(next, segs, depth + 1, op, dotted),
                None => Ok(()),
            }
        }
        toml_edit::Value::Array(a) => {
            let idx = toml_index(segs, depth, dotted, a.len())?;
            if depth + 1 == segs.len() {
                match op {
                    TomlOp::Remove => {
                        a.remove(idx);
                    }
                    TomlOp::Set(new) => {
                        if let Some(slot) = a.get_mut(idx) {
                            let decor = slot.decor().clone();
                            let mut nv = to_edit_value(new);
                            *nv.decor_mut() = decor;
                            *slot = nv;
                        }
                    }
                }
                return Ok(());
            }
            match a.get_mut(idx) {
                Some(next) => toml_apply_value(next, segs, depth + 1, op, dotted),
                None => Ok(()),
            }
        }
        _ => bail!(
            "TOML path {dotted}: {} is a scalar, cannot descend into {:?}",
            render_dotted(&segs[..depth]),
            segs[depth]
        ),
    }
}

/// 数组段必须是十进制下标且落在范围内。越界不猜、不追加,直接点名报错。
fn toml_index(segs: &[String], depth: usize, dotted: &str, len: usize) -> Result<usize> {
    let seg = &segs[depth];
    let idx = seg.parse::<usize>().map_err(|_| {
        anyhow!(
            "TOML path {dotted}: {} is an array, so segment {seg:?} must be a decimal index",
            render_dotted(&segs[..depth])
        )
    })?;
    if idx >= len {
        bail!(
            "TOML path {dotted}: index {idx} is out of range, {} has {len} elements",
            render_dotted(&segs[..depth])
        );
    }
    Ok(idx)
}

/// 点路径前缀的人话形式,专供报错点名。
fn render_dotted(segs: &[String]) -> String {
    if segs.is_empty() {
        "the document root".to_string()
    } else {
        segs.join(".")
    }
}

/// 带大小上限的文本读取。超限直接报错并说明实际大小,防止把
/// 误认成配置的巨型文件整个吸进内存。
pub fn read_to_string_capped(path: &Path, cap: u64) -> anyhow::Result<String> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to read metadata: {}", path.display()))?;
    if meta.len() > cap {
        bail!(
            "file too large, refusing to read: {} is {} bytes, cap is {} bytes",
            path.display(),
            meta.len(),
            cap
        );
    }
    fs::read_to_string(path).with_context(|| format!("failed to read file: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const JSON_FIXTURE: &str = r#"{"mcpServers":{"foo":{"command":"npx","args":["-y","foo"]}}}"#;
    const TOML_FIXTURE: &str = "[mcp.servers.foo]\ncommand = \"npx\"\nargs = [\"-y\", \"foo\"]\n";

    fn write_tmp(dir: &tempfile::TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    #[test]
    fn read_file_dispatches_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let j = write_tmp(&dir, "cfg.json", JSON_FIXTURE);
        let t = write_tmp(&dir, "cfg.toml", TOML_FIXTURE);
        assert!(matches!(read_file(&j).unwrap(), Doc::Json(_)));
        assert!(matches!(read_file(&t).unwrap(), Doc::Toml(_)));
    }

    #[test]
    fn read_file_sniffs_without_extension() {
        let dir = tempfile::tempdir().unwrap();
        // 无扩展名的 JSON 内容 -> 嗅探成 JSON。
        let j = write_tmp(&dir, "config", JSON_FIXTURE);
        assert!(matches!(read_file(&j).unwrap(), Doc::Json(_)));
        // 无扩展名的 TOML 内容(非法 JSON)-> 落到 TOML。
        let t = write_tmp(&dir, "settings", TOML_FIXTURE);
        assert!(matches!(read_file(&t).unwrap(), Doc::Toml(_)));
        // 两者都不是 -> 报错里同时提到两种格式的失败。
        let bad = write_tmp(&dir, "garbage", "{{{ not = valid [");
        let msg = read_file(&bad).unwrap_err().to_string();
        assert!(msg.contains("JSON"), "报错应包含 JSON 失败信息: {msg}");
        assert!(msg.contains("TOML"), "报错应包含 TOML 失败信息: {msg}");
    }

    #[test]
    fn json_pointer_hit_and_miss() {
        let v: serde_json::Value = serde_json::from_str(JSON_FIXTURE).unwrap();
        assert_eq!(
            json_pointer(&v, "/mcpServers/foo/command").and_then(|x| x.as_str()),
            Some("npx")
        );
        assert!(json_pointer(&v, "/mcpServers/bar").is_none());
        // 空指针返回整棵树。
        assert!(json_pointer(&v, "").is_some());
    }

    #[test]
    fn toml_path_hit_and_miss() {
        let v: toml::Value = toml::from_str(TOML_FIXTURE).unwrap();
        assert_eq!(
            toml_path(&v, "mcp.servers.foo.command").and_then(|x| x.as_str()),
            Some("npx")
        );
        // 数组下标下钻。
        assert_eq!(
            toml_path(&v, "mcp.servers.foo.args.1").and_then(|x| x.as_str()),
            Some("foo")
        );
        assert!(toml_path(&v, "mcp.servers.bar").is_none());
        assert!(toml_path(&v, "mcp.servers.foo.args.9").is_none());
        // 空路径返回整棵树。
        assert!(toml_path(&v, "").is_some());
    }

    #[test]
    fn capped_read_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "big.json", "0123456789");
        let msg = read_to_string_capped(&p, 4).unwrap_err().to_string();
        assert!(msg.contains("10"), "报错应说明实际大小: {msg}");
        // 上限内正常读取。
        assert_eq!(read_to_string_capped(&p, 10).unwrap(), "0123456789");
    }

    #[test]
    fn jsonc_comments_parse_like_the_comment_free_equivalent() {
        let jsonc = "// banner:copilot/cursor 的真实文件就是这样开头的\n\
                     {\n  \"a\": 1, // 行尾注释\n  /* 块注释\n     跨了两行 */\n  \"b\": [\"x\", \"y\"]\n}\n";
        let plain = r#"{"a":1,"b":["x","y"]}"#;
        let got: serde_json::Value = serde_json::from_str(&strip_json_comments(jsonc)).unwrap();
        let want: serde_json::Value = serde_json::from_str(plain).unwrap();
        assert_eq!(got, want, "剥注释后应与无注释版本等价");
    }

    #[test]
    fn slashes_inside_strings_survive() {
        let src = r#"{"url": "https://x.test/a", "glob": "/*.rs"}"#;
        let v: serde_json::Value = serde_json::from_str(&strip_json_comments(src)).unwrap();
        assert_eq!(v["url"], serde_json::json!("https://x.test/a"));
        assert_eq!(v["glob"], serde_json::json!("/*.rs"));
    }

    #[test]
    fn escaped_quote_does_not_desync_scanner() {
        // 字符串里先有转义引号,再有 `//`;字符串外还有一条真注释。
        let src = r#"{"msg": "he said \"hi\" // not a comment"} // 真注释"#;
        let v: serde_json::Value = serde_json::from_str(&strip_json_comments(src)).unwrap();
        assert_eq!(
            v["msg"].as_str(),
            Some(r#"he said "hi" // not a comment"#),
            "转义引号不应让扫描器提前认为字符串结束"
        );
    }

    #[test]
    fn trailing_commas_are_tolerated() {
        // 对象与数组各一处尾逗号,并夹一条注释确认跨注释判定也成立。
        let src = "{\n  \"a\": [1, 2,],\n  \"b\": 3,\n  // 注释挡在尾逗号和 } 之间\n}\n";
        let v: serde_json::Value = serde_json::from_str(&strip_json_comments(src)).unwrap();
        assert_eq!(v, serde_json::json!({"a": [1, 2], "b": 3}));
    }

    #[test]
    fn comment_free_input_is_borrowed() {
        // 零分配保证:绝大多数配置文件走的就是这条路径。
        let src = r#"{"a": 1, "b": ["x"]}"#;
        assert!(matches!(strip_json_comments(src), Cow::Borrowed(_)));
        // 有东西要删时才允许 Owned。
        assert!(matches!(strip_json_comments("{} // x"), Cow::Owned(_)));
    }

    #[test]
    fn block_comment_preserves_line_numbers() {
        // 错误在第 7 行,上面压着一段 3 行的块注释。
        let src = "{\n\
                   \x20 \"a\": 1,\n\
                   \x20 /* 块注释第一行\n\
                   \x20    第二行\n\
                   \x20    第三行 */\n\
                   \x20 \"b\": 2,\n\
                   \x20 \"c\": @\n\
                   }\n";
        let err = serde_json::from_str::<serde_json::Value>(&strip_json_comments(src)).unwrap_err();
        assert_eq!(err.line(), 7, "行号必须仍指向原文第 7 行,实际报错: {err}");
    }

    #[test]
    fn read_file_reads_jsonc_extension_and_commented_json() {
        let dir = tempfile::tempdir().unwrap();
        // `.jsonc` 走专门分支。
        let a = write_tmp(&dir, "opencode.jsonc", "// hi\n{\"a\": 1,}\n");
        assert!(matches!(read_file(&a).unwrap(), Doc::Json(_)));
        // `.json` 但内容是 JSONC——copilot / cursor / qoder 的真实形态。
        let b = write_tmp(&dir, "argv.json", "// banner\n{\"b\": 2}\n");
        match read_file(&b).unwrap() {
            Doc::Json(v) => assert_eq!(v["b"], serde_json::json!(2)),
            other => panic!("应解析成 JSON,实际: {other:?}"),
        }
        // 无扩展名的嗅探路径同样吃 JSONC。
        let c = write_tmp(&dir, "config", "/* head */\n{\"c\": 3}\n");
        assert!(matches!(read_file(&c).unwrap(), Doc::Json(_)));
    }

    #[test]
    fn read_file_strips_leading_bom() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "bom.json", "\u{feff}{\"a\": 1}");
        match read_file(&p).unwrap() {
            Doc::Json(v) => assert_eq!(v["a"], serde_json::json!(1)),
            other => panic!("BOM 不应挡住解析,实际: {other:?}"),
        }
    }

    #[test]
    fn genuinely_malformed_json_keeps_existing_error_wording() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_tmp(&dir, "broken.json", "{\"a\": }");
        let msg = read_file(&p).unwrap_err().to_string();
        assert!(
            msg.contains("failed to parse JSON:"),
            "畸形文件的报错文案不应变化: {msg}"
        );
    }

    // --- 写方向 ------------------------------------------------------------

    /// 一份 TOML,形状照抄本机 `~/.codex/config.toml`:头部手写注释块、行尾
    /// 注释、显式的空 `[mcp_servers]` 表头、属于某张表的独立注释、表间空行,
    /// 以及键名里带 `.` 和 `/` 的带引号表头。这些注释一旦丢了,用户既会发现
    /// 也没法找回。
    const TOML_CONFIG: &str = "\
# codex 配置头,这段手写注释必须活下来
# 第二行

model = \"gpt-5\"  # 行尾注释也要留住

[mcp_servers]

[mcp_servers.stitch]
command = \"npx\"
args = [\"-y\", \"stitch\"]

# 这条注释属于 context7,删它的时候要跟着走
[mcp_servers.context7]
command = \"uvx\"

[projects.\"/Users/laibu/Downloads\"]
trust_level = \"trusted\"
";

    /// 一份带首行 banner、键间注释的 JSONC,形状照着 `~/.cursor/argv.json` 来。
    const JSONC_CONFIG: &str = "\
// banner:cursor / copilot 的配置就是这样开头的
{
  \"a\": 1,
  // 这条注释夹在两个键之间
  \"b\": {
    \"c\": \"old\"
  }
}
";

    fn tbl(pairs: &[(&str, &str)]) -> toml::Value {
        let mut t = toml::value::Table::new();
        for (k, v) in pairs {
            t.insert((*k).to_string(), toml::Value::String((*v).to_string()));
        }
        toml::Value::Table(t)
    }

    #[test]
    fn toml_set_adds_one_table_and_touches_nothing_else() {
        let got = set_toml_path(
            TOML_CONFIG,
            "mcp_servers.duster",
            &tbl(&[("command", "duster")]),
        )
        .unwrap();
        // 整串断言:新表挨着它的兄弟表落下(toml_edit 按逻辑分组插入,不是甩到
        // 文件末尾),除此之外每一个字节——包括带引号的 `[projects."…"]` 表头
        // ——都必须原样。
        let want = "\
# codex 配置头,这段手写注释必须活下来
# 第二行

model = \"gpt-5\"  # 行尾注释也要留住

[mcp_servers]

[mcp_servers.stitch]
command = \"npx\"
args = [\"-y\", \"stitch\"]

# 这条注释属于 context7,删它的时候要跟着走
[mcp_servers.context7]
command = \"uvx\"

[mcp_servers.duster]
command = \"duster\"

[projects.\"/Users/laibu/Downloads\"]
trust_level = \"trusted\"
";
        assert_eq!(got, want, "新增一张表不得动到别处任何字节");
    }

    #[test]
    fn toml_set_replaces_scalar_in_place_keeping_its_trailing_comment() {
        let got = set_toml_path(
            TOML_CONFIG,
            "model",
            &toml::Value::String("gpt-6".to_string()),
        )
        .unwrap();
        let want = TOML_CONFIG.replace("\"gpt-5\"", "\"gpt-6\"");
        assert_eq!(got, want, "就地换值时行尾 # 注释与两侧空白都要留在原处");
    }

    #[test]
    fn toml_remove_drops_one_table_and_leaves_siblings_intact() {
        let got = remove_toml_path(TOML_CONFIG, "mcp_servers.stitch").unwrap();
        let want = "\
# codex 配置头,这段手写注释必须活下来
# 第二行

model = \"gpt-5\"  # 行尾注释也要留住

[mcp_servers]

# 这条注释属于 context7,删它的时候要跟着走
[mcp_servers.context7]
command = \"uvx\"

[projects.\"/Users/laibu/Downloads\"]
trust_level = \"trusted\"
";
        assert_eq!(got, want, "兄弟表、它们的注释和先后顺序都不许动");
    }

    #[test]
    fn toml_remove_takes_the_removed_table_s_own_comment_with_it() {
        // 表头上那段注释描述的正是这张表,删表就该一起走;
        // 别人的注释一个字节都不许少。
        let got = remove_toml_path(TOML_CONFIG, "mcp_servers.context7").unwrap();
        let want = "\
# codex 配置头,这段手写注释必须活下来
# 第二行

model = \"gpt-5\"  # 行尾注释也要留住

[mcp_servers]

[mcp_servers.stitch]
command = \"npx\"
args = [\"-y\", \"stitch\"]

[projects.\"/Users/laibu/Downloads\"]
trust_level = \"trusted\"
";
        assert_eq!(got, want);
    }

    #[test]
    fn toml_remove_path_through_scalar_errors_instead_of_reporting_success() {
        // 「走穿标量」不是「已经不在了」,是路径讲不通 —— 静默成功会把调用方的
        // bug 一路藏进用户的配置文件。
        let err = remove_toml_path(TOML_CONFIG, "model.deeper")
            .unwrap_err()
            .to_string();
        assert!(err.contains("scalar"), "应说明走穿了标量: {err}");
        assert!(err.contains("model"), "报错必须点名出事的路径: {err}");
    }

    #[test]
    fn toml_remove_missing_path_returns_input_byte_for_byte() {
        // 删一个本来就不在的东西是成功,而且必须原样返回。
        let got = remove_toml_path(TOML_CONFIG, "mcp_servers.nope").unwrap();
        assert_eq!(got, TOML_CONFIG);
        let got = remove_toml_path(TOML_CONFIG, "no.such.deep.path").unwrap();
        assert_eq!(got, TOML_CONFIG);
    }

    #[test]
    fn toml_set_creates_missing_parent_chain() {
        let src = "a = 1\n";
        let got = set_toml_path(src, "x.y.z", &toml::Value::Integer(7)).unwrap();
        // 中间层是隐式表,只印出真正带直属键值的那一层表头。
        assert_eq!(got, "a = 1\n\n[x.y]\nz = 7\n");
        match read_file(&write_tmp(&tempfile::tempdir().unwrap(), "c.toml", &got)).unwrap() {
            Doc::Toml(v) => assert_eq!(toml_path(&v, "x.y.z"), Some(&toml::Value::Integer(7))),
            other => panic!("应是 TOML: {other:?}"),
        }
    }

    #[test]
    fn toml_dotted_path_refuses_quoted_segments_instead_of_guessing() {
        // 键名含 `.` 只能写成带引号的段,而点路径表达不了 —— 报错,别去改错的键。
        let err = set_toml_path(TOML_CONFIG, "a.\"b.c\".d", &toml::Value::Integer(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("quoted"), "报错应点名引号问题: {err}");
        let err = set_toml_path(TOML_CONFIG, "a..b", &toml::Value::Integer(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty segment"), "空段应被拒绝: {err}");
    }

    #[test]
    fn toml_path_through_scalar_errors_and_names_the_path() {
        let err = set_toml_path(TOML_CONFIG, "model.deeper", &toml::Value::Integer(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("scalar"), "应说明走穿了标量: {err}");
        assert!(err.contains("model"), "报错必须点名出事的路径: {err}");
    }

    #[test]
    fn toml_crlf_input_stays_crlf_including_inserted_lines() {
        let src = "a = 1\r\n\r\n[t]\r\nk = \"v\"\r\n";
        let got = set_toml_path(src, "t.k2", &toml::Value::String("x".to_string())).unwrap();
        assert_eq!(got, "a = 1\r\n\r\n[t]\r\nk = \"v\"\r\nk2 = \"x\"\r\n");
        assert!(!got.replace("\r\n", "").contains('\n'), "不许留下裸 LF");
    }

    #[test]
    fn jsonc_comments_survive_a_nested_replacement() {
        let got = set_json_pointer(JSONC_CONFIG, "/b/c", &serde_json::json!("new")).unwrap();
        // 只有那一段值变了,两条注释和缩进逐字节不动。
        assert_eq!(got, JSONC_CONFIG.replace("\"old\"", "\"new\""));
        assert!(got.starts_with("// banner"), "首行 banner 必须还在");
        assert!(
            got.contains("  // 这条注释夹在两个键之间\n"),
            "键间注释必须还在"
        );
    }

    #[test]
    fn jsonc_comments_survive_an_insertion() {
        // 这是最容易在日后「顺手简化」时被弄丢的性质,单独钉死。
        let got = set_json_pointer(JSONC_CONFIG, "/b/d", &serde_json::json!(2)).unwrap();
        let want = "\
// banner:cursor / copilot 的配置就是这样开头的
{
  \"a\": 1,
  // 这条注释夹在两个键之间
  \"b\": {
    \"c\": \"old\",
    \"d\": 2
  }
}
";
        assert_eq!(got, want);
    }

    #[test]
    fn json_insertion_keeps_tab_indentation() {
        let src = "{\n\t\"a\": 1\n}\n";
        let got = set_json_pointer(src, "/b", &serde_json::json!(2)).unwrap();
        assert_eq!(got, "{\n\t\"a\": 1,\n\t\"b\": 2\n}\n");
    }

    #[test]
    fn json_insertion_keeps_four_space_indentation() {
        let src = "{\n    \"a\": 1\n}\n";
        let got = set_json_pointer(src, "/b", &serde_json::json!({"k": "v"})).unwrap();
        // 子层缩进按嗅探到的 4 空格递进。
        assert_eq!(
            got,
            "{\n    \"a\": 1,\n    \"b\": {\n        \"k\": \"v\"\n    }\n}\n"
        );
    }

    #[test]
    fn json_crlf_input_stays_crlf_including_inserted_lines() {
        let src = "{\r\n  \"a\": 1\r\n}\r\n";
        let got = set_json_pointer(src, "/b", &serde_json::json!({"k": 1})).unwrap();
        assert_eq!(
            got,
            "{\r\n  \"a\": 1,\r\n  \"b\": {\r\n    \"k\": 1\r\n  }\r\n}\r\n"
        );
        assert!(!got.replace("\r\n", "").contains('\n'), "不许留下裸 LF");
    }

    #[test]
    fn json_set_creates_missing_parent_chain() {
        let src = "{\n}\n";
        let got = set_json_pointer(src, "/a/b/c", &serde_json::json!(1)).unwrap();
        assert_eq!(
            got,
            "{\n  \"a\": {\n    \"b\": {\n      \"c\": 1\n    }\n  }\n}\n"
        );
        // 真实场景:文件里还没有 mcpServers,直接写 /mcpServers/foo。
        let got = set_json_pointer(
            "{\n  \"x\": 1\n}\n",
            "/mcpServers/foo",
            &serde_json::json!({"command": "npx", "args": ["-y", "foo"]}),
        )
        .unwrap();
        assert_eq!(
            got,
            "{\n  \"x\": 1,\n  \"mcpServers\": {\n    \"foo\": {\n      \"command\": \"npx\",\n      \"args\": [\"-y\", \"foo\"]\n    }\n  }\n}\n",
            "全标量数组保持单行,这才是人手写配置的样子"
        );
    }

    #[test]
    fn json_remove_missing_pointer_returns_input_byte_for_byte() {
        assert_eq!(
            remove_json_pointer(JSONC_CONFIG, "/nope/deep").unwrap(),
            JSONC_CONFIG
        );
        assert_eq!(
            remove_json_pointer(JSONC_CONFIG, "/b/zzz").unwrap(),
            JSONC_CONFIG
        );
    }

    #[test]
    fn json_remove_takes_the_whole_line_and_fixes_the_comma() {
        // 删中间的键:后面的键连缩进一起留在原位。
        let src = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n";
        assert_eq!(
            remove_json_pointer(src, "/b").unwrap(),
            "{\n  \"a\": 1,\n  \"c\": 3\n}\n"
        );
        // 删末位的键:前一个键的逗号必须跟着走,否则剩下严格 JSON 不认的尾逗号。
        assert_eq!(
            remove_json_pointer(src, "/c").unwrap(),
            "{\n  \"a\": 1,\n  \"b\": 2\n}\n"
        );
        // 单行对象:逗号后的空格也一并收走。
        assert_eq!(
            remove_json_pointer("{\"a\": 1, \"b\": 2}", "/a").unwrap(),
            "{\"b\": 2}"
        );
        // 只剩一个成员时删空,容器骨架留着。
        assert_eq!(
            remove_json_pointer("{\n  \"a\": 1\n}\n", "/a").unwrap(),
            "{\n}\n"
        );
    }

    #[test]
    fn json_remove_keeps_neighbouring_comments() {
        let got = remove_json_pointer(JSONC_CONFIG, "/b").unwrap();
        let want = "\
// banner:cursor / copilot 的配置就是这样开头的
{
  \"a\": 1
  // 这条注释夹在两个键之间
}
";
        assert_eq!(got, want, "被删成员之外的注释一条都不许少");
    }

    #[test]
    fn json_pointer_through_scalar_errors_and_names_the_path() {
        let src = "{\"a\": \"str\"}";
        let err = set_json_pointer(src, "/a/b", &serde_json::json!(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("scalar"), "应说明走穿了标量: {err}");
        assert!(err.contains("/a"), "报错必须点名出事的路径: {err}");
        let err = remove_json_pointer("{\"a\": {\"b\": 1}}", "/a/b/c")
            .unwrap_err()
            .to_string();
        assert!(err.contains("scalar"), "删除方向同样要拒绝: {err}");
    }

    #[test]
    fn json_write_refuses_the_whole_document_and_malformed_pointers() {
        assert!(set_json_pointer("{}", "", &serde_json::json!(1)).is_err());
        assert!(remove_json_pointer("{}", "").is_err());
        let err = set_json_pointer("{}", "a/b", &serde_json::json!(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("start with '/'"), "指针语法错误要说清: {err}");
    }

    #[test]
    fn json_write_preserves_bom_and_missing_trailing_newline() {
        let got =
            set_json_pointer("\u{feff}{\n  \"a\": 1\n}", "/b", &serde_json::json!(2)).unwrap();
        assert_eq!(got, "\u{feff}{\n  \"a\": 1,\n  \"b\": 2\n}");
        assert!(
            !got.ends_with('\n'),
            "原文没有末尾换行,新文本也不许凭空多一个"
        );
    }

    #[test]
    fn json_write_handles_trailing_comma_style_and_line_end_comments() {
        // VS Code 系的尾逗号风格:插入后仍旧留尾逗号。
        assert_eq!(
            set_json_pointer("{\n  \"a\": 1,\n}\n", "/b", &serde_json::json!(2)).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": 2,\n}\n"
        );
        // 逗号要补在值紧后面,不能跑到行尾注释后头去。
        assert_eq!(
            set_json_pointer(
                "{\n  \"a\": 1 // 行尾注释\n}\n",
                "/b",
                &serde_json::json!(2)
            )
            .unwrap(),
            "{\n  \"a\": 1, // 行尾注释\n  \"b\": 2\n}\n"
        );
    }

    #[test]
    fn json_array_append_and_element_removal() {
        let src = "{\n  \"args\": [\"-y\", \"foo\"]\n}\n";
        assert_eq!(
            set_json_pointer(src, "/args/-", &serde_json::json!("bar")).unwrap(),
            "{\n  \"args\": [\"-y\", \"foo\", \"bar\"]\n}\n"
        );
        assert_eq!(
            remove_json_pointer(src, "/args/0").unwrap(),
            "{\n  \"args\": [\"foo\"]\n}\n"
        );
        // 往中间塞会悄悄挪动别人的下标,拒绝。
        let err = set_json_pointer(src, "/args/9", &serde_json::json!("x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("out of range"), "越界下标要点名: {err}");
    }

    #[test]
    fn write_direction_round_trips_through_read_file() {
        let dir = tempfile::tempdir().unwrap();
        // 1) set_json_pointer
        let a =
            set_json_pointer(JSONC_CONFIG, "/b/c", &serde_json::json!({"deep": [1, 2]})).unwrap();
        match read_file(&write_tmp(&dir, "a.jsonc", &a)).unwrap() {
            Doc::Json(v) => {
                assert_eq!(json_pointer(&v, "/b/c/deep/1"), Some(&serde_json::json!(2)))
            }
            other => panic!("应是 JSON: {other:?}"),
        }
        // 2) remove_json_pointer
        let b = remove_json_pointer(JSONC_CONFIG, "/a").unwrap();
        match read_file(&write_tmp(&dir, "b.jsonc", &b)).unwrap() {
            Doc::Json(v) => {
                assert!(json_pointer(&v, "/a").is_none(), "/a 应已删除");
                assert_eq!(json_pointer(&v, "/b/c"), Some(&serde_json::json!("old")));
            }
            other => panic!("应是 JSON: {other:?}"),
        }
        // 3) set_toml_path
        let c = set_toml_path(
            TOML_CONFIG,
            "mcp_servers.duster",
            &tbl(&[("command", "duster")]),
        )
        .unwrap();
        match read_file(&write_tmp(&dir, "c.toml", &c)).unwrap() {
            Doc::Toml(v) => {
                assert_eq!(
                    toml_path(&v, "mcp_servers.duster.command").and_then(|x| x.as_str()),
                    Some("duster")
                );
                // 兄弟表还在。
                assert!(toml_path(&v, "mcp_servers.stitch.command").is_some());
            }
            other => panic!("应是 TOML: {other:?}"),
        }
        // 4) remove_toml_path
        let d = remove_toml_path(TOML_CONFIG, "mcp_servers.stitch").unwrap();
        match read_file(&write_tmp(&dir, "d.toml", &d)).unwrap() {
            Doc::Toml(v) => {
                assert!(
                    toml_path(&v, "mcp_servers.stitch").is_none(),
                    "stitch 应已删除"
                );
                assert!(
                    toml_path(&v, "mcp_servers.context7").is_some(),
                    "兄弟表必须还在"
                );
                assert_eq!(
                    toml_path(&v, "model").and_then(|x| x.as_str()),
                    Some("gpt-5")
                );
            }
            other => panic!("应是 TOML: {other:?}"),
        }
    }

    /// 形状照抄本机 `~/.cursor/argv.json`(qoder 的那份逐字节同构):多行 `//`
    /// banner、tab 缩进、每个键头上压着自己的注释块、键组之间留空行、有一个被
    /// 注释掉的成员、**结尾没有换行**。这份文件是 VS Code 系发出来的,用户自己
    /// 又在上面写过东西,重排版等同于毁掉它。
    const CURSOR_ARGV: &str = "\
// This configuration file allows you to pass permanent command line arguments.
//
// NOTE: Changing this file requires a restart.
{
\t// Use software rendering instead of hardware accelerated rendering.
\t// \"disable-hardware-acceleration\": true,

\t// Allows to disable crash reporting.
\t\"enable-crash-reporter\": true,

\t// Unique id used for correlating crash reports sent from this instance.
\t\"crash-reporter-id\": \"3339dddb\"
}";

    #[test]
    fn cursor_style_argv_json_survives_a_round_trip() {
        let added = set_json_pointer(
            CURSOR_ARGV,
            "/duster-probe",
            &serde_json::json!({"a": [1, 2]}),
        )
        .unwrap();
        let want = "\
// This configuration file allows you to pass permanent command line arguments.
//
// NOTE: Changing this file requires a restart.
{
\t// Use software rendering instead of hardware accelerated rendering.
\t// \"disable-hardware-acceleration\": true,

\t// Allows to disable crash reporting.
\t\"enable-crash-reporter\": true,

\t// Unique id used for correlating crash reports sent from this instance.
\t\"crash-reporter-id\": \"3339dddb\",
\t\"duster-probe\": {
\t\t\"a\": [1, 2]
\t}
}";
        assert_eq!(
            added, want,
            "banner、注释块、空行、tab 缩进、无末尾换行全都要活着"
        );
        // 加了再删必须逐字节回到原文。
        assert_eq!(
            remove_json_pointer(&added, "/duster-probe").unwrap(),
            CURSOR_ARGV
        );
        // 被注释掉的那个成员始终只是注释,不该被当成真成员。
        match read_file(&write_tmp(
            &tempfile::tempdir().unwrap(),
            "argv.json",
            &added,
        ))
        .unwrap()
        {
            Doc::Json(v) => {
                assert!(json_pointer(&v, "/disable-hardware-acceleration").is_none());
                assert_eq!(
                    json_pointer(&v, "/duster-probe/a/1"),
                    Some(&serde_json::json!(2))
                );
            }
            other => panic!("应是 JSON: {other:?}"),
        }
    }
}
