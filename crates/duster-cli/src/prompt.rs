//! 自研交互问句原语:y/N、单行输入、单选列表、勾选表。菜单里的每一张列表
//! 都从这里出,dialoguer 只剩 [`ColorfulTheme`] 在用(`?` / `✔` 这套视觉词汇)。
//!
//! dialoguer 0.11 的四个缺陷实测确认(源码级):
//! - `Confirm` 默认 `wait_for_newline = false` —— 按 `y` / `n` **立即生效**,
//!   不等回车;且不支持方向键;
//! - `Input` 的键循环里没有 `Escape` 分支 —— 按 Esc 被静默忽略,问句没有
//!   取消通道(只能 Ctrl-C 整个进程);
//! - `MultiSelect` 行上**没有光标**(条目前缀只有勾选框),而且勾选之间
//!   **不能联动**(中途没有回调),「全选」那一行只能说谎;
//! - `Select` / `MultiSelect` 的页高由它们**自己**从终端行数算
//!   (`paging.rs`:`max_length.clamp(3, rows) - 2`),调用方在它们上面已经
//!   印了几行它们并不知道 —— banner、表头、上一级回执一律不算,矮终端里
//!   把最后几行顶出屏幕;而且 `pages > 1` 才认 ←/→,一页装得下时那两个键
//!   静静地什么都不做,提示行却照旧写着「←/→ page」。
//!
//! 菜单问句是 duster 的门面,这四条只能自己修:这里用 `console::Term` 直接
//! 读键,渲染复用 dialoguer `ColorfulTheme` 的公开 [`Theme`] trait,样式与
//! dialoguer 组件零偏差(同一个 `?` 前缀、`(y/n)` 提示、`✔` 完成标记)。
//!
//! # 键约定
//!
//! - [`prompt_confirm`]:`↑` / `↓` / `←` / `→` 或 `y` / `n` 切换选择,
//!   **Enter 才确认**;`Esc` 取消。默认值只决定初始高亮,不决定回车结果——
//!   回车确认的是当前高亮的那一个。
//! - [`prompt_line`]:常规行编辑(字符 / 退格 / `←` / `→` / Home / End),
//!   Enter 提交(过 validator),`Esc` 取消。
//! - [`prompt_pick`] 与 [`prompt_checklist`]:`↑` / `↓` 环绕移动、`←` / `→`
//!   整页跳、Home / End 贴边;勾选表另有 `空格` 切一行、`a` 切全表。
//!   Enter 提交,`Esc` 取消。两张表的移动键**必须一模一样**——同一个菜单里
//!   两张表按同一个键做不同的事,比少一个键更糟。
//! - [`prompt_browse`]:上面两张表的合体(浏览 + 勾选同屏)。移动与勾选键
//!   同上,另认 `/`(把「过滤」交回调用方);Enter 无勾选 = 打开光标行,
//!   有勾选 = 对勾选集合动手(见 [`BrowseEvent`])。
//!
//! 提交与取消的区分是铁律:`Ok(Some(_))` = 提交了内容,`Ok(None)` = 用户取消,
//! 调用方原样回菜单,什么都没发生。Ctrl-C 仍由终端送 SIGINT(与 dialoguer
//! 一致),那是最后一扇逃生门,不在问句里处理。

use std::io;

use console::{Key, Style, Term, measure_text_width};
use dialoguer::theme::ColorfulTheme;
use dialoguer::theme::Theme;

/// 隐光标的 RAII 守卫。
///
/// 问句原语里 `hide_cursor` 与 `show_cursor` 之间夹着十几个 `?`,每一个
/// 都是一条不恢复光标的退出路径(`read_key` 在 stdin EOF / 被信号打断时
/// 真的会返回 `Err`)。靠人手工在每个分支前补一句是守不住的,交给 `Drop`。
///
/// Ctrl-C 盖不住:console 不清 ISIG,SIGINT 直接结束进程,`Drop` 不会跑。
/// 那是与 dialoguer 同档的行为,不在这里处理。
struct CursorGuard<'a>(&'a Term);

impl<'a> CursorGuard<'a> {
    fn hide(term: &'a Term) -> io::Result<Self> {
        term.hide_cursor()?;
        Ok(Self(term))
    }
}

impl Drop for CursorGuard<'_> {
    fn drop(&mut self) {
        // 恢复光标失败时无计可施:这一刻要么终端已经没了,要么正在担无可担。
        let _ = self.0.show_cursor();
    }
}

/// 一句 y/N。方向键或 `y`/`n` 切换,Enter 确认,Esc 取消。
pub fn prompt_confirm(
    theme: &ColorfulTheme,
    prompt: &str,
    default: bool,
) -> io::Result<Option<bool>> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "not a terminal",
        ));
    }
    let mut current = default;
    let _guard = CursorGuard::hide(&term)?;
    render_confirm(&term, theme, prompt, Some(current))?;
    let picked = loop {
        match confirm_key(term.read_key()?) {
            ConfirmAction::Toggle(v) => current = v,
            ConfirmAction::Submit => break Some(current),
            ConfirmAction::Cancel => break None,
            ConfirmAction::Ignore => continue, // 无关键不重绘
        }
        render_confirm(&term, theme, prompt, Some(current))?;
    };
    term.clear_line()?;
    if let Some(v) = picked {
        let mut buf = String::new();
        theme
            .format_confirm_prompt_selection(&mut buf, prompt, Some(v))
            .expect("writing to a String cannot fail");
        term.write_line(&buf)?;
    }
    Ok(picked)
}

/// 一行文本的校验器:返回 `Err(人话)` 则拒绝提交、报错后重问。
type Validator<'a> = Option<&'a dyn Fn(&str) -> Result<(), String>>;

/// 一行文本。Enter 提交(过 validator),Esc 取消。
///
/// 空输入算不算有效由 `validator` 决定:必填问句传非空校验(空输入报错重问),
/// 选填问句传 `None`(空提交原样返回,由调用方解释成"不带这个旗标")。
pub fn prompt_line(
    theme: &ColorfulTheme,
    prompt: &str,
    validator: Validator<'_>,
) -> io::Result<Option<String>> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "not a terminal",
        ));
    }
    let mut st = LineState::default();
    let _guard = CursorGuard::hide(&term)?;
    render_line(&term, theme, prompt, &st.chars, st.pos)?;
    let text = loop {
        match line_key(&mut st, term.read_key()?) {
            LineAction::Edited => {}
            LineAction::Submit(text) => {
                // 校验失败:错误行印在问句上面,然后重绘问句——输入还在,改完再交。
                // 先擦掉问句那一行:光标还停在它上面,不擦就会把错误接在问句尾巴后面。
                if let Some(err) = validator.and_then(|v| v(&text).err()) {
                    let mut err_buf = String::new();
                    theme
                        .format_error(&mut err_buf, &err)
                        .expect("writing to a String cannot fail");
                    term.clear_line()?;
                    term.write_line(&err_buf)?;
                    render_line(&term, theme, prompt, &st.chars, st.pos)?;
                    continue;
                }
                break text;
            }
            LineAction::Cancel => {
                term.clear_line()?;
                return Ok(None);
            }
            LineAction::Ignore => continue, // 控制字符忽略,不重绘
        }
        render_line(&term, theme, prompt, &st.chars, st.pos)?;
    };
    term.clear_line()?;
    let mut buf = String::new();
    theme
        .format_input_prompt_selection(&mut buf, prompt, &text)
        .expect("writing to a String cannot fail");
    term.write_line(&buf)?;
    Ok(Some(text))
}

/// 一张勾选表:多选、带光标、可翻页,第一行可以是「全选」行。
///
/// dialoguer 的 `MultiSelect` 有两条修不了的缺陷(源码级确认,
/// `theme/colorful.rs::format_multi_select_prompt_item`):
///
/// - **行上没有光标**。它的条目前缀只有勾选框,「当前在哪一行」全靠给正文
///   套一个 `active_item_style`——纯颜色差。`Select` 有 `❯` 前缀所以不吃这个
///   亏,多选一屏十几行长得一模一样,谁也说不出光标在哪(复制粘贴出去连
///   颜色都没了)。这里给当前行一个真实的位置标记。
/// - **勾选之间不能联动**。`MultiSelect` 只在结束时一次性交出勾选结果,中途
///   没有任何回调,所以「全选」那一行勾上之后其余行**不会跟着变**——一张说
///   了谎的勾选表比没有这一行更糟。这里的 `select_all` 行是真的联动。
pub struct Checklist<'a> {
    /// 问句正文(键位提示由调用方写进来,与其他问句一致)。
    pub prompt: &'a str,
    /// 可选表头:多列条目才需要,缩进与条目正文对齐。
    pub header: Option<&'a str>,
    /// 每行正文。
    pub items: &'a [String],
    /// 初始勾选,长度必须与 `items` 一致。
    pub checked: Vec<bool>,
    /// 第 0 行是不是「全选」行:勾它 = 全勾,取消它 = 全不勾;其余行全勾时
    /// 它自动跟着勾上,任意一行取消时它自动取消。
    pub select_all: bool,
    /// 一屏最多几行条目(不含问句、表头与翻页提示)。
    pub page: usize,
}

/// 跑一张勾选表。`Ok(Some(勾选下标))`(含「全选」行自己的 0),
/// `Ok(None)` = Esc 取消,调用方原样退回,什么都没发生。
///
/// 退场时把自己画的每一行都擦掉:回显归调用方——它才知道这一屏问出来的
/// 东西该怎么说成一句人话。
pub fn prompt_checklist(
    theme: &ColorfulTheme,
    list: Checklist<'_>,
) -> io::Result<Option<Vec<usize>>> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "not a terminal",
        ));
    }
    debug_assert_eq!(list.checked.len(), list.items.len());
    if list.items.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let page = body_page(list.page, list.header.is_some(), list.items.len());
    let mut st = ChecklistState::new(list.checked.clone(), page, list.select_all);
    let _guard = CursorGuard::hide(&term)?;
    let mut drawn = render_marked(
        &term, theme, list.prompt, list.header, list.items, &st.checked, &st.view, 0,
    )?;
    let submitted = loop {
        match checklist_key(&mut st, term.read_key()?) {
            ChecklistAction::Redraw => {}
            ChecklistAction::Submit => break true,
            ChecklistAction::Cancel => break false,
            ChecklistAction::Ignore => continue,
        }
        drawn = render_marked(
            &term, theme, list.prompt, list.header, list.items, &st.checked, &st.view, drawn,
        )?;
    };
    term.clear_last_lines(drawn)?;
    if !submitted {
        return Ok(None);
    }
    Ok(Some(
        st.checked
            .iter()
            .enumerate()
            .filter(|(_, c)| **c)
            .map(|(i, _)| i)
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// 键 → 状态机的纯函数部分。渲染与 IO 不测(要真终端),这两块是全部逻辑,
// 抽出来让键序列可以直接断言。
// ---------------------------------------------------------------------------

/// y/N 问句的一键结果。
enum ConfirmAction {
    /// 切到这一侧(y / n / 方向键)。
    Toggle(bool),
    /// Enter:确认当前高亮。
    Submit,
    /// Esc:取消,什么都不发生。
    Cancel,
    /// 其余键:不动、不重绘。
    Ignore,
}

fn confirm_key(key: Key) -> ConfirmAction {
    match key {
        // 上下左右四向都认:方向键比单键更不容易误触。
        Key::ArrowUp | Key::ArrowLeft | Key::Char('y' | 'Y') => ConfirmAction::Toggle(true),
        Key::ArrowDown | Key::ArrowRight | Key::Char('n' | 'N') => ConfirmAction::Toggle(false),
        Key::Enter => ConfirmAction::Submit,
        Key::Escape => ConfirmAction::Cancel,
        _ => ConfirmAction::Ignore,
    }
}

/// 文本输入状态:已输入字符 + 光标位置。
#[derive(Default)]
struct LineState {
    chars: Vec<char>,
    pos: usize,
}

impl LineState {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }
}

/// 文本问句的一键结果。
enum LineAction {
    /// 内容或光标动了,需要重绘。
    Edited,
    /// Enter:带着当前文本提交(是否通过 validator 由调用方裁决)。
    Submit(String),
    /// Esc:取消,什么都不发生。
    Cancel,
    /// 其余键:不动、不重绘。
    Ignore,
}

fn line_key(st: &mut LineState, key: Key) -> LineAction {
    match key {
        Key::Backspace if st.pos > 0 => {
            st.chars.remove(st.pos - 1);
            st.pos -= 1;
            LineAction::Edited
        }
        Key::Del if st.pos < st.chars.len() => {
            st.chars.remove(st.pos);
            LineAction::Edited
        }
        Key::Home => {
            st.pos = 0;
            LineAction::Edited
        }
        Key::End => {
            st.pos = st.chars.len();
            LineAction::Edited
        }
        Key::ArrowLeft if st.pos > 0 => {
            st.pos -= 1;
            LineAction::Edited
        }
        Key::ArrowRight if st.pos < st.chars.len() => {
            st.pos += 1;
            LineAction::Edited
        }
        Key::Char(c) if !c.is_ascii_control() => {
            st.chars.insert(st.pos, c);
            st.pos += 1;
            LineAction::Edited
        }
        Key::Enter => LineAction::Submit(st.text()),
        Key::Escape => LineAction::Cancel,
        _ => LineAction::Ignore,
    }
}

/// 一屏能放几行**条目**:调用方给的是这张表可用的总行数,里面还要养表头与
/// 翻页提示各一行。
///
/// 翻页提示只在真的多于一页时才画,所以这里要试一次:先按「让出提示行」算,
/// 让出后一页就装得下全部条目,那就没有提示行,把那一行还回来。少了这一步,
/// 恰好差一行的表会白白翻成两页。
pub(crate) fn body_page(avail: usize, has_header: bool, len: usize) -> usize {
    let body = avail.saturating_sub(usize::from(has_header));
    let paged = body.saturating_sub(1);
    if paged >= len { body } else { paged }.clamp(1, len.max(1))
}

/// 一屏之内的位置:总行数、光标、视窗起点、一页几行。
///
/// 勾选表(多选)与列表选择(单选)共用同一套移动语义:上下环绕、←/→ 整页、
/// Home / End 贴边,光标出屏就把视窗挪过去。两处的键必须一模一样——同一个
/// 菜单里两张表按同一个键做不同的事,是比少一个键更糟的事。
struct Viewport {
    len: usize,
    cursor: usize,
    /// 视窗第一行的下标。
    off: usize,
    /// 一屏最多几行条目。
    page: usize,
}

impl Viewport {
    fn new(len: usize, page: usize) -> Self {
        Self {
            len,
            cursor: 0,
            off: 0,
            page: page.clamp(1, len.max(1)),
        }
    }

    /// 视窗:从 `off` 起最多 `page` 行。
    fn visible(&self) -> std::ops::Range<usize> {
        self.off..(self.off + self.page).min(self.len)
    }

    /// 条目多于一页才需要「第几到第几 / 共几行」这一行。
    fn footer(&self) -> Option<String> {
        (self.page < self.len).then(|| {
            let last = (self.off + self.page).min(self.len);
            format!("{}-{} of {}", self.off + 1, last, self.len)
        })
    }

    /// 光标出了视窗就把视窗挪过去,一次挪一行(翻页时贴边)。
    fn scroll_into_view(&mut self) {
        if self.cursor < self.off {
            self.off = self.cursor;
        } else if self.cursor >= self.off + self.page {
            self.off = self.cursor + 1 - self.page;
        }
    }

    /// 移动键:认了就返回 true(调用方据此重绘),不认返回 false。
    fn nav(&mut self, key: &Key) -> bool {
        let last = self.len - 1;
        match key {
            // 上下环绕:十几行的表里,从头一路按到尾比按 Home 更顺手。
            Key::ArrowUp => {
                self.cursor = if self.cursor == 0 {
                    last
                } else {
                    self.cursor - 1
                }
            }
            Key::ArrowDown => {
                self.cursor = if self.cursor == last {
                    0
                } else {
                    self.cursor + 1
                }
            }
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = last,
            Key::ArrowLeft | Key::PageUp => self.cursor = self.cursor.saturating_sub(self.page),
            Key::ArrowRight | Key::PageDown => self.cursor = (self.cursor + self.page).min(last),
            _ => return false,
        }
        self.scroll_into_view();
        true
    }
}

/// 勾选表状态:勾选位 + 视窗。
struct ChecklistState {
    checked: Vec<bool>,
    select_all: bool,
    view: Viewport,
}

impl ChecklistState {
    fn new(checked: Vec<bool>, page: usize, select_all: bool) -> Self {
        let view = Viewport::new(checked.len(), page);
        let mut st = Self {
            checked,
            select_all,
            view,
        };
        st.sync_all_row();
        st
    }

    /// 其余行是否全勾。「全选」行自己不算,否则这就是个自指的死结。
    fn rest_all_checked(&self) -> bool {
        self.checked.len() > 1 && self.checked[1..].iter().all(|&c| c)
    }

    /// 「全选」行跟着其余行走:全勾则勾上,任意一行没勾就取消。
    fn sync_all_row(&mut self) {
        if self.select_all && self.checked.len() > 1 {
            self.checked[0] = self.rest_all_checked();
        }
    }

    fn set_all(&mut self, v: bool) {
        self.checked.iter_mut().for_each(|c| *c = v);
    }

    /// 切一行。「全选」行带着全表一起走;普通行切完要回头校正「全选」行。
    fn toggle(&mut self, i: usize) {
        let v = !self.checked[i];
        if self.select_all && i == 0 {
            self.set_all(v);
        } else {
            self.checked[i] = v;
            self.sync_all_row();
        }
    }
}

/// 勾选表的一键结果。
enum ChecklistAction {
    /// 状态变了,重绘。
    Redraw,
    /// Enter:交出当前勾选。
    Submit,
    /// Esc:取消,什么都不发生。
    Cancel,
    /// 其余键:不动、不重绘。
    Ignore,
}

fn checklist_key(st: &mut ChecklistState, key: Key) -> ChecklistAction {
    if st.view.nav(&key) {
        return ChecklistAction::Redraw;
    }
    match key {
        Key::Char(' ') => {
            let i = st.view.cursor;
            st.toggle(i);
        }
        // `a` 是「全都要 / 全不要」的快捷键:没有「全选」行的表也能用。
        Key::Char('a' | 'A') => {
            let v = !st.checked.iter().all(|&c| c);
            st.set_all(v);
        }
        Key::Enter => return ChecklistAction::Submit,
        Key::Escape => return ChecklistAction::Cancel,
        _ => return ChecklistAction::Ignore,
    }
    ChecklistAction::Redraw
}

/// 一张单选列表:带光标、按终端高度翻页、Enter 交出下标。
///
/// dialoguer 的 `Select` 会翻页,但页高由它**自己**从终端行数重算
/// (`paging.rs`:`max_length.clamp(3, rows) - 2`),调用方在它上面已经印了
/// 什么它并不知道,于是表头 + 提示 + 问句那几行不算在内——20 行的表在
/// 24 行的终端里正好顶出屏幕,滚上去的部分还带着重绘残影。更要紧的是
/// `pages > 1` 才认 ←/→:一页装得下时那两个键**静静地什么都不做**,而提示
/// 行照旧写着「←/→ page」。页高必须由知道自己印了几行的人来定。
pub struct Picker<'a> {
    /// 问句正文。
    pub prompt: &'a str,
    /// 表头:多列条目的列名,缩进与条目正文对齐。
    pub header: Option<&'a str>,
    /// 每行正文(调用方已对齐)。
    pub items: &'a [String],
    /// 一屏最多几行条目(不含问句、表头与翻页提示)。
    pub page: usize,
    /// 初始光标停在第几行(菜单的「默认选中」)。越界按 0 算。
    pub start: usize,
}

/// 跑一张单选列表。`Ok(Some(下标))` = Enter 选中,`Ok(None)` = Esc 退回。
///
/// 与 [`prompt_checklist`] 一样退场时擦干净自己画的每一行:回显归调用方。
pub fn prompt_pick(theme: &ColorfulTheme, list: Picker<'_>) -> io::Result<Option<usize>> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "not a terminal",
        ));
    }
    if list.items.is_empty() {
        return Ok(None);
    }
    let page = body_page(list.page, list.header.is_some(), list.items.len());
    let mut view = Viewport::new(list.items.len(), page);
    view.cursor = if list.start < list.items.len() {
        list.start
    } else {
        0
    };
    view.scroll_into_view();
    let _guard = CursorGuard::hide(&term)?;
    let mut drawn = render_pick(&term, theme, &list, &view, 0)?;
    let picked = loop {
        let key = term.read_key()?;
        if view.nav(&key) {
            drawn = render_pick(&term, theme, &list, &view, drawn)?;
            continue;
        }
        match key {
            Key::Enter => break Some(view.cursor),
            Key::Escape => break None,
            _ => continue,
        }
    };
    term.clear_last_lines(drawn)?;
    Ok(picked)
}

/// browse 列表的一次交互收场。Enter 的去向由勾选状态决定:一行都没勾时
/// 是「打开光标行」,勾了就是「对勾选集合动手」——下钻与批量不再是两张表。
#[derive(Debug, PartialEq, Eq)]
pub enum BrowseEvent {
    /// Enter 且一行都没勾:打开光标行(下钻详情)。
    Open(usize),
    /// Enter 且有勾选:对勾选的行动手(下标升序)。
    Act(Vec<usize>),
    /// `/`:调用方去问过滤词。控件不做行编辑——过滤词归 [`prompt_line`],
    /// 过滤集合归调用方(只有它认识行文本与原始下标的映射)。
    Filter,
    /// Esc。「过滤态先清过滤再退屏」也由调用方决定:控件不知道自己看到
    /// 的行是不是被滤过的。
    Cancel,
}

/// 一张可勾选的浏览表。与 [`Picker`] 只差 `checked`:勾选位归调用方所有,
/// 控件原地改——过滤与下钻的来回之间勾选要保得住,状态就不能锁在控件的
/// 栈帧里。
pub struct Browser<'a> {
    /// 问句正文(键位提示由调用方写进来,与其他问句一致)。
    pub prompt: &'a str,
    /// 表头:多列条目的列名,缩进与条目正文对齐。
    pub header: Option<&'a str>,
    /// 每行正文(调用方已对齐)。
    pub items: &'a [String],
    /// 一屏最多几行条目(不含问句、表头与翻页提示)。
    pub page: usize,
    /// 初始光标停在第几行。越界按 0 算。
    pub start: usize,
    /// 勾选位,长度必须与 `items` 一致。
    pub checked: &'a mut [bool],
}

/// 跑一张浏览表。移动键与 [`prompt_pick`] 一模一样,勾选键(`空格` / `a`)
/// 与 [`prompt_checklist`] 一模一样——同一个菜单里两张表按同一个键做不同
/// 的事,比少一个键更糟,这条铁律在合体控件上同样成立。
///
/// 与两位亲戚一样退场时擦干净自己画的每一行:回显归调用方。
pub fn prompt_browse(theme: &ColorfulTheme, list: Browser<'_>) -> io::Result<BrowseEvent> {
    let term = Term::stderr();
    if !term.is_term() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "not a terminal",
        ));
    }
    debug_assert_eq!(list.checked.len(), list.items.len());
    if list.items.is_empty() {
        return Ok(BrowseEvent::Cancel);
    }
    let page = body_page(list.page, list.header.is_some(), list.items.len());
    let mut view = Viewport::new(list.items.len(), page);
    view.cursor = if list.start < list.items.len() {
        list.start
    } else {
        0
    };
    view.scroll_into_view();
    let _guard = CursorGuard::hide(&term)?;
    let mut drawn = render_marked(
        &term, theme, list.prompt, list.header, list.items, list.checked, &view, 0,
    )?;
    let event = loop {
        let key = term.read_key()?;
        if view.nav(&key) {
            drawn = render_marked(
                &term, theme, list.prompt, list.header, list.items, list.checked, &view, drawn,
            )?;
            continue;
        }
        match browse_key(list.checked, view.cursor, key) {
            BrowseAction::Redraw => {
                drawn = render_marked(
                    &term, theme, list.prompt, list.header, list.items, list.checked, &view,
                    drawn,
                )?;
            }
            BrowseAction::Done(ev) => break ev,
            BrowseAction::Ignore => {}
        }
    };
    term.clear_last_lines(drawn)?;
    Ok(event)
}

/// browse 的一键结果(移动键在进这里之前已被 [`Viewport::nav`] 吃掉)。
enum BrowseAction {
    Redraw,
    Done(BrowseEvent),
    Ignore,
}

/// browse 的键 → 状态机。与 [`checklist_key`] 只差两处:`/` 交出过滤请求,
/// Enter 按「有没有勾选」分流——这正是这个控件存在的理由。抽成纯函数,
/// 键序列可以直接断言,不必 mock 终端。
fn browse_key(checked: &mut [bool], cursor: usize, key: Key) -> BrowseAction {
    match key {
        Key::Char(' ') => {
            checked[cursor] = !checked[cursor];
            BrowseAction::Redraw
        }
        // `a` 是「全都要 / 全不要」:有一行没勾就全勾,已勾满就全清。
        Key::Char('a' | 'A') => {
            let v = !checked.iter().all(|&c| c);
            checked.iter_mut().for_each(|c| *c = v);
            BrowseAction::Redraw
        }
        Key::Char('/') => BrowseAction::Done(BrowseEvent::Filter),
        Key::Enter => {
            let sel: Vec<usize> = checked
                .iter()
                .enumerate()
                .filter(|(_, c)| **c)
                .map(|(i, _)| i)
                .collect();
            BrowseAction::Done(if sel.is_empty() {
                BrowseEvent::Open(cursor)
            } else {
                BrowseEvent::Act(sel)
            })
        }
        Key::Escape => BrowseAction::Done(BrowseEvent::Cancel),
        _ => BrowseAction::Ignore,
    }
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

/// 重绘 y/N 问句行。**原地**重绘:`clear_line` 回到本行行首并擦掉这一行,
/// 而不是 `clear_last_lines(1)` ——后者按定义擦的是「当前行之前的 n 行」,
/// 而问句是 `write_str` 印的、光标始终停在问句这一行上,于是每按一次键
/// 就往上啃掉一行已经印好的内容(计划清单就在上面),旧的那版问句留在原地
/// 不擦:切一次 y/n 多一行残影,切十几次就是十几行。
fn render_confirm(
    term: &Term,
    theme: &ColorfulTheme,
    prompt: &str,
    default: Option<bool>,
) -> io::Result<()> {
    term.clear_line()?;
    let mut buf = String::new();
    theme
        .format_confirm_prompt(&mut buf, prompt, default)
        .expect("writing to a String cannot fail");
    term.write_str(&buf)?;
    term.flush()
}

/// 重绘一张带勾选框的表,返回这一版画了几行(下一版据此擦干净)。
/// 勾选表([`prompt_checklist`])与浏览表([`prompt_browse`])共用这一份:
/// 两者的屏上形状一模一样,分开写就是两处漂移。
///
/// 每行的形状是 `❯ [x] 正文`:**位置标记与勾选状态分开两个字符位**。
/// 光标只用颜色表示是不够的——一屏十几行长得一样,而颜色在复制粘贴、
/// 在 `NO_COLOR`、在不认 ANSI 的终端里全部消失,位置信息就跟着没了。
///
/// 条目多于一页时最后补一行「第几到第几 / 共几行」:不写这一行,用户
/// 无法知道自己看的是全部还是一角。
#[allow(clippy::too_many_arguments)]
fn render_marked(
    term: &Term,
    theme: &ColorfulTheme,
    prompt: &str,
    header: Option<&str>,
    items: &[String],
    checked: &[bool],
    view: &Viewport,
    drawn: usize,
) -> io::Result<usize> {
    if drawn > 0 {
        term.clear_last_lines(drawn)?;
    }
    let mut lines = 0;
    let mut buf = String::new();
    theme
        .format_prompt(&mut buf, prompt)
        .expect("writing to a String cannot fail");
    term.write_line(&buf)?;
    lines += 1;
    // 表头缩进 6 列:与条目正文(`❯ [x] `)对齐。
    if let Some(header) = header {
        term.write_line(&format!(
            "      {}",
            Style::new().for_stderr().bold().apply_to(header)
        ))?;
        lines += 1;
    }
    for i in view.visible() {
        let mark = if checked[i] {
            Style::new().for_stderr().green().apply_to("[x]")
        } else {
            Style::new().for_stderr().dim().apply_to("[ ]")
        };
        write_row(term, &items[i], i == view.cursor, Some(mark))?;
        lines += 1;
    }
    lines += write_footer(term, view)?;
    term.flush()?;
    Ok(lines)
}

/// 重绘整张单选列表。与勾选表同一套形状,只是没有勾选框那一列。
fn render_pick(
    term: &Term,
    theme: &ColorfulTheme,
    list: &Picker<'_>,
    view: &Viewport,
    drawn: usize,
) -> io::Result<usize> {
    if drawn > 0 {
        term.clear_last_lines(drawn)?;
    }
    let mut lines = 0;
    let mut buf = String::new();
    theme
        .format_prompt(&mut buf, list.prompt)
        .expect("writing to a String cannot fail");
    term.write_line(&buf)?;
    lines += 1;
    // 表头缩进 2 列:与条目正文(`❯ `)对齐。
    if let Some(header) = list.header {
        term.write_line(&format!(
            "  {}",
            Style::new().for_stderr().bold().apply_to(header)
        ))?;
        lines += 1;
    }
    for i in view.visible() {
        write_row(term, &list.items[i], i == view.cursor, None)?;
        lines += 1;
    }
    lines += write_footer(term, view)?;
    term.flush()?;
    Ok(lines)
}

/// 一行条目:`❯ [x] 正文` 或 `❯ 正文`。位置标记单独占一列,不靠颜色。
fn write_row(
    term: &Term,
    text: &str,
    at_cursor: bool,
    mark: Option<console::StyledObject<&str>>,
) -> io::Result<()> {
    let cursor = if at_cursor {
        Style::new().for_stderr().cyan().bold().apply_to("❯")
    } else {
        Style::new().for_stderr().apply_to(" ")
    };
    let body = if at_cursor {
        Style::new().for_stderr().bold().apply_to(text)
    } else {
        Style::new().for_stderr().apply_to(text)
    };
    match mark {
        Some(mark) => term.write_line(&format!("{cursor} {mark} {body}")),
        None => term.write_line(&format!("{cursor} {body}")),
    }
}

/// 多于一页时补一行「第几到第几 / 共几行」;不写这一行,用户无法知道
/// 自己看的是全部还是一角。返回补了几行。
fn write_footer(term: &Term, view: &Viewport) -> io::Result<usize> {
    match view.footer() {
        Some(text) => {
            term.write_line(&format!(
                "  {}",
                Style::new().for_stderr().dim().apply_to(text)
            ))?;
            Ok(1)
        }
        None => Ok(0),
    }
}

/// 重绘文本输入行:问句 + 已输入文本,光标停在 `pos`。原地重绘,理由同
/// [`render_confirm`]。
///
/// 光标按**显示宽度**左移(`measure_text_width`),CJK 字符占两列,按字符数
/// 移动会错位。输入超出终端宽度会折行,`clear_line` 只清一行——
/// 问句的实际输入(agent id / rid / 路径)都短,这条限制只在超长输入时留残影。
fn render_line(
    term: &Term,
    theme: &ColorfulTheme,
    prompt: &str,
    chars: &[char],
    pos: usize,
) -> io::Result<()> {
    term.clear_line()?;
    let mut buf = String::new();
    theme
        .format_input_prompt(&mut buf, prompt, None)
        .expect("writing to a String cannot fail");
    let text: String = chars.iter().collect();
    buf.push_str(&text);
    term.write_str(&buf)?;
    let tail: String = chars[pos..].iter().collect();
    if !tail.is_empty() {
        term.move_cursor_left(measure_text_width(&tail))?;
    }
    term.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 方向键与 y/n 都切换选择;Enter 只确认当前高亮;Esc 取消;无关键不动。
    #[test]
    fn confirm_键序列_方向键切换_回车才确认() {
        use Key::*;
        // 方向键与 y/n 都切到同一侧;Enter 只确认当前高亮;Esc 取消;无关键不动。
        for k in [ArrowUp, ArrowLeft, Char('y'), Char('Y')] {
            assert!(matches!(confirm_key(k), ConfirmAction::Toggle(true)));
        }
        for k in [ArrowDown, ArrowRight, Char('n')] {
            assert!(matches!(confirm_key(k), ConfirmAction::Toggle(false)));
        }
        // 无关键既不切换也不提交
        assert!(matches!(confirm_key(Char('x')), ConfirmAction::Ignore));
        // 回车确认的是当前值,与默认无关
        assert!(matches!(confirm_key(Enter), ConfirmAction::Submit));
        assert!(matches!(confirm_key(Escape), ConfirmAction::Cancel));
    }

    /// 编辑序列:输入 → 左移插入 → Home → 退格,提交时文本与光标位置正确。
    #[test]
    fn line_键序列_编辑与提交() {
        use Key::*;
        let mut st = LineState::default();
        for k in [Char('a'), Char('b'), Char('c')] {
            assert!(matches!(line_key(&mut st, k), LineAction::Edited));
        }
        assert_eq!(st.text(), "abc");
        assert_eq!(st.pos, 3);

        let _ = line_key(&mut st, ArrowLeft); // 光标在 c 前
        let _ = line_key(&mut st, Char('X'));
        assert_eq!(st.text(), "abXc");

        let _ = line_key(&mut st, Home);
        let _ = line_key(&mut st, Char('0'));
        assert_eq!(st.text(), "0abXc");

        let _ = line_key(&mut st, Backspace); // 删 0
        assert_eq!(st.text(), "abXc");
        assert_eq!(st.pos, 0);

        let _ = line_key(&mut st, End);
        assert!(matches!(line_key(&mut st, Enter), LineAction::Submit(s) if s == "abXc"));
    }

    /// Esc 在两个问句里都是「取消」,与提交严格分开。
    #[test]
    fn esc_取消_两个问句一致() {
        assert!(matches!(confirm_key(Key::Escape), ConfirmAction::Cancel));
        let mut st = LineState::default();
        assert!(matches!(line_key(&mut st, Key::Escape), LineAction::Cancel));
        // 空状态按退格 / 左移 / Del 都是 Ignore,不会 panic
        assert!(matches!(
            line_key(&mut st, Key::Backspace),
            LineAction::Ignore
        ));
        assert!(matches!(
            line_key(&mut st, Key::ArrowLeft),
            LineAction::Ignore
        ));
        assert!(matches!(line_key(&mut st, Key::Del), LineAction::Ignore));
    }

    /// 勾选表的键序列 helper:造一张 n 行、每页 page 行的表。
    fn list(n: usize, page: usize, select_all: bool) -> ChecklistState {
        ChecklistState::new(vec![true; n], page, select_all)
    }

    /// 「全选」行必须真的联动——这是自研这张表的头号理由。
    /// dialoguer 的 MultiSelect 中途没有回调,勾了 `All` 其余行一动不动。
    #[test]
    fn 勾选表_全选行双向联动() {
        // 1 + 3 行,初始全勾 → 全选行自然是勾上的
        let mut st = list(4, 10, true);
        assert!(st.checked[0], "其余行全勾时,全选行必须自己勾上");

        // 取消其中一行:全选行必须跟着取消,否则它在说谎
        st.view.cursor = 2;
        let _ = checklist_key(&mut st, Key::Char(' '));
        assert_eq!(st.checked, vec![false, true, false, true]);

        // 勾回来:全选行自动恢复
        let _ = checklist_key(&mut st, Key::Char(' '));
        assert_eq!(st.checked, vec![true, true, true, true]);

        // 取消全选行:全表清空(含它自己)
        st.view.cursor = 0;
        let _ = checklist_key(&mut st, Key::Char(' '));
        assert_eq!(st.checked, vec![false; 4]);

        // 再勾全选行:全表勾满
        let _ = checklist_key(&mut st, Key::Char(' '));
        assert_eq!(st.checked, vec![true; 4]);
    }

    /// 没有「全选」行的表(计划勾选表)第 0 行就是普通一行,不许联动。
    #[test]
    fn 勾选表_无全选行时第0行是普通行() {
        let mut st = list(3, 10, false);
        let _ = checklist_key(&mut st, Key::Char(' '));
        assert_eq!(st.checked, vec![false, true, true]);
        // `a` 键在两种表里都管全勾 / 全不勾
        let _ = checklist_key(&mut st, Key::Char('a'));
        assert_eq!(st.checked, vec![true; 3], "有未勾的行时 a 应勾满");
        let _ = checklist_key(&mut st, Key::Char('a'));
        assert_eq!(st.checked, vec![false; 3], "已勾满时 a 应清空");
    }

    /// 光标环绕、整页跳,以及视窗跟着光标走(超过一页的表才有这回事)。
    #[test]
    fn 勾选表_光标环绕与视窗跟随() {
        let mut st = list(12, 5, false);
        assert_eq!((st.view.cursor, st.view.off), (0, 0));

        // 上箭头从头绕到尾:视窗必须跟到底,否则光标停在看不见的地方
        let _ = checklist_key(&mut st, Key::ArrowUp);
        assert_eq!(st.view.cursor, 11);
        assert_eq!(st.view.visible(), 7..12);

        // 下箭头从尾绕回头,视窗贴顶
        let _ = checklist_key(&mut st, Key::ArrowDown);
        assert_eq!((st.view.cursor, st.view.off), (0, 0));

        // 整页跳:一次 5 行,末尾贴边不越界
        let _ = checklist_key(&mut st, Key::ArrowRight);
        assert_eq!(st.view.cursor, 5);
        let _ = checklist_key(&mut st, Key::ArrowRight);
        let _ = checklist_key(&mut st, Key::ArrowRight);
        assert_eq!(st.view.cursor, 11);
        // 往回一页
        let _ = checklist_key(&mut st, Key::ArrowLeft);
        assert_eq!(st.view.cursor, 6);
        assert!(st.view.visible().contains(&6));

        let _ = checklist_key(&mut st, Key::Home);
        assert_eq!((st.view.cursor, st.view.off), (0, 0));
        let _ = checklist_key(&mut st, Key::End);
        assert_eq!(st.view.cursor, 11);
    }

    /// 提交与取消严格分开,无关键不重绘。
    #[test]
    fn 勾选表_提交取消与无关键() {
        let mut st = list(3, 10, false);
        assert!(matches!(
            checklist_key(&mut st, Key::Enter),
            ChecklistAction::Submit
        ));
        assert!(matches!(
            checklist_key(&mut st, Key::Escape),
            ChecklistAction::Cancel
        ));
        assert!(matches!(
            checklist_key(&mut st, Key::Char('z')),
            ChecklistAction::Ignore
        ));
        // 一行的表:环绕不能越界
        let mut one = list(1, 10, false);
        let _ = checklist_key(&mut one, Key::ArrowUp);
        assert_eq!(one.view.cursor, 0);
        let _ = checklist_key(&mut one, Key::ArrowDown);
        assert_eq!(one.view.cursor, 0);
    }

    /// 页高由**知道自己印了几行的人**来定,这是自研这张单选表的头号理由。
    ///
    /// dialoguer 的 `Select` 从终端行数自己重算页高,调用方在它上面印的
    /// 表头与提示不算在内——20 行的表在 24 行的终端里正好顶出屏幕。
    #[test]
    fn 页高_把表头与翻页提示让出来() {
        // 12 行可用、有表头、20 条:表头 1 行 + 提示 1 行,条目只能放 10 行
        assert_eq!(body_page(12, true, 20), 10);
        // 没有表头就多放一行
        assert_eq!(body_page(12, false, 20), 11);
        // 恰好差一行:让出提示行后正好装得下全部条目,那就没有提示行,
        // 把那一行还回来——不试这一次,10 条的表会白白翻成两页
        assert_eq!(body_page(11, false, 10), 10);
        // 装得下就一页装完,不许把页高压到条目数以下
        assert_eq!(body_page(40, true, 6), 6);
        // 极端窄:至少留一行,不能是 0(视窗会除以它)
        assert_eq!(body_page(1, true, 5), 1);
        assert_eq!(body_page(0, false, 5), 1);
    }

    /// ←/→ 在**任何**行数下都真的翻页:dialoguer 只在 `pages > 1` 时才认这
    /// 两个键,一页装得下时它们静静地什么都不做,而提示行照旧写着「←/→ page」。
    /// 这里翻页只是「光标跳一页」,一页装得下时它等价于跳到末行 —— 有反应,
    /// 不是死键。
    #[test]
    fn 单选表_翻页与环绕() {
        let mut v = Viewport::new(20, 7);
        assert!(v.nav(&Key::ArrowRight));
        assert_eq!(v.cursor, 7);
        assert!(v.visible().contains(&7), "翻页后光标必须在视窗内");
        assert_eq!(v.footer().as_deref(), Some("2-8 of 20"));

        assert!(v.nav(&Key::ArrowLeft));
        assert_eq!(v.cursor, 0);

        // 上箭头从头绕到尾,视窗跟到底
        assert!(v.nav(&Key::ArrowUp));
        assert_eq!(v.cursor, 19);
        assert_eq!(v.visible(), 13..20);
        assert_eq!(v.footer().as_deref(), Some("14-20 of 20"));

        // 一页装得下:没有「第几到第几」这一行,←/→ 仍然认(跳到末行 / 回头)
        let mut all = Viewport::new(5, 5);
        assert_eq!(all.footer(), None);
        assert!(all.nav(&Key::ArrowRight));
        assert_eq!(all.cursor, 4);

        // 非移动键一律不认,由调用方去分辨 Enter / Esc
        assert!(!all.nav(&Key::Enter));
        assert!(!all.nav(&Key::Char('x')));
    }

    /// browse 键序列:空格切光标行、`a` 全勾/全清、`/` 交出过滤请求、
    /// Enter 按「有没有勾选」分流、Esc 取消。这是合体控件的全部新逻辑,
    /// 移动键已由 [`Viewport::nav`] 的测试守着。
    #[test]
    fn browse_键序列_勾选分流与过滤请求() {
        let mut checked = vec![false; 4];

        // 无勾选时 Enter = 打开光标行
        assert!(matches!(
            browse_key(&mut checked, 2, Key::Enter),
            BrowseAction::Done(BrowseEvent::Open(2))
        ));

        // 空格切光标行,Enter 变成对勾选集合动手(下标升序)
        assert!(matches!(
            browse_key(&mut checked, 1, Key::Char(' ')),
            BrowseAction::Redraw
        ));
        assert_eq!(checked, vec![false, true, false, false]);
        let _ = browse_key(&mut checked, 3, Key::Char(' '));
        match browse_key(&mut checked, 0, Key::Enter) {
            BrowseAction::Done(BrowseEvent::Act(sel)) => assert_eq!(sel, vec![1, 3]),
            _ => panic!("有勾选时 Enter 必须交出勾选集合"),
        }

        // `a`:有一行没勾就全勾,已勾满就全清
        let _ = browse_key(&mut checked, 0, Key::Char('a'));
        assert_eq!(checked, vec![true; 4], "有未勾的行时 a 应勾满");
        let _ = browse_key(&mut checked, 0, Key::Char('a'));
        assert_eq!(checked, vec![false; 4], "已勾满时 a 应清空");

        // `/` 与 Esc 原样交回;无关键不动
        assert!(matches!(
            browse_key(&mut checked, 0, Key::Char('/')),
            BrowseAction::Done(BrowseEvent::Filter)
        ));
        assert!(matches!(
            browse_key(&mut checked, 0, Key::Escape),
            BrowseAction::Done(BrowseEvent::Cancel)
        ));
        assert!(matches!(
            browse_key(&mut checked, 0, Key::Char('z')),
            BrowseAction::Ignore
        ));
    }
}
