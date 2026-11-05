//! 界面层：GPUI 实现，v0.2 起是唯一的 UI（egui 版已移除）。
//!
//! GPUI 的 Windows 后端是 Direct3D 11 + DirectComposition，实际要求 Win10
//! 及以上；v0.2 之前那套「DX12 → WARP → glow」的兜底链随 eframe 一起删掉了，
//! 换来的是更小的依赖树、更少的内存占用和一套真正跨平台的渲染路径
//! （macOS 走 Metal，Linux 走 Vulkan/blade）。Win7/8 停在 v0.1.2。
//!
//! 非 UI 层（model / store / provider / download / engine / util）与界面无关，
//! 这里只负责把同一份数据画出来并接住事件。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App as GpuiApp, AppContext, Application, Bounds, Context, Div, FontWeight,
    InteractiveElement, KeyDownEvent,
    IntoElement, ParentElement, Pixels, Point, Render, SharedString, Stateful, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, point, prelude::FluentBuilder, px,
    relative, rgb, size,
};

use crate::download::DlSpec;
use crate::engine::{Engine, UiMsg};
use crate::http::Http;
use crate::i18n::{self, t};
use crate::tf;
use crate::model::{Asset, AssetKind, Config, DownloadRecord, Release, Track};
use crate::store::Store;
use crate::util;

// ---------------------------------------------------------------------------
// 界面缩放
// ---------------------------------------------------------------------------

/// 界面缩放系数，进程级。用静态量而非字段的理由与 `i18n` 相同：界面只有一个
/// 窗口，而 `badge()` / `chip()` 这些自由函数没有 `self`。
///
/// 它**不是** DPI 系数 —— GPUI 已经按系统缩放把逻辑像素换算成物理像素了
/// （150% 时 1 逻辑像素 = 1.5 物理像素）。这里管的是另一件事：11 逻辑像素
/// 在 4K 屏上照样嫌小，用户得能自己把它调大。
static SCALE: AtomicU32 = AtomicU32::new(0x3F80_0000); // f32 1.0 的位模式

const SCALE_MIN: f32 = 0.8;
const SCALE_MAX: f32 = 1.8;
const SCALE_STEP: f32 = 0.1;

pub fn set_ui_scale(s: f32) {
    let clamped = if s.is_finite() {
        s.clamp(SCALE_MIN, SCALE_MAX)
    } else {
        1.0
    };
    SCALE.store(clamped.to_bits(), Ordering::Relaxed);
}

fn scale() -> f32 {
    f32::from_bits(SCALE.load(Ordering::Relaxed))
}

/// 设计值 → 实际逻辑像素。字号、行高、输入框高度、侧栏宽度都过这里。
fn sp(n: f32) -> Pixels {
    px(n * scale())
}

/// 下载任务状态。原先定义在 egui 的 `app.rs` 里（那时两套前端共用）；egui 版
/// 删掉后就留在界面层，`download` / `engine` 不关心它。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Canceled,
}

impl TaskStatus {
    fn label(self) -> &'static str {
        match self {
            TaskStatus::Queued => t("排队中", "queued"),
            TaskStatus::Running => t("下载中", "downloading"),
            TaskStatus::Paused => t("已暂停", "paused"),
            TaskStatus::Done => t("已完成", "done"),
            TaskStatus::Failed => t("失败", "failed"),
            TaskStatus::Canceled => t("已取消", "canceled"),
        }
    }
}

/// 一条下载任务。
#[derive(Clone)]
struct DlTask {
    spec: DlSpec,
    status: TaskStatus,
    downloaded: u64,
    total: u64,
    speed: f64,
    sha_ok: Option<bool>,
    error: Option<String>,
    path: Option<String>,
    started_at: i64,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
}

impl DlTask {
    fn new(spec: DlSpec) -> Self {
        Self {
            spec,
            status: TaskStatus::Queued,
            downloaded: 0,
            total: 0,
            speed: 0.0,
            sha_ok: None,
            error: None,
            path: None,
            started_at: util::now_secs(),
            cancel: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    fn frac(&self) -> f32 {
        if self.total == 0 {
            if self.status == TaskStatus::Done { 1.0 } else { 0.0 }
        } else {
            (self.downloaded as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }
}

/// 鼠标停在「正在下载」的侧栏条目上时浮出的详情。
///
/// 只记**打开那一刻鼠标的纵向位置**（横向固定贴侧栏右沿），之后鼠标在这条里移动
/// 不再重算 —— 悬停时每动一像素就改一次状态会让整窗重绘，而浮层本来就不该跟着
/// 手抖。
struct DlPop {
    /// 追踪项的 id，用来确认浮层对应的任务还在。
    track: String,
    y: f32,
}

/// 鼠标停在一个**已经下过东西**的侧栏条目上时浮出的「已下载版本」清单。
///
/// 与 [`DlPop`] 分成两个类型而不是加个 `mode` 字段：前者是只读的进度播报，
/// 后者每一行都能点（打开所在文件夹），生命周期也多一个「鼠标要能挪进来」的
/// 问题，混在一起只会让两边都变难读。
struct DonePop {
    track: String,
    y: f32,
}

/// 一个已经下完的版本，浮层里一行。
struct DoneVersion {
    version: String,
    dir: String,
    size: u64,
    at: i64,
}

/// 这条子任务是不是还「没结束」（排队、进行中、暂停都算）。
fn is_live(s: TaskStatus) -> bool {
    matches!(s, TaskStatus::Running | TaskStatus::Queued | TaskStatus::Paused)
}

/// 同一条追踪项下多条任务的**合计**进度与配色。
///
/// 一个条目可以同时下好几个文件（比如 Windows 和 Linux 版一起下）。侧栏那一行
/// 代表的是「这一项」，不是「最后点的那一个」，所以进度要按总字节数合起来算——
/// 否则列表里的条会随最后一条任务跳动，看着像退回去了。
fn aggregate(tasks: &[&DlTask]) -> (f32, u32) {
    let total: u64 = tasks.iter().map(|t| t.total).sum();
    let done: u64 = tasks.iter().map(|t| t.downloaded).sum();
    let frac = if total == 0 {
        0.
    } else {
        (done as f64 / total as f64).clamp(0., 1.) as f32
    };
    // 有一条在跑就是「进行中」的主色；全暂停给橙色；只剩排队则是灰的。
    let color = if tasks.iter().any(|t| t.status == TaskStatus::Running) {
        ACCENT
    } else if tasks.iter().any(|t| t.status == TaskStatus::Paused) {
        ORANGE
    } else {
        MUTED
    };
    (frac, color)
}

fn status_color(s: TaskStatus) -> u32 {
    match s {
        TaskStatus::Running => ACCENT,
        TaskStatus::Done => GREEN,
        TaskStatus::Failed => RED,
        TaskStatus::Paused => ORANGE,
        TaskStatus::Canceled => FAINT,
        TaskStatus::Queued => MUTED,
    }
}

/// 写进 `history.json` 的状态。**存中文常量，不存界面语言**：历史文件是跨语言
/// 共用的，存了 "done" 之后再切回中文就没法还原了。显示时由 `status_text` 翻。
#[derive(Clone, Copy)]
enum RecordStatus {
    Done,
    Failed,
    Canceled,
}

impl RecordStatus {
    fn stored(self) -> &'static str {
        match self {
            RecordStatus::Done => "完成",
            RecordStatus::Failed => "失败",
            RecordStatus::Canceled => "已取消",
        }
    }
}

/// 历史行上的状态文字：库里的值转成当前语言。
fn status_text(stored: &str) -> String {
    match stored {
        "完成" => t("完成", "done").to_string(),
        "失败" => t("失败", "failed").to_string(),
        "已取消" => t("已取消", "canceled").to_string(),
        // 认不出来的取值原样显示，总比显示空白好查。
        other => other.to_string(),
    }
}

/// 侧栏分组标题与详情页的分组徽章。
///
/// 分组名是数据里的字符串（`@scene#54:"日常开发"` 可以自定义），所以只翻内置的
/// 那几个；认不出来的原样显示 —— 用户自己起的名字不该被翻译。
fn group_label(group: &str) -> String {
    match group {
        "GitHub 项目" => t("GitHub 项目", "GitHub").to_string(),
        "软件站" => t("软件站", "Download sites").to_string(),
        "系统镜像" => t("系统镜像", "OS images").to_string(),
        "日常开发" => t("日常开发", "Dev tools").to_string(),
        other => other.to_string(),
    }
}

/// 带计数的短语：英文分单复数，中/日/韩不分 —— 它们的量词跟在数字后面，
/// 没有「1 个 / 2 个」以外的变化。
///
/// 实参是「数字后面那一截」，空格算在里面：`count_text(3, " 个追踪项", " item",
/// " items", " 件", "개")`。
fn count_text(
    n: usize,
    zh: &str,
    en_one: &str,
    en_many: &str,
    ja: &str,
    ko: &str,
) -> String {
    match i18n::current() {
        i18n::Lang::Zh => format!("{n}{zh}"),
        i18n::Lang::En => format!("{n}{}", if n == 1 { en_one } else { en_many }),
        i18n::Lang::Ja => format!("{n}{ja}"),
        i18n::Lang::Ko => format!("{n}{ko}"),
    }
}

// ---------------------------------------------------------------------------
// 调色板：沿用 v0.1 的配色（浅灰底 + 白卡片 + 蓝色强调），老用户不会觉得换了软件。
// ---------------------------------------------------------------------------

const BG: u32 = 0xF4F6F9;
const CARD: u32 = 0xFFFFFF;
const SIDEBAR: u32 = 0xFAFBFD;
const BORDER: u32 = 0xE2E6ED;
const TEXT: u32 = 0x1B2027;
const MUTED: u32 = 0x6B7485;
const FAINT: u32 = 0x9AA3B2;
const ACCENT: u32 = 0x2563EB;
const ACCENT_SOFT: u32 = 0xE8F0FE;
const GREEN: u32 = 0x15803D;
const GREEN_SOFT: u32 = 0xE7F6EC;
const ORANGE: u32 = 0xB45309;
const ORANGE_SOFT: u32 = 0xFDF1E3;
const RED: u32 = 0xB91C1C;
const RED_SOFT: u32 = 0xFDECEC;
const HOVER: u32 = 0xEEF1F6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Overview,
    Notes,
    History,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Overview => t("概览", "Overview"),
            Tab::Notes => t("更新说明", "What's new"),
            Tab::History => t("历史版本", "Versions"),
        }
    }
}

// ---------------------------------------------------------------------------
// 快捷键
// ---------------------------------------------------------------------------

gpui::actions!(
    app_monitor,
    [
        AddTrack,
        OpenSettings,
        OpenHistory,
        TabOverview,
        TabNotes,
        TabVersions,
        FocusFilter
    ]
);

// ---------------------------------------------------------------------------
// 文本输入
// ---------------------------------------------------------------------------

/// GPUI 0.2 没有现成的文本输入控件，而「添加追踪」和「设置」两个弹窗都需要输入。
/// 这里实现够用的最小版本：可打印字符、退格/删除、左右/Home/End、Ctrl+V 粘贴、
/// Ctrl+A 全选（再输入即替换）。
///
/// 不接 `EntityInputHandler` 的代价是中文输入法用不了；但这两处要填的是 URL、
/// 目录、Token 和代理地址，全是 ASCII，粘进来也不受键盘布局影响。追踪项的名称
/// 由 URL 推导（`parse_input` 自己会起名），不需要手工键入中文。
/// 纯字符串编辑状态。与窗口无关，因此可以直接写单测覆盖光标与 UTF-8 边界。
#[derive(Default, Clone)]
struct EditState {
    text: String,
    /// 光标位置：字节偏移，始终落在 char 边界上。
    cursor: usize,
    /// 选区，**已排序**（`a <= b`）：下一次输入会整体替换、退格整体删掉。
    selection: Option<(usize, usize)>,
    /// 拖选时固定不动的另一端。有了它，鼠标抖动或往回拖都能正确收窄选区 ——
    /// 只记 `(起点, 当前)` 两个数在下一次移动时就没法区分谁是谁了。
    anchor: Option<usize>,
}

impl EditState {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            selection: None,
            anchor: None,
        }
    }

    fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.selection = None;
        self.anchor = None;
    }

    fn split(&self) -> (&str, &str) {
        let cursor = self.cursor.min(self.text.len());
        match self.text.get(..cursor) {
            Some(before) => (before, &self.text[cursor..]),
            None => (&self.text, ""),
        }
    }

    fn insert(&mut self, s: &str, multiline: bool) {
        self.anchor = None;
        if let Some((a, b)) = self.selection.take() {
            let (a, b) = (a.min(b), a.max(b));
            self.text.replace_range(a..b, "");
            self.cursor = a;
        }
        // 先归一 CRLF：直接按字符替换会把 Windows 换行变成两个空格。
        let s = if multiline {
            s.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            s.replace("\r\n", " ").replace(['\r', '\n'], " ")
        };
        let cursor = self.cursor.min(self.text.len());
        self.text.insert_str(cursor, &s);
        self.cursor = cursor + s.len();
    }

    fn backspace(&mut self) {
        // 有选区时退格 = 删掉选区，而不是像早先那样把整段清空：那时选区只可能
        // 来自 Ctrl+A，整段清空恰好等价；现在界面上能拖选出一段，就不等价了。
        if self.cut_selection() {
            return;
        }
        self.anchor = None;
        if self.cursor == 0 {
            return;
        }
        let mut i = self.cursor - 1;
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        self.text.replace_range(i..self.cursor, "");
        self.cursor = i;
    }

    fn delete(&mut self) {
        if self.cut_selection() {
            return;
        }
        self.anchor = None;
        if self.cursor >= self.text.len() {
            return;
        }
        let mut j = self.cursor + 1;
        while j < self.text.len() && !self.text.is_char_boundary(j) {
            j += 1;
        }
        self.text.replace_range(self.cursor..j, "");
    }

    /// 光标左边第一个字符的起始偏移。
    fn prev_index(&self) -> usize {
        if self.cursor == 0 {
            return 0;
        }
        let mut i = self.cursor - 1;
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    /// 光标右边第一个字符的结束偏移。
    fn next_index(&self) -> usize {
        if self.cursor >= self.text.len() {
            return self.text.len();
        }
        let mut j = self.cursor + 1;
        while j < self.text.len() && !self.text.is_char_boundary(j) {
            j += 1;
        }
        j
    }

    // 只在单测里直接调用这几个：界面上方向键走 `to_edge`（要认 Shift），
    // 留着它们是给测试读起来更像「光标挪一格」。
    #[cfg(test)]
    fn move_left(&mut self) {
        self.move_caret_to(self.prev_index(), false);
    }

    #[cfg(test)]
    fn move_right(&mut self) {
        self.move_caret_to(self.next_index(), false);
    }

    #[cfg(test)]
    fn home(&mut self) {
        self.move_caret_to(0, false);
    }

    #[cfg(test)]
    fn end(&mut self) {
        self.move_caret_to(self.text.len(), false);
    }

    /// 光标挪到 `ix`；`keep` 为真表示按住 Shift —— 从原来那端起一段选区。
    fn move_caret_to(&mut self, ix: usize, keep: bool) {
        let ix = crate::select_text::snap_to_char(&self.text, ix);
        if keep {
            self.select_to(ix);
        } else {
            self.cursor = ix;
            self.selection = None;
            self.anchor = None;
        }
    }

    fn select_all(&mut self) {
        self.selection = Some((0, self.text.len()));
        self.anchor = None;
    }

    /// 鼠标按下：光标落到落点上，从这儿开始拉选区（此时还没有选区）。
    fn start_select(&mut self, ix: usize) {
        let ix = crate::select_text::snap_to_char(&self.text, ix);
        self.cursor = ix;
        self.anchor = Some(ix);
        self.selection = None;
    }

    /// 拖到 `ix`：锚点不动，光标跟着走，选区就是这两点之间。
    fn extend_to(&mut self, ix: usize) {
        self.select_to(ix);
    }

    /// Shift+方向键：锚点若还没有，就从当前光标起。
    fn select_to(&mut self, ix: usize) {
        let ix = crate::select_text::snap_to_char(&self.text, ix);
        let anchor = self.anchor.unwrap_or(self.cursor);
        self.anchor = Some(anchor);
        self.cursor = ix;
        self.selection = if anchor == ix {
            None
        } else {
            Some((anchor.min(ix), anchor.max(ix)))
        };
    }

    fn end_select(&mut self) {
        self.anchor = None;
    }

    fn is_dragging(&self) -> bool {
        self.anchor.is_some()
    }

    /// 选区里的文字与字数。空选区返回 `None` —— 复制、剪切靠它判断能不能用。
    ///
    /// 两端都过一遍 `snap_to_char`：日韩文字是多字节的，切在中间会 panic。
    fn selected(&self) -> Option<(String, usize)> {
        let (a, b) = self.selection?;
        let a = crate::select_text::snap_to_char(&self.text, a.min(b));
        let b = crate::select_text::snap_to_char(&self.text, a.max(b));
        if a >= b {
            return None;
        }
        let text = self.text.get(a..b)?.to_string();
        let n = text.chars().count();
        Some((text, n))
    }

    /// 删掉选区并把光标放回起点。
    fn cut_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection.take() else {
            return false;
        };
        let a = crate::select_text::snap_to_char(&self.text, a.min(b));
        let b = crate::select_text::snap_to_char(&self.text, a.max(b));
        if a >= b {
            return false;
        }
        self.text.replace_range(a..b, "");
        self.cursor = a;
        true
    }
}

struct TextField {
    edit: EditState,
    focus: gpui::FocusHandle,
    /// Token 这类内容渲染成圆点。
    secret: bool,
    /// 允许粘贴带换行的文本（「添加追踪」要一次贴多行）。
    multiline: bool,
}

impl TextField {
    fn new(focus: gpui::FocusHandle, text: impl Into<String>) -> Self {
        Self {
            edit: EditState::new(text),
            focus,
            secret: false,
            multiline: false,
        }
    }

    fn text(&self) -> &str {
        &self.edit.text
    }

    fn set(&mut self, text: impl Into<String>) {
        self.edit.set(text);
    }

    fn clear(&mut self) {
        self.edit.set(String::new());
    }

    fn split(&self) -> (&str, &str) {
        self.edit.split()
    }

    fn insert(&mut self, s: &str) {
        self.edit.insert(s, self.multiline);
    }

    fn backspace(&mut self) {
        self.edit.backspace();
    }

    fn delete(&mut self) {
        self.edit.delete();
    }

    fn select_all(&mut self) {
        self.edit.select_all();
    }

    fn start_select(&mut self, ix: usize) {
        self.edit.start_select(ix);
    }

    fn extend_to(&mut self, ix: usize) {
        self.edit.extend_to(ix);
    }

    fn end_select(&mut self) {
        self.edit.end_select();
    }

    fn is_dragging(&self) -> bool {
        self.edit.is_dragging()
    }

    /// 光标 / 选区（字节）换算成第几个字符 —— Token 这类框画的是圆点，一个点
    /// 对应一个字，字节数对不上。
    fn char_to_byte(&self, chars: usize) -> usize {
        self.text()
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.text().len())
    }

    fn selected(&self) -> Option<(String, usize)> {
        self.edit.selected()
    }

    fn cut_selection(&mut self) -> bool {
        self.edit.cut_selection()
    }
}

/// 复制按钮的回执文案：「已复制 **什么**」里的那个什么。
///
/// 不用 `&str` 传，是因为中英语序不同 —— 调用处只写中文的话，英文界面就会
/// 冒出「Copied 版本号」这种半截翻译。
#[derive(Clone)]
enum What {
    Version,
    PageLink,
    Notes,
    AssetLink(String),
    /// 拖选下来的片段，带字数 —— 回执里说清「复制了多少字」比只说「已复制」有用。
    Selection(usize),
}

impl What {
    /// 「被复制的东西」在当前语言下叫什么。
    ///
    /// 只给标签、不给整个句子：句子有「已复制 X」「没有可复制的 X」两种，
    /// 而 X 本身又带名字或字数，把两侧都塞进同一个字符串只会变成拼贴。
    fn label(&self) -> String {
        match self {
            What::Version => t("版本号", "version number").to_string(),
            What::PageLink => t("来源页链接", "source page link").to_string(),
            What::Notes => t("更新说明", "release notes").to_string(),
            What::AssetLink(name) => tf!("「{}」的链接", "link to \"{}\"", name),
            What::Selection(n) => {
                tf!("选中的 {} 个字", "selected {} character(s)", n)
            }
        }
    }
}

/// 一段正文里的选区。
///
/// `owner` 区分不同的文本块：概览摘要、更新说明各有一份，切换条目或页签后
/// 旧的偏移不能再套到新文本上。`text` 存整段原文，用来把字节偏移切成字符串。
#[derive(Clone)]
struct Selection {
    owner: String,
    anchor: usize,
    head: usize,
    text: String,
}

/// 选区上右键弹出的菜单。
///
/// 位置存的是**窗口坐标下的逻辑像素**（`MouseDownEvent::position` 就是这个口径），
/// 与根节点做绝对定位的参照系一致，所以不必再减一遍 bounds。
struct CtxMenu {
    x: f32,
    y: f32,
    owner: String,
    text: String,
}

/// 文本输入框上右键弹出的菜单。
///
/// 与 [`CtxMenu`] 分开是因为两者能做的事不一样：那个菜单作用于**只读正文**的
/// 选区（复制 / 全选 / 取消选择），这个要改输入框的内容（剪切 / 粘贴），而且
/// 得记住作用于哪一个框。坐标口径与 `CtxMenu` 相同：窗口坐标下的逻辑像素。
struct FieldCtx {
    field: FieldId,
    x: f32,
    y: f32,
}

/// 输入框右键菜单里的四个动作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldAct {
    Cut,
    Copy,
    Paste,
    All,
}

/// 语言下拉菜单是被哪一颗按钮拉起的：顶栏一颗、设置弹窗里一颗。
///
/// 菜单只记「哪一个」，坐标每帧从对应的 cell 现读 —— 窗口缩放或设置内容滚动
/// 之后按钮会挪位置，存快照会让菜单悬在半空。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LangAnchor {
    Top,
    Set,
}

/// 选区的字节范围 → 可以放进剪贴板的字符串与字数。
///
/// 两端都要 snap：鼠标落在字形中间时 `index_at` 给出的是最近的字节索引，
/// 直接切 `&str[a..b]` 有可能切在 UTF-8 中间而 panic。首尾空白一并去掉，
/// 用户拖选时几乎不可能正好停在字上。
fn picked(sel: &Selection) -> Option<(String, usize)> {
    let a = crate::select_text::snap_to_char(&sel.text, sel.anchor.min(sel.head));
    let b = crate::select_text::snap_to_char(&sel.text, sel.anchor.max(sel.head));
    if a >= b {
        return None;
    }
    let text = sel.text.get(a..b)?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let n = text.chars().count();
    Some((text, n))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldId {
    AddInput,
    Filter,
    DownloadDir,
    Proxy,
    Token,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialog {
    Add,
    Settings,
    History,
}

/// `--select <名称> --cand <n>`：启动时直接定位到某个条目 / 候选页签。
/// 用 `OnceLock` 而不是构造参数，是为了让 `Application::run` 的闭包保持简单。
static START_HINT: std::sync::OnceLock<(Option<String>, usize)> = std::sync::OnceLock::new();

pub fn set_start_hint(name: Option<String>, candidate: usize) {
    let _ = START_HINT.set((name, candidate));
}

pub struct App {
    tracks: Vec<Track>,
    history: Vec<DownloadRecord>,
    cfg: Config,
    selected: Option<String>,
    tab: Tab,
    candidate_sel: HashMap<String, usize>,
    /// 数据文件损坏时的提示（store 层负责隔离与备份，这里只负责讲清楚）。
    issues: Vec<crate::store::LoadIssue>,
    toast: Option<(String, Instant)>,
    engine: Engine,
    http: Arc<Http>,
    tasks: Vec<DlTask>,
    resolving: std::collections::HashSet<String>,

    // ---- 弹窗与输入 ----
    dialog: Option<Dialog>,
    add_input: TextField,
    /// 侧栏筛选框。
    filter: TextField,
    set_dir: TextField,
    set_proxy: TextField,
    set_token: TextField,
    /// 当前获得键盘焦点的输入框，只用于画高亮边框。
    active_field: Option<FieldId>,
    /// 待聚焦的输入框。事件回调里拿不到 `&mut Window`，只能等下一帧在
    /// `render` 里真正把焦点交出去。
    pending_focus: Option<FieldId>,
    /// 下一帧把焦点交还给根节点。只有关弹窗时才置位 —— 弹窗里的输入框不再绘制，
    /// 焦点悬空会让快捷键全部失灵。
    refocus_root: bool,
    /// 根节点的焦点句柄。GPUI 的键盘派发要靠「当前有焦点的节点」铺 dispatch
    /// 路径，一个可聚焦元素都没有时按快捷键会 panic（active_node 为空）。
    root_focus: gpui::FocusHandle,
    /// 上一次「添加追踪」时无法识别的行，留在弹窗里给用户看。
    add_skipped: Vec<String>,
    /// 删除确认：为 Some 时该条目显示「确认删除」。
    confirm_delete: Option<String>,

    /// 自动检查的计时起点。
    last_auto: Instant,
    /// 窗口标题上一次是按哪种语言设的。切换语言时要在 `render` 里重设一次
    /// （`&mut Window` 只在绘制阶段拿得到），而每帧都设一遍会白打扰系统。
    title_lang: Option<i18n::Lang>,
    /// 概览正文的选区。GPUI 0.2.2 没有文本选中，这是自己实现的那一份。
    sel: Option<Selection>,
    /// 最近一帧画出来的正文 bounds。鼠标事件挂在 div 上，拿不到元素自己的位置，
    /// 只能靠绘制阶段回填一次。
    sel_geom: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// 底部下载面板是否展开。默认收起：下载中的条目在左侧列表里自带一条进度条，
    /// 想看成败明细、暂停/取消/打开这些操作时才从顶栏点开。
    queue_show: bool,
    /// 悬停浮出的下载详情，见 [`DlPop`]。
    dl_pop: Option<DlPop>,
    /// 语言下拉菜单。打开时记下**是哪一个按钮**拉起的，菜单每帧去读那颗按钮的
    /// bounds 再定位。`None` 表示收起。
    lang_menu: Option<LangAnchor>,
    /// 顶栏与设置弹窗里那两颗语言按钮各自的 bounds，绘制阶段回填。
    ///
    /// 两处分开记而不是共用一个：设置弹窗关掉之后弹窗里那颗不再绘制，共用的话
    /// 下一次点顶栏按钮会读到弹窗里那颗的旧坐标，菜单就飘到屏幕中间去了。
    lang_anchor_top: Rc<RefCell<Option<Bounds<Pixels>>>>,
    lang_anchor_set: Rc<RefCell<Option<Bounds<Pixels>>>>,
    /// 选区上右键弹出的菜单。松手即复制之外再给一条显式路径：用户习惯先选中、
    /// 再右键点「复制」，缺了它就会被当成「这个界面不能复制」。
    ctx: Option<CtxMenu>,
    /// 输入框上右键弹出的菜单（剪切 / 复制 / 粘贴 / 全选）。`None` 表示收起。
    field_ctx: Option<FieldCtx>,
    /// 每个输入框绘制时的外框（窗口坐标）。鼠标落下时靠它把位置换算成字节偏移 ——
    /// GPUI 的事件回调里拿不到元素自己的 bounds，只能在绘制阶段回填一次。
    field_geom: [Rc<RefCell<Option<Bounds<Pixels>>>>; 5],
    /// 正在拖选的输入框。拖出框外（甚至拖到别的一行上）也要继续选，所以从按下
    /// 到松手之间一直记着是哪一个。
    field_drag: Option<FieldId>,
    /// 悬停浮出的「已下载版本」，见 [`DonePop`]。
    done_pop: Option<DonePop>,
    /// 浮层该收起的时刻。鼠标从条目挪到浮层要跨过几像素的缝，离开即收的话
    /// 用户永远点不到里面的行，所以给一小段宽限。
    done_pop_hide_at: Option<Instant>,
}

impl App {
    fn new(cx: &mut Context<Self>) -> Self {
        let store = Store::load();
        // 语言与缩放是全局的（见 i18n / SCALE 的注释），必须赶在第一次绘制之前生效。
        // 环境变量优先，方便截图与排障时临时切换，不去动 config.json。
        let lang = std::env::var("AM_LANG")
            .map(|v| i18n::Lang::from_code(&v))
            // 配置里没写过语言（空串）就跟随系统，见 i18n::Lang::resolve。
            .unwrap_or_else(|_| i18n::Lang::resolve(&store.config.lang));
        i18n::set(lang);
        set_ui_scale(
            std::env::var("AM_UI_SCALE")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(store.config.ui_scale),
        );
        if store.seeded {
            crate::store::write_tracks(&store.tracks);
        }
        let (hint_name, hint_cand) = START_HINT.get().cloned().unwrap_or((None, 0));
        let selected = hint_name
            .as_deref()
            .and_then(|n| {
                let needle = n.to_lowercase();
                store
                    .tracks
                    .iter()
                    .find(|t| t.name.to_lowercase().contains(&needle))
                    .map(|t| t.id.clone())
            })
            .or_else(|| store.tracks.first().map(|t| t.id.clone()));
        let mut candidate_sel = HashMap::new();
        if hint_cand > 0 {
            if let Some(id) = &selected {
                candidate_sel.insert(id.clone(), hint_cand);
            }
        }
        let (http, http_err) = make_http(&store.config);
        let set_dir = TextField::new(cx.focus_handle(), store.config.download_dir.clone());
        let set_proxy = TextField::new(cx.focus_handle(), store.config.proxy.clone());
        let mut set_token = TextField::new(cx.focus_handle(), store.config.github_token.clone());
        set_token.secret = true;
        let mut add_input = TextField::new(cx.focus_handle(), "");
        add_input.multiline = true;

        let auto = store.config.auto_check_on_start;
        let mut app = Self {
            tracks: store.tracks,
            history: store.history,
            cfg: store.config,
            selected,
            tab: Tab::Overview,
            candidate_sel,
            issues: store.issues,
            toast: http_err.map(|e| {
                (
                    crate::i18n::t1(
                        "代理设置无效，本次改用直连：{}",
                        "Proxy settings are invalid, using a direct connection: {}",
                        e,
                    ),
                    Instant::now(),
                )
            }),
            engine: Engine::new(),
            http,
            tasks: Vec::new(),
            resolving: std::collections::HashSet::new(),
            dialog: None,
            add_input,
            filter: TextField::new(cx.focus_handle(), ""),
            set_dir,
            set_proxy,
            set_token,
            active_field: None,
            pending_focus: None,
            refocus_root: false,
            root_focus: cx.focus_handle(),
            add_skipped: Vec::new(),
            confirm_delete: None,
            last_auto: Instant::now(),
            title_lang: None,
            sel: None,
            sel_geom: Rc::new(RefCell::new(None)),
            ctx: None,
            field_ctx: None,
            field_geom: std::array::from_fn(|_| Rc::new(RefCell::new(None))),
            field_drag: None,
            done_pop: None,
            done_pop_hide_at: None,
            queue_show: false,
            dl_pop: None,
            lang_menu: None,
            lang_anchor_top: Rc::new(RefCell::new(None)),
            lang_anchor_set: Rc::new(RefCell::new(None)),
        };
        app.spawn_pump(cx);
        if auto {
            app.resolve_all(cx);
        }
        // 排障钩子（与 `--download` 同理）：GPUI 版的界面不好在无头环境里点按钮，
        // 这个开关让它在启动后自己下最小的一个直链，用来验证队列与进度渲染。
        // 截图/复现用：启动就直接打开某个弹窗（界面上点按钮在无头环境里不可靠）。
        if let Ok(which) = std::env::var("AM_START_DIALOG") {
            match which.as_str() {
                "add" => app.open_dialog(Dialog::Add, cx),
                "settings" => app.open_dialog(Dialog::Settings, cx),
                "history" => app.open_dialog(Dialog::History, cx),
                _ => {}
            }
        }
        if std::env::var("AM_GPUI_DEMO_DL").is_ok() {
            let pick = app.tracks.iter().find_map(|t| {
                let r = t.release.as_ref()?;
                let a = r
                    .assets
                    .iter()
                    .filter(|a| a.kind == AssetKind::Direct)
                    .min_by_key(|a| a.size.unwrap_or(u64::MAX))?;
                Some((t.id.clone(), r.version.clone(), a.clone(), r.page_url.clone()))
            });
            if let Some((id, version, asset, page)) = pick {
                app.start_asset(&id, &version, &asset, &page, cx);
            }
        }
        // 排障钩子（同 `AM_GPUI_DEMO_DL`）：塞一条**假的**进行中任务，用来在
        // 不真的下一遍文件的前提下看侧栏进度条和悬停详情的排版。只活在内存里，
        // 不发请求、不写历史。
        if let Ok(v) = std::env::var("AM_GPUI_FAKE_DL") {
            // 值写成 2 就塞两条（同一个条目下 Windows + Linux 一起下的场面）。
            let n = v.trim().parse::<usize>().unwrap_or(1).clamp(1, 3);
            app.fake_tasks(n);
        }
        app
    }

    /// 造 `n` 条进行中的假任务，见 `new` 里的 `AM_GPUI_FAKE_DL`。
    fn fake_tasks(&mut self, n: usize) {
        let Some(track) = self.tracks.first() else {
            return;
        };
        // 文件名和版本跟真实数据对齐，截图才不会被「deepxcode-1.2.3」这种编出来的
        // 名字暴露；没有解析结果时退回到一个通用名字。
        let release = track.release.clone();
        let version = release
            .as_ref()
            .map(|r| r.version.clone())
            .unwrap_or_else(|| "v1.0.0".to_string());
        let direct: Vec<String> = release
            .as_ref()
            .map(|r| {
                r.assets
                    .iter()
                    .filter(|a| a.kind == AssetKind::Direct)
                    .map(|a| a.name.clone())
                    .collect()
            })
            .unwrap_or_default();
        // 进度取三个不一样的值，一眼能看出这是两条独立的下载。
        for i in 0..n {
            let file = direct
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("{}-{}-{}.bin", track.name, version, i + 1));
            let spec = DlSpec {
                id: format!("fake-dl-{i}"),
                track_id: track.id.clone(),
                app: track.name.clone(),
                version: version.clone(),
                file,
                url: String::new(),
                dir: std::path::PathBuf::new(),
                sha256: None,
                referer: None,
            };
            let total = 48_000_000u64 + i as u64 * 12_000_000;
            let frac = [0.42, 0.18, 0.66][i % 3];
            let mut task = DlTask::new(spec);
            task.status = TaskStatus::Running;
            task.total = total;
            task.downloaded = (total as f64 * frac) as u64;
            task.speed = 2.4 * 1024.0 * 1024.0 - i as f64 * 600.0 * 1024.0;
            self.tasks.insert(i, task);
        }
    }

    /// 后台消息泵：engine 用 std::sync::mpsc 回传解析/下载结果，这里每 150ms
    /// 抽一次队列，只有真的收到消息才重绘。
    fn spawn_pump(&mut self, cx: &mut Context<Self>) {
        cx.spawn(|this: gpui::WeakEntity<App>, cx: &mut gpui::AsyncApp| {
            // `AsyncApp` 必须按值搬进 future：直接捕获 `&mut AsyncApp` 会要求
            // 外层引用活过整个循环，编译器不接受。
            let mut cx = cx.clone();
            async move {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(150))
                        .await;
                    if this
                        .update(&mut cx, |this, cx| {
                            this.drain(cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
        .detach();
    }

    fn drain(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(msg) = self.engine.rx.try_recv() {
            match msg {
                UiMsg::Resolved(id, release) => {
                    self.resolving.remove(&id);
                    if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                        // 第一次解析到的版本记为「已看过」，否则启动后满屏"有更新"。
                        if t.seen_version.is_none() {
                            t.seen_version = Some(release.version.clone());
                        }
                        t.last_checked = Some(util::now_secs());
                        t.last_error = None;
                        t.release = Some(*release);
                    }
                    changed = true;
                }
                UiMsg::ResolveFailed(id, err) => {
                    self.resolving.remove(&id);
                    if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                        t.last_checked = Some(util::now_secs());
                        t.last_error = Some(err);
                    }
                    changed = true;
                }
                UiMsg::DlProgress { id, p } => {
                    if let Some(task) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        task.status = TaskStatus::Running;
                        task.downloaded = p.downloaded;
                        task.total = p.total;
                        task.speed = p.speed;
                    }
                    changed = true;
                }
                UiMsg::DlDone {
                    id,
                    path,
                    sha_ok,
                    downloaded,
                    ..
                } => {
                    let mut snap = None;
                    if let Some(task) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        task.status = TaskStatus::Done;
                        task.sha_ok = sha_ok;
                        task.downloaded = downloaded;
                        if task.total == 0 {
                            task.total = downloaded;
                        }
                        task.speed = 0.0;
                        task.error = None;
                        task.path = Some(path.clone());
                        snap = Some(task.clone());
                    }
                    if let Some(task) = snap {
                        let name = task.spec.file.clone();
                        self.toast = Some((
                            match sha_ok {
                                Some(true) => crate::i18n::t1(
                                    "{} 下载完成，SHA-256 校验通过",
                                    "{} downloaded — SHA-256 verified",
                                    &name,
                                ),
                                Some(false) => crate::i18n::t1(
                                    "{} 下载完成，但 SHA-256 不一致",
                                    "{} downloaded, but the SHA-256 does not match",
                                    &name,
                                ),
                                None => crate::i18n::t1(
                                    "{} 下载完成（无官方摘要）",
                                    "{} downloaded (no official checksum)",
                                    &name,
                                ),
                            },
                            Instant::now(),
                        ));
                        self.record_history(&task, RecordStatus::Done, None);
                    }
                    changed = true;
                }
                UiMsg::DlFailed {
                    id,
                    error,
                    downloaded,
                    total,
                } => {
                    let mut snap = None;
                    if let Some(task) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        task.status = TaskStatus::Failed;
                        task.error = Some(error.clone());
                        task.downloaded = downloaded;
                        task.total = total;
                        task.speed = 0.0;
                        snap = Some(task.clone());
                    }
                    if let Some(task) = snap {
                        self.record_history(&task, RecordStatus::Failed, Some(error));
                    }
                    changed = true;
                }
                UiMsg::DlCanceled {
                    id,
                    paused,
                    downloaded,
                    total,
                } => {
                    let mut snap = None;
                    if let Some(task) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        task.status = if paused {
                            TaskStatus::Paused
                        } else {
                            TaskStatus::Canceled
                        };
                        task.downloaded = downloaded;
                        task.total = total;
                        task.speed = 0.0;
                        // 暂停只是临时状态，用户多半会继续下，不写进历史。
                        if !paused {
                            snap = Some(task.clone());
                        }
                    }
                    if let Some(task) = snap {
                        self.record_history(&task, RecordStatus::Canceled, None);
                    }
                    changed = true;
                }
            }
        }
        // 悬停浮层的宽限到期就收起来。借用这个 150ms 的泵而不是为浮层单开一个
        // 定时器：浮层只在鼠标悬停时存在，多开一个后台任务不值当。
        if let Some(at) = self.done_pop_hide_at {
            if Instant::now() >= at {
                self.done_pop_hide_at = None;
                if self.done_pop.take().is_some() {
                    changed = true;
                }
            }
        }
        if changed {
            crate::store::write_tracks(&self.tracks);
            // 一个任务结束后空出了并发位，后面的排队项该动起来了。
            self.pump_queue(cx);
            cx.notify();
        }
        self.tick_auto(cx);
    }

    fn resolve_all(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<Track> = self
            .tracks
            .iter()
            .filter(|t| !self.resolving.contains(&t.id))
            .cloned()
            .collect();
        if pending.is_empty() {
            self.toast(t("全部条目都在检查中", "Every tracked item is already being checked"), cx);
            return;
        }
        for t in pending {
            self.resolving.insert(t.id.clone());
            self.engine.start_resolve(self.http.clone(), t);
        }
        self.toast(t("已开始检查全部追踪项", "Checking every tracked item"), cx);
    }

    /// 把一项标为已读：把 `seen_version` 推到当前版本，橙色的「新版本」提示
    /// 随之消失。**不动** `release` 本身 —— 详情页照旧显示这一版，只是不再
    /// 催着用户看。用户很可能只是想清掉提醒，不代表他不关心这个版本。
    fn mark_read(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) else {
            return;
        };
        if t.mark_seen() {
            crate::store::write_tracks(&self.tracks);
            cx.notify();
        }
    }

    /// 一次清掉所有提醒。返回清掉的条数，0 表示本来就没有。
    fn mark_all_read(&mut self, cx: &mut Context<Self>) -> usize {
        let mut n = 0usize;
        for t in self.tracks.iter_mut() {
            // 先判 has_update 再 mark_seen：后者本身是幂等的，但多遍历一次
            // 无更新的条目也没意义。
            if t.has_update() && t.mark_seen() {
                n += 1;
            }
        }
        if n > 0 {
            crate::store::write_tracks(&self.tracks);
            cx.notify();
        }
        n
    }

    fn resolve_one(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.resolving.contains(id) {
            return;
        }
        let Some(track) = self.tracks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        self.resolving.insert(id.to_string());
        self.engine.start_resolve(self.http.clone(), track);
        cx.notify();
    }

    /// 沿用 v0.1 的规则：非直链只交给浏览器，直链落到
    /// `<下载目录>/<软件名>/<版本>/`，并检查是否已在队列里。
    fn start_asset(
        &mut self,
        track_id: &str,
        version: &str,
        asset: &Asset,
        page_url: &str,
        cx: &mut Context<Self>,
    ) {
        let Some((app_name, track_url)) = self
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| (t.name.clone(), t.url.clone()))
        else {
            return;
        };
        // 不再自动展开底部面板：进度改在左侧列表那条上显示，面板由顶栏的
        // 「下载队列」自己开关。这里只是把上一次的悬停浮层收掉，免得它留在
        // 上一个任务的位置上。
        self.dl_pop = None;
        if asset.kind != AssetKind::Direct {
            let is_http = asset.url.starts_with("http://") || asset.url.starts_with("https://");
            if is_http {
                let _ = open::that_detached(&asset.url);
                self.toast(
                    if asset.kind == AssetKind::Pan {
                        t("已在浏览器中打开网盘链接", "Opened the net-disk link in your browser")
                    } else {
                        t("已在浏览器中打开页面", "Opened the page in your browser")
                    },
                    cx,
                );
            } else {
                self.toast(
                    t(
                        "该链接为磁力/电驴协议，请用「复制」后粘贴到下载工具",
                        "That is a magnet/ed2k link — use Copy and paste it into a download tool",
                    ),
                    cx,
                );
            }
            return;
        }
        let version = if version.trim().is_empty() {
            t("未标注版本", "untagged").to_string()
        } else {
            version.trim().to_string()
        };
        let dir = PathBuf::from(self.cfg.download_dir.trim())
            .join(crate::download::sanitize(&app_name))
            .join(crate::download::sanitize(&version));
        let page = if page_url.trim().is_empty() {
            track_url
        } else {
            page_url.trim().to_string()
        };
        let id = format!(
            "dl-{}",
            util::now_secs() as u128 * 1000 + (self.tasks.len() as u128 % 1000)
        );
        let spec = DlSpec {
            id,
            track_id: track_id.to_string(),
            app: app_name,
            version,
            file: asset.name.clone(),
            url: asset.url.clone(),
            dir,
            sha256: asset.sha256.clone(),
            referer: Some(page),
        };
        if self
            .tasks
            .iter()
            .any(|t| t.spec.url == spec.url && matches!(t.status, TaskStatus::Running | TaskStatus::Queued))
        {
            self.toast(t("该文件已在下载队列中", "That file is already in the download queue"), cx);
            return;
        }
        let missing_sha = spec.sha256.is_none();
        self.tasks.insert(0, DlTask::new(spec));
        self.toast(
            if missing_sha {
                t(
                    "已加入下载队列（该文件无官方摘要，下载后无法校验）",
                    "Added to the queue — no official checksum, so the file can't be verified",
                )
            } else {
                t("已加入下载队列", "Added to the download queue")
            },
            cx,
        );
        self.pump_queue(cx);
    }

    fn pump_queue(&mut self, cx: &mut Context<Self>) {
        let max = self.cfg.max_concurrent.max(1);
        let running = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        let mut started = false;
        for i in (0..self.tasks.len()).rev() {
            if running + usize::from(started) >= max {
                break;
            }
            if self.tasks[i].status != TaskStatus::Queued {
                continue;
            }
            let task = &mut self.tasks[i];
            task.status = TaskStatus::Running;
            self.engine.start_download(
                self.http.clone(),
                task.spec.clone(),
                task.cancel.clone(),
                task.paused.clone(),
            );
            started = true;
        }
        if started {
            cx.notify();
        }
    }

    fn toggle_pause(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
            match t.status {
                TaskStatus::Running => {
                    t.paused.store(true, Ordering::Relaxed);
                    t.status = TaskStatus::Paused;
                }
                TaskStatus::Paused => {
                    t.paused.store(false, Ordering::Relaxed);
                    t.status = TaskStatus::Running;
                }
                _ => {}
            }
            cx.notify();
        }
    }

    fn cancel_task(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
            t.cancel.store(true, Ordering::Relaxed);
            t.paused.store(false, Ordering::Relaxed);
        }
        cx.notify();
    }

    fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.tasks.retain(|t| {
            matches!(t.status, TaskStatus::Running | TaskStatus::Queued | TaskStatus::Paused)
        });
        // 队列被清空时顺手收起：否则下一次下载会让这个空面板自己又弹出来，
        // 而用户上次是主动关掉它的。
        if self.tasks.is_empty() {
            self.queue_show = false;
        }
        cx.notify();
    }

    // -- 输入、弹窗与设置 --------------------------------------------------

    fn field(&self, id: FieldId) -> &TextField {
        match id {
            FieldId::AddInput => &self.add_input,
            FieldId::Filter => &self.filter,
            FieldId::DownloadDir => &self.set_dir,
            FieldId::Proxy => &self.set_proxy,
            FieldId::Token => &self.set_token,
        }
    }

    fn field_mut(&mut self, id: FieldId) -> &mut TextField {
        match id {
            FieldId::AddInput => &mut self.add_input,
            FieldId::Filter => &mut self.filter,
            FieldId::DownloadDir => &mut self.set_dir,
            FieldId::Proxy => &mut self.set_proxy,
            FieldId::Token => &mut self.set_token,
        }
    }

    /// 输入框在 `field_geom` 里的槽位。顺序与 [`Self::field`] 一一对应。
    fn field_slot(id: FieldId) -> usize {
        match id {
            FieldId::AddInput => 0,
            FieldId::Filter => 1,
            FieldId::DownloadDir => 2,
            FieldId::Proxy => 3,
            FieldId::Token => 4,
        }
    }

    /// 这个输入框上一帧画在哪儿（窗口坐标）。还没画过就是 `None`。
    fn geom(&self, id: FieldId) -> Rc<RefCell<Option<Bounds<Pixels>>>> {
        self.field_geom[Self::field_slot(id)].clone()
    }

    /// 窗口坐标 → 输入框里的字节偏移。
    ///
    /// 做法与正文选区一致：用同一套字体字号把**显示串**排一次版，再让排版结果
    /// 把鼠标位置换算回字节下标。密文框排的是圆点串，所以最后还要按「第几个字」
    /// 换回原文的偏移 —— 一个点对应一个字，字节数对不上。
    fn field_index_at(
        &self,
        id: FieldId,
        pos: Point<Pixels>,
        window: &mut Window,
    ) -> Option<usize> {
        let bounds = (*self.geom(id).borrow())?;
        let tf = self.field(id);
        let display = if tf.secret {
            "•".repeat(tf.text().chars().count())
        } else {
            tf.text().to_string()
        };
        // 与 `text_input` 的 `px_2` 一致：文字从左内边距起画。
        let pad = sp(8.);
        let width = (bounds.size.width - pad * 2.).max(px(1.));
        let prep = crate::select_text::prepare(
            &display,
            width,
            sp(13.),
            sp(20.),
            ui_font(),
            rgb(TEXT).into(),
            window,
        );
        let local = point(pos.x - bounds.origin.x - pad, pos.y - bounds.origin.y - pad);
        let at = prep.index_at(local);
        Some(if tf.secret {
            tf.char_to_byte(at / '•'.len_utf8())
        } else {
            at
        })
    }

    /// 在输入框上按下左键：光标落到点击处，开始一段可能被拖长的选区。
    fn start_field_drag(
        &mut self,
        id: FieldId,
        pos: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.field_index_at(id, pos, window) else {
            return;
        };
        self.field_mut(id).start_select(ix);
        self.field_drag = Some(id);
        cx.notify();
    }

    /// 拖到某处：锚点不动，另一头跟过去。
    fn drag_field_to(
        &mut self,
        id: FieldId,
        pos: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.field_drag != Some(id) || !self.field(id).is_dragging() {
            return;
        }
        let Some(ix) = self.field_index_at(id, pos, window) else {
            return;
        };
        let before = self.field(id).edit.selection;
        self.field_mut(id).extend_to(ix);
        if self.field(id).edit.selection != before {
            cx.notify();
        }
    }

    fn end_field_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.field_drag.take() {
            self.field_mut(id).end_select();
            cx.notify();
        }
    }

    fn focus_field(&mut self, id: FieldId, cx: &mut Context<Self>) {
        self.active_field = Some(id);
        self.pending_focus = Some(id);
        cx.notify();
    }

    /// 一个够用的单行编辑器：可打印字符、退格、删除、方向键、Ctrl+A / Ctrl+V。
    fn on_field_key(&mut self, id: FieldId, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // AM_LOG_KEY=1：把到达输入框的按键打到 stderr，排查「敲不进去」时用。
        if std::env::var_os("AM_LOG_KEY").is_some() {
            eprintln!("[key] {id:?} key={:?} char={:?}", ks.key, ks.key_char);
        }
        let cmd = ks.modifiers.platform || ks.modifiers.control;
        let mut handled = true;

        if cmd && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                self.field_mut(id).insert(&text);
            }
        } else if cmd && ks.key == "c" {
            self.field_copy(id, false, cx);
        } else if cmd && ks.key == "x" {
            self.field_copy(id, true, cx);
        } else if cmd && ks.key == "a" {
            self.field_mut(id).select_all();
        } else if cmd {
            handled = false;
        } else {
            match ks.key.as_str() {
                "backspace" => self.field_mut(id).backspace(),
                "delete" => self.field_mut(id).delete(),
                // 方向键带 Shift 就是「拉选区」：编辑器的通例，手不用离开键盘。
                "left" | "right" | "home" | "end" => {
                    let keep = ks.modifiers.shift;
                    let f = self.field_mut(id);
                    match ks.key.as_str() {
                        "left" => {
                            let ix = f.edit.prev_index();
                            f.edit.move_caret_to(ix, keep);
                        }
                        "right" => {
                            let ix = f.edit.next_index();
                            f.edit.move_caret_to(ix, keep);
                        }
                        "home" => f.edit.move_caret_to(0, keep),
                        _ => {
                            let end = f.text().len();
                            f.edit.move_caret_to(end, keep);
                        }
                    }
                }
                // 空格是 GPUI 里的「命名的键」：`key` 是 "space"，`key_char` 是
                // None（Windows 后端的 `parse_immutable` 先把它认走了，只有模拟
                // 输入才会补 `key_char`）。所以它走不到下面那条通用分支，不单独
                // 认一下，输入框里就敲不出空格。
                "space" => self.field_mut(id).insert(" "),
                // 添加弹窗里 Enter 直接提交（多行粘贴不受影响，那是 Ctrl+V 进来的）。
                "enter" if id == FieldId::AddInput => {
                    self.submit_add(cx);
                    handled = false;
                }
                "escape" => {
                    self.close_dialog(cx);
                    handled = false;
                }
                "enter" | "tab" => handled = false,
                _ => match ks.key_char.as_deref() {
                    Some(ch) => {
                        let printable: String =
                            ch.chars().filter(|c| !c.is_control()).collect();
                        if printable.is_empty() {
                            handled = false;
                        } else {
                            self.field_mut(id).insert(&printable);
                        }
                    }
                    None => handled = false,
                },
            }
        }
        if handled {
            cx.notify();
        }
    }

    fn open_dialog(&mut self, which: Dialog, cx: &mut Context<Self>) {
        match which {
            Dialog::Add => {
                self.add_skipped.clear();
            }
            Dialog::Settings => {
                self.set_dir.set(self.cfg.download_dir.clone());
                self.set_proxy.set(self.cfg.proxy.clone());
                self.set_token.set(self.cfg.github_token.clone());
            }
            Dialog::History => {}
        }
        self.dialog = Some(which);
        self.active_field = None;
        // 弹窗一开，设置里那颗按钮就换了位置（甚至被盖住），菜单还挂在旧坐标上
        // 会悬在窗口中间。语言本身已经落盘，直接收起即可。
        self.lang_menu = None;
        self.field_ctx = None;
        self.field_drag = None;
        cx.notify();
    }

    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        self.dialog = None;
        self.active_field = None;
        self.lang_menu = None;
        self.field_ctx = None;
        self.field_drag = None;
        self.refocus_root = true;
        cx.notify();
    }

    /// 把粘贴进来的文本交给 `parse_input`，只追加不覆盖已有条目。
    fn submit_add(&mut self, cx: &mut Context<Self>) {
        let text = self.add_input.text().to_string();
        if text.trim().is_empty() {
            self.toast(
                t(
                    "先粘贴要追踪的页面、仓库或软件站地址",
                    "Paste a page, repository or download-site URL to track first",
                ),
                cx,
            );
            return;
        }
        let parsed = crate::parse::parse_input(&text);
        if parsed.tracks.is_empty() {
            self.add_skipped = parsed.skipped.clone();
            let skipped = parsed.skipped.join(" / ");
            let detail = if skipped.is_empty() {
                String::new()
            } else if i18n::is_en() {
                // 英文用半角括号与空格，中文用全角括号，两边都不别扭。
                format!(" ({skipped})")
            } else {
                format!("（{skipped}）")
            };
            self.toast(
                format!(
                    "{}{detail}",
                    t("没能识别出可追踪的地址", "No trackable URL found")
                ),
                cx,
            );
            return;
        }
        let known: Vec<String> = self
            .tracks
            .iter()
            .flat_map(|t| [t.id.clone(), t.url.clone()])
            .collect();
        let mut first_new: Option<String> = None;
        let mut added = 0usize;
        let mut dup = 0usize;
        for track in parsed.tracks {
            if known.iter().any(|k| *k == track.id || *k == track.url) {
                dup += 1;
                continue;
            }
            if first_new.is_none() {
                first_new = Some(track.id.clone());
            }
            self.tracks.push(track);
            added += 1;
        }
        self.add_skipped = parsed.skipped;
        self.add_input.clear();
        crate::store::write_tracks(&self.tracks);

        if added == 0 {
            self.toast(
                tf!(
                    "没有新增条目（{} 个已在列表里）",
                    "Nothing new — all {} are already tracked",
                    dup
                ),
                cx,
            );
            return;
        }
        if let Some(id) = first_new {
            self.select(&id, cx);
        }
        self.dialog = None;
        self.field_ctx = None;
        self.refocus_root = true;
        self.toast(
            if dup > 0 {
                tf!(
                    "已添加 {} 个追踪项，跳过 {} 个重复项",
                    "Added {} item(s), skipped {} duplicate(s)",
                    added,
                    dup
                )
            } else {
                tf!(
                    "已添加 {} 个追踪项，开始检查版本…",
                    "Added {} item(s), checking versions…",
                    added
                )
            },
            cx,
        );
        self.resolve_all(cx);
    }

    fn submit_settings(&mut self, cx: &mut Context<Self>) {
        let dir = self.set_dir.text().trim().to_string();
        if dir.is_empty() {
            self.toast(t("下载目录不能为空", "The download folder cannot be empty"), cx);
            return;
        }
        self.cfg.download_dir = dir;
        self.cfg.proxy = self.set_proxy.text().trim().to_string();
        self.cfg.github_token = self.set_token.text().trim().to_string();
        let (http, err) = make_http(&self.cfg);
        if let Some(e) = err {
            // 代理填错时不要静默退回直连：那会让用户以为设置生效了。
            self.toast(
                crate::i18n::t1("代理或 Token 无效：{}", "Proxy or token is invalid: {}", e),
                cx,
            );
            return;
        }
        self.http = http;
        crate::store::write_config(&self.cfg);
        self.dialog = None;
        self.field_ctx = None;
        self.refocus_root = true;
        self.toast(t("设置已保存", "Settings saved"), cx);
    }

    fn delete_track(&mut self, id: &str, cx: &mut Context<Self>) {
        let name = self
            .tracks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.name.clone())
            .unwrap_or_default();
        self.tracks.retain(|t| t.id != id);
        self.confirm_delete = None;
        if self.selected.as_deref() == Some(id) {
            self.selected = self.tracks.first().map(|t| t.id.clone());
        }
        crate::store::write_tracks(&self.tracks);
        self.toast(
            tf!(
                "已删除「{}」",
                "Deleted \"{}\" and its local record",
                name
            ),
            cx,
        );
    }

    fn record_history(&mut self, task: &DlTask, status: RecordStatus, error: Option<String>) {
        let rec = DownloadRecord {
            id: task.spec.id.clone(),
            app: task.spec.app.clone(),
            file: task.spec.file.clone(),
            version: task.spec.version.clone(),
            url: task.spec.url.clone(),
            path: task.path.clone().unwrap_or_default(),
            size: task.downloaded,
            started_at: task.started_at,
            finished_at: util::now_secs(),
            status: status.stored().to_string(),
            sha_ok: task.sha_ok,
            error,
        };
        self.history.insert(0, rec);
        self.history.truncate(500);
        crate::store::write_history(&self.history);
    }

    fn retry_task(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
            if matches!(t.status, TaskStatus::Failed | TaskStatus::Canceled) {
                t.status = TaskStatus::Queued;
                t.error = None;
                t.sha_ok = None;
                t.speed = 0.0;
                t.started_at = util::now_secs();
                t.cancel = Arc::new(AtomicBool::new(false));
                t.paused = Arc::new(AtomicBool::new(false));
                // 已落盘的 .part 会被 download::run 自动续上，不必清零进度。
            }
        }
        self.pump_queue(cx);
    }

    /// 定时自动检查（`check_interval_min == 0` 表示关闭）。
    fn tick_auto(&mut self, cx: &mut Context<Self>) {
        let mins = self.cfg.check_interval_min;
        if mins == 0 {
            return;
        }
        if self.last_auto.elapsed() >= Duration::from_secs(mins.saturating_mul(60)) {
            self.last_auto = Instant::now();
            log::info!("定时检查：间隔 {mins} 分钟已到");
            self.resolve_all(cx);
        }
    }

    fn track(&self) -> Option<&Track> {
        let id = self.selected.as_deref()?;
        self.tracks.iter().find(|t| t.id == id)
    }

    /// 当前条目要展示的版本：候选标签选中项覆盖主版本字段。
    fn view(&self) -> Option<Release> {
        let t = self.track()?;
        let r = t.release.as_ref()?;
        let sel = *self.candidate_sel.get(&t.id).unwrap_or(&0);
        Some(r.view_of(sel))
    }

    fn select(&mut self, id: &str, cx: &mut Context<Self>) {
        self.selected = Some(id.to_string());
        self.tab = Tab::Overview;
        self.done_pop = None;
        self.done_pop_hide_at = None;
        cx.notify();
    }

    fn toast(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        let stamp = Instant::now();
        self.toast = Some((msg.into(), stamp));
        // 4 秒后自己消失。没有下载在跑时界面不会周期性重绘，光靠渲染时过滤
        // `elapsed < 4s` 这条消息会一直挂在屏幕上。这里补一个定时器触发重绘；
        // 期间用户又点了一次，`stamp` 就对不上，旧定时器自然什么也不做。
        cx.spawn(move |this: gpui::WeakEntity<App>, cx: &mut gpui::AsyncApp| {
            let mut cx = cx.clone();
            async move {
                cx.background_executor()
                    .timer(Duration::from_millis(4200))
                    .await;
                let _ = this.update(&mut cx, |this, cx| {
                    if this.toast.as_ref().map(|(_, at)| *at) == Some(stamp) {
                        this.toast = None;
                        cx.notify();
                    }
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// 写剪贴板。GPUI 0.2 还没有文本选中，凡是可能想复制走的文本都给一个按钮。
    fn copy(&mut self, text: impl Into<String>, what: What, cx: &mut Context<Self>) {
        let text = text.into();
        let msg = if text.trim().is_empty() {
            tf!("没有可复制的{}", "Nothing to copy — no {}", what.label())
        } else {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            tf!("已复制{}", "Copied {}", what.label())
        };
        self.toast(msg, cx);
    }

    /// 一段可以拖选的正文。
    ///
    /// `owner` 必须唯一标识这块文本（带上条目与页签），否则换了条目之后旧选区
    /// 的字节偏移会套到新文本上。高亮由 `select_text` 自己画，鼠标事件挂在这层
    /// div 上 —— bounds 从绘制阶段回填的 `sel_geom` 里取。
    fn selectable_body(
        &self,
        owner: String,
        text: &str,
        width: Pixels,
        font_size: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let prep = crate::select_text::prepare(
            text,
            width,
            font_size,
            font_size * 1.45,
            ui_font(),
            rgb(TEXT).into(),
            window,
        );
        let sel = self
            .sel
            .as_ref()
            .filter(|s| s.owner == owner)
            .map(|s| (s.anchor, s.head));
        let hit_down = self.sel_geom.clone();
        let hit_move = self.sel_geom.clone();
        let down = prep.clone();
        let drag = prep.clone();
        let owner_press = owner.clone();
        let text_press = text.to_string();
        let owner_ctx = owner.clone();
        let text_ctx = text.to_string();
        div()
            .id(SharedString::from(format!("sel:{owner}")))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                    let Some(bounds) = *hit_down.borrow() else {
                        return;
                    };
                    let at = down.index_at(ev.position - bounds.origin);
                    this.select_start(owner_press.clone(), at, text_press.clone(), cx);
                }),
            )
            .on_mouse_move(cx.listener(move |this, ev: &gpui::MouseMoveEvent, _, cx| {
                // 只有按住键拖动才扩选；单纯悬停不动选区。
                if ev.pressed_button.is_none() {
                    return;
                }
                let Some(bounds) = *hit_move.borrow() else {
                    return;
                };
                this.select_drag(drag.index_at(ev.position - bounds.origin), cx);
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| this.select_finish(cx)),
            )
            // 右键只开菜单，不动选区 —— 选好的那一段要留在原位等高亮跟着菜单一起用。
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                    let p = ev.position;
                    this.open_ctx(owner_ctx.clone(), text_ctx.clone(), p.x.into(), p.y.into(), cx);
                }),
            )
            .child(crate::select_text::paint(
                prep,
                sel,
                self.sel_geom.clone(),
            ))
            .into_any_element()
    }

    fn select_start(&mut self, owner: String, at: usize, text: String, cx: &mut Context<Self>) {
        self.sel = Some(Selection {
            owner,
            anchor: at,
            head: at,
            text,
        });
        cx.notify();
    }

    fn select_drag(&mut self, at: usize, cx: &mut Context<Self>) {
        if let Some(sel) = self.sel.as_mut() {
            if sel.head != at {
                sel.head = at;
                cx.notify();
            }
        }
    }

    /// 松手即复制。
    ///
    /// 不用 Ctrl+C：GPUI 的按键要走焦点与 action，还得跟输入框抢这个快捷键，
    /// 而这段文字的唯一用途就是被带走 —— 拖完直接进剪贴板，回执里报字数。
    fn select_finish(&mut self, cx: &mut Context<Self>) {
        let Some(sel) = self.sel.as_ref() else {
            return;
        };
        let Some((picked, n)) = picked(sel) else {
            return;
        };
        self.copy(picked, What::Selection(n), cx);
    }

    // -- 输入框的右键菜单 --------------------------------------------------

    /// 输入框上右键。两个菜单互斥：同一时刻屏幕上只该有一张。
    fn open_field_ctx(&mut self, field: FieldId, x: f32, y: f32, cx: &mut Context<Self>) {
        self.ctx = None;
        self.field_ctx = Some(FieldCtx { field, x, y });
        cx.notify();
    }

    fn close_field_ctx(&mut self, cx: &mut Context<Self>) {
        if self.field_ctx.take().is_some() {
            cx.notify();
        }
    }

    /// 复制 / 剪切输入框里的选区。
    ///
    /// 没有选区就什么也不做 —— 与 Windows 的文本框一致（Ctrl+C 在空选区下静默），
    /// 菜单里这两项也会跟着置灰。输入框没有拖选，选区只能来自 Ctrl+A 或菜单里的
    /// 「全选」，所以范围要么是空的、要么是整段。
    fn field_copy(&mut self, field: FieldId, cut: bool, cx: &mut Context<Self>) {
        let Some((text, n)) = self.field(field).selected() else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        if cut {
            self.field_mut(field).cut_selection();
            self.toast(tf!("已剪切{}", "Cut {}", What::Selection(n).label()), cx);
        } else {
            self.toast(
                tf!("已复制{}", "Copied {}", What::Selection(n).label()),
                cx,
            );
        }
    }

    /// 右键菜单里某一项的落地动作。
    fn field_act(&mut self, field: FieldId, act: FieldAct, cx: &mut Context<Self>) {
        self.field_ctx = None;
        match act {
            FieldAct::Cut => self.field_copy(field, true, cx),
            FieldAct::Copy => self.field_copy(field, false, cx),
            FieldAct::Paste => {
                match cx.read_from_clipboard().and_then(|c| c.text()) {
                    Some(text) => {
                        self.field_mut(field).insert(&text);
                        cx.notify();
                    }
                    None => self.toast(
                        t("剪贴板里没有文字", "The clipboard has no text"),
                        cx,
                    ),
                }
            }
            FieldAct::All => {
                // 选完之后把焦点也给它：接着按 Ctrl+C 就该走这颗框。
                self.field_mut(field).select_all();
                self.focus_field(field, cx);
            }
        }
    }

    // -- 选区的右键菜单 ----------------------------------------------------

    fn open_ctx(
        &mut self,
        owner: String,
        text: String,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.ctx = Some(CtxMenu { x, y, owner, text });
        self.field_ctx = None;
        cx.notify();
    }

    fn close_ctx(&mut self, cx: &mut Context<Self>) {
        if self.ctx.take().is_some() {
            cx.notify();
        }
    }

    /// 菜单里的「复制」。走的是和松手即复制同一条 `picked` → `copy`，两处措辞、
    /// 字数统计不会出现两种口径。
    fn ctx_copy(&mut self, cx: &mut Context<Self>) {
        self.ctx = None;
        match self.sel.as_ref().and_then(picked) {
            Some((text, n)) => self.copy(text, What::Selection(n), cx),
            None => self.toast(
                t(
                    "先用左键拖选一段文字，再复制",
                    "Drag over some text first, then copy",
                ),
                cx,
            ),
        }
    }

    /// 整段选中。菜单里的这个入口比「从第一个字拖到最后一个字」实用得多。
    fn ctx_select_all(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.ctx.take() else {
            return;
        };
        let len = menu.text.len();
        self.sel = Some(Selection {
            owner: menu.owner,
            anchor: 0,
            head: len,
            text: menu.text,
        });
        cx.notify();
    }

    fn ctx_clear(&mut self, cx: &mut Context<Self>) {
        self.ctx = None;
        self.sel = None;
        cx.notify();
    }

    /// 右键菜单层：一层透明遮罩负责「点别处关掉」，菜单本身画在它上面。
    ///
    /// 遮罩覆盖整窗，所以菜单打开期间第一次点任何地方都只是关菜单 —— 这与
    /// 系统菜单的行为一致，也免得把那次点击漏给下面的按钮。
    fn ctx_layer(&self, vw: f32, vh: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(menu) = self.ctx.as_ref() else {
            return div().into_any_element();
        };
        let has = self
            .sel
            .as_ref()
            .filter(|s| s.owner == menu.owner)
            .and_then(picked)
            .is_some();
        let w = 172. * scale();
        let h = (if has { 3. } else { 2. } * 34. + 12.) * scale();
        // 贴边时往里收：菜单是绝对定位的，敞着画会被窗口边缘截掉一半。
        let x = menu.x.min((vw - w - 8.).max(8.)).max(8.);
        let y = if menu.y + h + 8. > vh {
            (menu.y - h).max(8.)
        } else {
            menu.y
        };

        #[derive(Clone, Copy, PartialEq)]
        enum Act {
            Copy,
            All,
            Clear,
        }

        let mut items: Vec<(Act, String, bool)> = Vec::new();
        items.push((Act::Copy, t("复制", "Copy").to_string(), has));
        items.push((Act::All, t("全选", "Select all").to_string(), true));
        if has {
            items.push((Act::Clear, t("取消选择", "Clear selection").to_string(), true));
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                // 透明遮罩本身不画任何东西，只负责接住那次「点别处」。
                // 必须 stop_propagation：遮罩底下就是正文，事件继续往下走的话
                // 正文会收到这次鼠标按下，把辛苦选好的选区改成空选区。
                div()
                    .id("ctx-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_ctx(cx);
                        }),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_ctx(cx);
                        }),
                    ),
            )
            .child(
                div()
                    .id("ctx-menu")
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .py(px(6. * scale()))
                    .rounded_md()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .children(items.into_iter().enumerate().map(|(i, (act, label, enabled))| {
                        let color = if enabled { TEXT } else { FAINT };
                        div()
                            .id(SharedString::from(format!("ctx-item-{i}")))
                            .px_3()
                            .py(px(7. * scale()))
                            .text_size(sp(13.))
                            .text_color(rgb(color))
                            .whitespace_nowrap()
                            .when(enabled, |d| {
                                // 在**按下**时就行动，不是等松手：菜单下面压着正文，
                                // 若这一下先漏给正文，选区会被改成空选区，等 on_click
                                // 再复制就只剩「没有选中文字」了。系统菜单同样按下即选。
                                d.cursor_pointer()
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            match act {
                                                Act::Copy => this.ctx_copy(cx),
                                                Act::All => this.ctx_select_all(cx),
                                                Act::Clear => this.ctx_clear(cx),
                                            }
                                        }),
                                    )
                            })
                            .child(label)
                    })),
            )
            .into_any_element()
    }

    /// 输入框右键菜单层。做法与 [`Self::ctx_layer`] 相同：一层透明遮罩接住
    /// 「点别处」，菜单画在它上面、贴着鼠标右下方，贴边时往里收。
    fn field_ctx_layer(&self, vw: f32, vh: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(menu) = self.field_ctx.as_ref() else {
            return div().into_any_element();
        };
        let field = menu.field;
        let tf = self.field(field);
        // 与 Windows 的文本框一致：没有选区时「剪切 / 复制」置灰；输入框是空的
        // 时候「全选」也没有意义。
        let has_sel = tf.selected().is_some();
        let has_text = !tf.text().is_empty();
        let items: [(FieldAct, &'static str, bool); 4] = [
            (FieldAct::Cut, t("剪切", "Cut"), has_sel),
            (FieldAct::Copy, t("复制", "Copy"), has_sel),
            (FieldAct::Paste, t("粘贴", "Paste"), true),
            (FieldAct::All, t("全选", "Select all"), has_text),
        ];
        let w = 148. * scale();
        let h = (items.len() as f32 * 34. + 12.) * scale();
        let x = menu.x.min((vw - w - 8.).max(8.)).max(8.);
        let y = if menu.y + h + 8. > vh {
            (menu.y - h).max(8.)
        } else {
            menu.y
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                div()
                    .id("field-ctx-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_field_ctx(cx);
                        }),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_field_ctx(cx);
                        }),
                    ),
            )
            .child(
                div()
                    .id("field-ctx-menu")
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .py(px(6. * scale()))
                    .rounded_md()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .children(items.into_iter().enumerate().map(|(i, (act, label, enabled))| {
                        let color = if enabled { TEXT } else { FAINT };
                        div()
                            .id(SharedString::from(format!("field-ctx-item-{i}")))
                            .px_3()
                            .py(px(7. * scale()))
                            .text_size(sp(13.))
                            .text_color(rgb(color))
                            .whitespace_nowrap()
                            .when(enabled, |d| {
                                d.cursor_pointer()
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.field_act(field, act, cx);
                                        }),
                                    )
                            })
                            .child(label)
                    })),
            )
            .into_any_element()
    }

    /// 侧栏「正在下载」那条上悬停时浮出的详情卡。
    ///
    /// 卡片不能画在行里：列表是滚动容器，超出侧栏宽度的部分会被裁掉。所以和
    /// 右键菜单同一套做法 —— 挂在根节点上绝对定位，横向贴侧栏右沿、纵向跟着
    /// 悬停的那一行，再按窗口边缘往里收。
    fn dl_pop_layer(&self, vw: f32, vh: f32) -> AnyElement {
        let Some(pop) = self.dl_pop.as_ref() else {
            return div().into_any_element();
        };
        // 任务可能刚在这一帧结束：没有活任务就当作已经散场。
        let tasks = self.live_tasks(&pop.track);
        if tasks.is_empty() {
            return div().into_any_element();
        }
        let n = tasks.len();
        let w = 248. * scale();
        // 高度按条目数估：标题一行 + 每个任务 3 行（文件名 / 数字 / 进度条）。
        // 估高只用于贴边翻向，多算几个像素不会露馅，少算才会被窗口底边切掉。
        let h = (34. + n as f32 * 44.) * scale();
        // 横坐标贴住侧栏右沿而不是跟着光标：侧栏只有 272 宽，跟着光标画的卡片会
        // 盖住下面两三条 —— 而那正是用户想拿来对照的邻居。纵坐标跟着行，卡片就
        // 像是从这一条上「抽」出来的。
        let x = (272. * scale() + 6.).min((vw - w - 8.).max(8.)).max(8.);
        let y = (pop.y - 12.).min((vh - h - 8.).max(8.)).max(8.);
        let app = tasks[0].spec.app.clone();

        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .text_ellipsis()
                            .child(app),
                    )
                    .child(div().flex_1())
                    // 一条目同时下多个文件时，必须先说明「下面有几条」，否则会被
                    // 当成同一个文件刷了两遍进度。
                    .when(n > 1, |d| {
                        d.child(
                            div()
                                .text_size(sp(11.5))
                                .text_color(rgb(ACCENT))
                                .child(tf!("{} 个文件", "{} files", n)),
                        )
                    }),
            )
            .children(tasks.into_iter().map(|task| {
                let pct = (task.frac() * 100.).round() as i32;
                let size = if task.total == 0 {
                    format!(
                        "{} · {}",
                        util::fmt_bytes(task.downloaded),
                        t("大小未知", "size unknown")
                    )
                } else {
                    format!(
                        "{} / {} · {pct}%",
                        util::fmt_bytes(task.downloaded),
                        util::fmt_bytes(task.total)
                    )
                };
                // 速度只在真的在跑、而且已经有测速结果时才有意义；刚起步那一两帧
                // 速度还是 0，此时显示「下载中」比一个「—」更像话。
                let right = if task.status == TaskStatus::Running && task.speed > 0. {
                    util::fmt_speed(task.speed)
                } else {
                    task.status.label().to_string()
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(TEXT))
                            .text_ellipsis()
                            .child(task.spec.file.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(sp(11.5))
                                    .text_color(rgb(MUTED))
                                    .text_ellipsis()
                                    .min_w_0()
                                    .child(size),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(sp(11.5))
                                    .text_color(rgb(status_color(task.status)))
                                    .child(right),
                            ),
                    )
                    .child(
                        div()
                            .h(px(3.))
                            .w_full()
                            .rounded_full()
                            .bg(rgb(0xDDE3EE))
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(task.frac()))
                                    .rounded_full()
                                    .bg(rgb(status_color(task.status))),
                            ),
                    )
            }))
            .into_any_element()
    }

    /// 切界面语言。立刻生效并落盘 —— 语言没有「取消」这种半程状态。
    fn set_lang(&mut self, lang: i18n::Lang, cx: &mut Context<Self>) {
        self.apply_lang(lang, lang.code().to_string(), cx);
    }

    /// 回到「跟随系统」：配置里留空，下次启动重新检测。
    fn set_lang_auto(&mut self, cx: &mut Context<Self>) {
        let lang = i18n::Lang::resolve("");
        self.apply_lang(lang, String::new(), cx);
    }

    /// `stored` 与生效值是两回事：跟随系统时前者为空、后者是检测结果，
    /// 所以不能拿 `current()` 是否变化来判断要不要写盘。
    fn apply_lang(&mut self, lang: i18n::Lang, stored: String, cx: &mut Context<Self>) {
        let changed = self.cfg.lang != stored || i18n::current() != lang;
        self.cfg.lang = stored;
        i18n::set(lang);
        if changed {
            crate::store::write_config(&self.cfg);
        }
        cx.notify();
    }

    /// 调整界面缩放。0.1 的浮点累加会留下 1.2000000000000002 这种尾巴，
    /// 所以每步都四舍五入回一位小数，免得设置里显示成 120.0000001%。
    fn nudge_scale(&mut self, delta: f32, cx: &mut Context<Self>) {
        let next = ((self.cfg.ui_scale + delta) * 10.0).round() / 10.0;
        let next = next.clamp(SCALE_MIN, SCALE_MAX);
        if (next - self.cfg.ui_scale).abs() < f32::EPSILON {
            return;
        }
        self.cfg.ui_scale = next;
        set_ui_scale(next);
        crate::store::write_config(&self.cfg);
        cx.notify();
    }

    fn update_count(&self) -> usize {
        self.tracks.iter().filter(|t| t.has_update()).count()
    }

    // -- 视图片段 ----------------------------------------------------------

    fn top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.tracks.len();
        let updates = self.update_count();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_5()
            .py_3()
            .bg(rgb(CARD))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(sp(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("App Monitor"),
                    )
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(rgb(MUTED))
                            .child(t("跨平台软件追踪下载器", "Cross-platform release tracker")),
                    ),
            )
            .child(badge(
                count_text(total, " 个追踪项", " item", " items", " 件", "개"),
                MUTED,
                0xF2F4F8,
            ))
            .when(updates > 0, |d| {
                d.child(badge(
                    count_text(updates, " 项有更新", " update", " updates", " 件の更新", "개 업데이트"),
                    ORANGE,
                    ORANGE_SOFT,
                ))
                // 徽章旁边直接给出口：一次清掉全部提醒，不必挨个点进条目。
                .child(
                    chip(
                        "mark-all-read",
                        t("全部标为已读", "Mark all read"),
                        ORANGE,
                        ORANGE_SOFT,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        let n = this.mark_all_read(cx);
                        this.toast(
                            match i18n::current() {
                                i18n::Lang::Zh => format!("已把 {n} 项更新标为已读"),
                                i18n::Lang::En => format!(
                                    "Marked {n} update{} as read",
                                    if n == 1 { "" } else { "s" }
                                ),
                                i18n::Lang::Ja => format!("{n} 件の更新を既読にしました"),
                                i18n::Lang::Ko => format!("{n}개 업데이트를 읽음 처리했습니다"),
                            },
                            cx,
                        );
                    })),
                )
            })
            .child(div().flex_1())
            .when(!self.resolving.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(rgb(ACCENT))
                        .child(tf!(
                            "检查中 {}…",
                            "Checking {}…",
                            self.resolving.len()
                        )),
                )
            })
            // 下载队列的开关。队列默认收在底部之外，它的入口就放在「检查全部
            // 更新」左边：用户想看下载在哪儿，第一眼扫的是这一排按钮。
            .when(!self.tasks.is_empty(), |d| {
                let active = self.active_count();
                let open = self.queue_show;
                d.child(
                    chip(
                        "btn-queue",
                        if active > 0 {
                            format!("{} {}", t("下载队列", "Downloads"), active)
                        } else {
                            t("下载队列", "Downloads").to_string()
                        },
                        if open { ACCENT } else { TEXT },
                        if open { ACCENT_SOFT } else { 0xF2F4F8 },
                    )
                    // 和旁边的「检查全部更新」同一尺寸：这排按钮里突然冒出一颗
                    // 小一号的胶囊，看上去像是「次要到不用点」。
                    .px_3()
                    .py_2()
                    .text_size(sp(13.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.queue_show = !this.queue_show;
                        cx.notify();
                    })),
                )
            })
            .child(
                btn("btn-check", t("检查全部更新", "Check all"), ACCENT, 0xFFFFFF)
                    .on_click(cx.listener(|this, _, _, cx| this.resolve_all(cx))),
            )
            .child(btn("btn-hist", t("下载历史", "History"), 0xF2F4F8, TEXT).on_click(
                cx.listener(|this, _, _, cx| this.open_dialog(Dialog::History, cx)),
            ))
            .child(btn("btn-settings", t("设置", "Settings"), 0xF2F4F8, TEXT).on_click(
                cx.listener(|this, _, _, cx| this.open_dialog(Dialog::Settings, cx)),
            ))
            .child(btn("btn-add", t("+ 添加追踪", "+ Add"), ACCENT, 0xFFFFFF).on_click(
                cx.listener(|this, _, _, cx| {
                    this.open_dialog(Dialog::Add, cx);
                    // 打开就把焦点给输入框，粘贴不用先点一下。
                    this.focus_field(FieldId::AddInput, cx);
                }),
            ))
            .child(
                self.lang_picker(
                    "btn-lang-top",
                    LangAnchor::Top,
                    self.lang_anchor_top.clone(),
                    cx,
                ),
            )
    }

    /// 顶栏最右侧的语言下拉按钮：显示当前语言名，点开才列出全部。
    ///
    /// 这里原先是一排「中 | EN | 日 | 한」小格，图的是「界面是外语时还认得出
    /// 哪颗是自己」。语言越加越多，那排小格会把顶栏挤满，于是收成一颗下拉 ——
    /// 按钮上写的是**当前语言自己的名字**，落到哪种界面都还能读出「我现在是
    /// 中文 / English」，不至于两眼一抹黑找不到路。
    ///
    /// `geom` 是这颗按钮绘制时的矩形，由外面的容器用 `on_children_prepainted`
    /// 回填 —— GPUI 的点击回调里拿不到元素自己的位置，菜单要贴边画只能这样。
    fn lang_picker(
        &self,
        id: &'static str,
        slot: LangAnchor,
        geom: Rc<RefCell<Option<Bounds<Pixels>>>>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cur = i18n::current();
        let open = self.lang_menu.is_some();
        let cell_paint = geom.clone();
        let cell_click = geom;
        // 多包一层只为量到下面那颗按钮的 bounds：`on_children_prepainted` 给的
        // 正是子元素的矩形，比在按钮里塞 canvas 干净（canvas 会参与布局）。
        div()
            .flex_shrink_0()
            .on_children_prepainted(move |bounds, _, _| {
                if let Some(b) = bounds.first() {
                    *cell_paint.borrow_mut() = Some(*b);
                }
            })
            .child(
                div()
                    .id(id)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .min_w(sp(78.))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(if open { ACCENT_SOFT } else { 0xF2F4F8 }))
                    .text_size(sp(13.))
                    .text_color(rgb(if open { ACCENT } else { TEXT }))
                    .hover(|s| s.opacity(0.88))
                    .cursor_pointer()
                    .child(div().flex_1().child(cur.label()))
                    .child(
                        div()
                            .text_size(sp(9.))
                            .text_color(rgb(if open { ACCENT } else { MUTED }))
                            .child("▼"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let has = cell_click.borrow().is_some();
                        this.toggle_lang_menu(slot, has, cx);
                    })),
            )
    }

    /// 开/关语言菜单。锚点还没量到（理论上不会 —— 点击一定发生在绘制之后）
    /// 就当作没点，免得菜单在一个猜出来的坐标上冒出来。
    fn toggle_lang_menu(&mut self, slot: LangAnchor, has: bool, cx: &mut Context<Self>) {
        if self.lang_menu.take().is_some() {
            cx.notify();
            return;
        }
        if has {
            self.lang_menu = Some(slot);
            cx.notify();
        }
    }

    fn close_lang_menu(&mut self, cx: &mut Context<Self>) {
        if self.lang_menu.take().is_some() {
            cx.notify();
        }
    }

    /// 菜单里选一项。`None` 是「跟随系统」：配置里留空，下次启动重新检测。
    fn pick_lang(&mut self, want: Option<i18n::Lang>, cx: &mut Context<Self>) {
        self.lang_menu = None;
        match want {
            Some(l) => self.set_lang(l, cx),
            None => self.set_lang_auto(cx),
        }
    }

    /// 语言菜单层：透明遮罩（点别处收起）+ 贴锚点画的列表。
    ///
    /// 遮罩覆盖整窗、菜单画在它上面，与右键菜单是同一套做法：打开期间第一次点
    /// 任何地方都只是收起菜单，不会漏给下面的按钮 —— 尤其是别再点一次那颗
    /// 按钮，否则「点开 / 点关」会连成两次 toggle。
    fn lang_menu_layer(&self, vw: f32, vh: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(slot) = self.lang_menu else {
            return div().into_any_element();
        };
        // 每帧重新读锚点，而不是在打开时存一份快照：窗口一缩放（或设置里的内容
        // 滚了一屏），按钮就换了位置，快照会让菜单挂在半空。
        let cell = match slot {
            LangAnchor::Top => &self.lang_anchor_top,
            LangAnchor::Set => &self.lang_anchor_set,
        };
        let Some(anchor) = *cell.borrow() else {
            return div().into_any_element();
        };
        let following = self.cfg.lang.trim().is_empty();
        let cur = i18n::current();
        // 「跟随系统」+ 现有语言。
        let mut items: Vec<(Option<i18n::Lang>, &'static str, bool)> =
            vec![(None, t("跟随系统", "System"), following)];
        for l in i18n::Lang::ALL {
            items.push((Some(l), l.label(), !following && cur == l));
        }

        let w = 196. * scale();
        let item_h = 31. * scale();
        // 高度估算法与悬停卡片一致：估高只用于贴边时翻向，多算几像素不露馅，
        // 少算才会被窗口底边切掉。
        let h = items.len() as f32 * item_h + 12. * scale();
        let ax: f32 = anchor.origin.x.into();
        let ay: f32 = anchor.origin.y.into();
        let aw: f32 = anchor.size.width.into();
        let ah: f32 = anchor.size.height.into();
        // 右对齐按钮右沿（它在顶栏最右边，左对齐会把菜单往窗口外推），
        // 横向再按窗口边缘收一下。
        let x = (ax + aw - w).min((vw - w - 8.).max(8.)).max(8.);
        let y = if ay + ah + h + 8. > vh {
            (ay - h - 4.).max(8.)
        } else {
            ay + ah + 4.
        };

        let mk_item = |i: usize,
                       want: Option<i18n::Lang>,
                       label: &'static str,
                       active: bool,
                       cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(format!("lang-item-{i}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .h(px(item_h))
                .text_size(sp(13.))
                .text_color(rgb(if active { ACCENT } else { TEXT }))
                .when(active, |d| d.bg(rgb(ACCENT_SOFT)))
                .cursor_pointer()
                .hover(|s| s.bg(rgb(HOVER)))
                .child(
                    div()
                        .w(sp(12.))
                        .flex_shrink_0()
                        .text_size(sp(11.))
                        .child(if active { "✓" } else { "" }),
                )
                .child(div().flex_1().whitespace_nowrap().child(label))
                // 按下即选，与右键菜单同理：遮罩下面压着正文，等 on_click 再动手
                // 时那一下已经先落到底下的按钮上了。
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.pick_lang(want, cx);
                    }),
                )
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                div()
                    .id("lang-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_lang_menu(cx);
                        }),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_lang_menu(cx);
                        }),
                    ),
            )
            .child(
                div()
                    .id("lang-menu")
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .py(px(6. * scale()))
                    .rounded_md()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    // 第一项是「跟随系统」，其余四种语言之间拉一条细线：前者是
                    // 「怎么决定」，后者是「决定成什么」，两类不该混在一起读。
                    .child(mk_item(0, items[0].0, items[0].1, items[0].2, cx))
                    .child(
                        div()
                            .mx_3()
                            .my_1()
                            .h(px(1.))
                            .bg(rgb(BORDER)),
                    )
                    .children(items[1..].iter().enumerate().map(|(i, (want, label, active))| {
                        mk_item(i + 1, *want, label, *active, cx)
                    })),
            )
            .into_any_element()
    }

    /// 某个追踪项当前**还没结束**的下载任务（排队 / 进行中 / 暂停），新的在前。
    ///
    /// 只看未结束的：已完成的任务在列表里再挂一条满格绿条没有信息量，结果由
    /// toast 和下载历史交代。同一个条目可能同时下好几个文件（Windows 版 +
    /// Linux 版），所以这里返回列表，侧栏与悬停浮层都按整组来算。
    fn live_tasks(&self, track_id: &str) -> Vec<&DlTask> {
        self.tasks
            .iter()
            .filter(|t| t.spec.track_id == track_id && is_live(t.status))
            .collect()
    }

    /// 还在跑的任务数（排队、进行中、暂停都算「没结束」）。顶栏按钮的角标和
    /// 队列标题都用它。
    fn active_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Running | TaskStatus::Queued | TaskStatus::Paused
                )
            })
            .count()
    }

    /// 这一项已经下完的版本，最近完成的在前。
    ///
    /// 数据来自 `history.json`：同一个版本重复下过（断点续传后又点了一次）只留
    /// 最后一次，否则清单里会出现同一版两行、指向同两个目录。失败的记录不算
    /// ——「下过但没成」没有目录可打开。
    fn done_versions(&self, name: &str) -> Vec<DoneVersion> {
        let done = RecordStatus::Done.stored();
        let mut out: Vec<DoneVersion> = Vec::new();
        for r in &self.history {
            if r.app != name || r.status != done || r.path.is_empty() {
                continue;
            }
            let dir = std::path::Path::new(&r.path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            if dir.is_empty() {
                continue;
            }
            match out.iter_mut().find(|d| d.version == r.version) {
                Some(d) => {
                    if r.finished_at > d.at {
                        d.dir = dir;
                        d.size = r.size;
                        d.at = r.finished_at;
                    }
                }
                None => out.push(DoneVersion {
                    version: r.version.clone(),
                    dir,
                    size: r.size,
                    at: r.finished_at,
                }),
            }
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.at));
        out
    }

    /// 悬停浮层「已下载版本」。与 [`Self::dl_pop_layer`] 同一套定位，只是内容
    /// 换成可以点的行：点一行就用系统文件管理器打开那个版本目录。
    fn done_pop_layer(&self, vw: f32, vh: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(pop) = self.done_pop.as_ref() else {
            return div().into_any_element();
        };
        let Some(track) = self.tracks.iter().find(|t| t.id == pop.track) else {
            return div().into_any_element();
        };
        let vers = self.done_versions(&track.name);
        if vers.is_empty() {
            return div().into_any_element();
        }
        const MAX_ROWS: usize = 8;
        let shown = vers.len().min(MAX_ROWS);
        let more = vers.len() - shown;
        let w = 268. * scale();
        let h = (40. + shown as f32 * 26. + 20.) * scale();
        let x = (272. * scale() + 6.).min((vw - w - 8.).max(8.)).max(8.);
        let y = (pop.y - 12.).min((vh - h - 8.).max(8.)).max(8.);

        // 鼠标能从条目挪进浮层里点：离开条目先给一小段宽限，浮层自己收到
        // hover(true) 就把宽限取消，离开浮层再重新计时。
        let ent = cx.entity();

        div()
            .id("done-pop")
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(w))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .overflow_hidden()
            .on_hover(move |hovered: &bool, _, app| {
                let hovered = *hovered;
                ent.update(app, |this, cx| {
                    if hovered {
                        if this.done_pop_hide_at.take().is_some() {
                            cx.notify();
                        }
                    } else if this.done_pop.is_some() {
                        this.done_pop_hide_at =
                            Some(Instant::now() + Duration::from_millis(220));
                        cx.notify();
                    }
                });
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .text_ellipsis()
                            .child(track.name.clone()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(sp(11.5))
                            .text_color(rgb(GREEN))
                            .child(tf!("已下 {} 个版本", "{} versions", vers.len())),
                    ),
            )
            .children(vers.into_iter().take(MAX_ROWS).map(|v| {
                let dir = v.dir.clone();
                div()
                    .id(SharedString::from(format!("done-{}", v.version)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(sp(12.))
                            .text_color(rgb(TEXT))
                            .text_ellipsis()
                            .child(format!("v{}", v.version.trim_start_matches('v'))),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(sp(11.))
                            .text_color(rgb(FAINT))
                            .child(util::fmt_bytes(v.size)),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(sp(11.))
                            .text_color(rgb(ACCENT))
                            .child(t("打开", "Open")),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let _ = open::that_detached(&dir);
                        this.done_pop = None;
                        this.done_pop_hide_at = None;
                        cx.notify();
                    }))
            }))
            .when(more > 0, |d| {
                d.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_size(sp(11.))
                        .text_color(rgb(FAINT))
                        .child(tf!("还有 {} 个更早的版本", "{} older versions", more)),
                )
            })
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(sp(11.))
                    .text_color(rgb(FAINT))
                    .child(t("点一行打开所在文件夹", "Click a row to open its folder")),
            )
            .into_any_element()
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let needle = self.filter.text().trim().to_lowercase();
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            if !needle.is_empty()
                && !util::icontains(&t.name, &needle)
                && !util::icontains(&t.url, &needle)
            {
                continue;
            }
            match groups.iter_mut().find(|(g, _)| g == &t.group) {
                Some((_, v)) => v.push(i),
                None => groups.push((t.group.clone(), vec![i])),
            }
        }
        let shown: usize = groups.iter().map(|(_, v)| v.len()).sum();
        let selected = self.selected.clone();

        div()
            .flex()
            .flex_col()
            .w(sp(272.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .pt_3()
                    .pb_2()
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(MUTED))
                            .child(t("追踪列表", "TRACKED")),
                    )
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(rgb(FAINT))
                            .child(if shown == self.tracks.len() {
                                format!("{}", self.tracks.len())
                            } else {
                                format!("{shown}/{}", self.tracks.len())
                            }),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .pb_2()
                    .child(self.text_input(
                        FieldId::Filter,
                        t("筛选名称或地址", "Filter by name or URL"),
                        cx,
                    )),
            )
            .child(
                div()
                    .id("track-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_1()
                    .px_2()
                    .pb_3()
                    .overflow_y_scroll()
                    .when(groups.is_empty(), |d| {
                        d.child(
                            div()
                                .px_2()
                                .py_2()
                                .text_size(sp(12.5))
                                .text_color(rgb(FAINT))
                                .child(if needle.is_empty() {
                                    t(
                                        "还没有追踪项，点右上角「添加追踪」",
                                        "Nothing tracked yet — try “+ Add” at the top right",
                                    )
                                } else {
                                    t("没有匹配的条目", "No matches")
                                }),
                        )
                    })
                    .children(groups.into_iter().map(|(group, rows)| {
                        let selected = selected.clone();
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .px_2()
                                    .pt_2()
                                    .pb_1()
                                    .text_size(sp(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(FAINT))
                                    .child(group_label(&group)),
                            )
                            .children(rows.into_iter().map(|i| {
                                let t = &self.tracks[i];
                                let is_sel = selected.as_deref() == Some(t.id.as_str());
                                let (sub, color) = track_status(t);
                                let t_id = t.id.clone();
                                let live = self.live_tasks(&t.id);
                                let downloading = !live.is_empty();
                                // 同时下多个文件时，表里这一条按合计进度画：它代表的
                                // 是「这一项」，不是「最后点的那一个」。
                                let agg = downloading.then(|| aggregate(&live));
                                let pop_id = t.id.clone();
                                // 已经下完的版本数：有的话悬停也要浮出一张清单，
                                // 让「这一项我下过哪几版、在哪儿」不用点进详情去看。
                                let done_n = self.done_versions(&t.name).len();
                                div()
                                    .id(SharedString::from(format!("row-{}", t.id)))
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .px_2()
                                    .py_2()
                                    .rounded_md()
                                    .when(is_sel, |d| d.bg(rgb(ACCENT_SOFT)))
                                    .when(!is_sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                                    .cursor_pointer()
                                    .child(
                                        div()
                                            .text_size(sp(14.))
                                            .font_weight(if is_sel {
                                                FontWeight::SEMIBOLD
                                            } else {
                                                FontWeight::NORMAL
                                            })
                                            .text_color(rgb(TEXT))
                                            .text_ellipsis()
                                            .child(t.name.clone()),
                                    )
                                    // 第二行平时是版本/状态文字。有下载在跑时换成一条
                                    // 细进度条 —— 列表里只需要一眼看出「这条在动」，
                                    // 已下载多少、多快放到悬停浮层里说，不然每行都要
                                    // 塞两行数字，列表会变成一块仪表盘。
                                    .when_some(agg, |d, (frac, fill)| {
                                        d.child(
                                            div()
                                                .h(px(3.))
                                                .w_full()
                                                .rounded_full()
                                                // 比侧栏底色深一档，0% 也看得见轨道
                                                .bg(rgb(0xDDE3EE))
                                                .child(
                                                    div()
                                                        .h_full()
                                                        .w(relative(frac))
                                                        .rounded_full()
                                                        .bg(rgb(fill)),
                                                ),
                                        )
                                    })
                                    .when(!downloading, |d| {
                                        d.child(
                                            div()
                                                .text_size(sp(12.))
                                                .text_color(rgb(color))
                                                .text_ellipsis()
                                                .child(sub),
                                        )
                                    })
                                    .when(downloading || done_n > 0, |d| {
                                        let ent = cx.entity();
                                        let live_id = pop_id.clone();
                                        let done_id = pop_id.clone();
                                        d.on_mouse_move(cx.listener(
                                            move |this, ev: &gpui::MouseMoveEvent, _, cx| {
                                                // 同一条上继续移动不再重设：位置一样却
                                                // 每像素通知一次，等于整窗每秒重绘几十遍。
                                                if downloading {
                                                    if this.dl_pop.as_ref().is_some_and(|p| {
                                                        p.track == live_id
                                                    }) {
                                                        return;
                                                    }
                                                    this.dl_pop = Some(DlPop {
                                                        track: live_id.clone(),
                                                        y: ev.position.y.into(),
                                                    });
                                                    // 下载中的浮层优先级更高，别让下面那份
                                                    // 同时挂在半空。
                                                    this.done_pop = None;
                                                    this.done_pop_hide_at = None;
                                                } else {
                                                    if this.done_pop.as_ref().is_some_and(|p| {
                                                        p.track == done_id
                                                    }) {
                                                        return;
                                                    }
                                                    this.done_pop = Some(DonePop {
                                                        track: done_id.clone(),
                                                        y: ev.position.y.into(),
                                                    });
                                                    this.done_pop_hide_at = None;
                                                }
                                                cx.notify();
                                            },
                                        ))
                                        // 离开这条必须靠 hover 的结束事件：`on_mouse_move`
                                        // 只在鼠标还在元素上时触发，移出去以后一次都不会
                                        // 再来，浮层就会一直挂在那儿。
                                        .on_hover(move |hovered: &bool, _, app| {
                                            if *hovered {
                                                return;
                                            }
                                            ent.update(app, |this, cx| {
                                                if this.dl_pop.is_some() {
                                                    this.dl_pop = None;
                                                    cx.notify();
                                                }
                                                // 已下载那份是**可以点的**，立刻收会让
                                                // 鼠标永远挪不进去：先给一小段宽限，等
                                                // 浮层自己收到 hover 再撤。
                                                if this.done_pop.is_some() {
                                                    this.done_pop_hide_at =
                                                        Some(Instant::now()
                                                            + Duration::from_millis(220));
                                                    cx.notify();
                                                }
                                            });
                                        })
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.select(&t_id, cx)
                                    }))
                            }))
                    })),
            )
    }

    fn main_panel(
        &self,
        text_w: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(track) = self.track().cloned() else {
            return div()
                .flex_1()
                .h_full()
                .bg(rgb(BG))
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_color(rgb(FAINT)).child(t("左侧还没有追踪项", "Nothing tracked yet")));
        };
        let view = self.view();
        let sel = *self.candidate_sel.get(&track.id).unwrap_or(&0);

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(rgb(BG))
            .child(self.detail_header(&track, view.as_ref(), cx))
            .child(
                div()
                    .id("detail-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    // 同上：flex 子项默认 min-height:auto，内容比可视区高时
                    // 它会撑高自己而不是出现滚动条。
                    .min_h_0()
                    .min_w_0()
                    .gap_3()
                    .px_5()
                    // 底部多留一点：内容被截在滚动区边界时，最后一张卡片贴着边
                    // 会让人以为「页面到这里就断了」，留白能说明它还能往下滚。
                    .pb_8()
                    .overflow_y_scroll()
                    .child(match view.as_ref() {
                        Some(r) => self.release_card(&track, r, sel, text_w, window, cx),
                        None => self.empty_card(&track),
                    })
                    .when(self.tab == Tab::Overview, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .w_full()
                                .min_w_0()
                                .gap_2()
                                .when_some(view.clone(), |d, r| {
                                    d.child(self.asset_card(&track, &r, cx))
                                }),
                        )
                    })
                    .when_some(
                        match (self.tab, view.as_ref()) {
                            (Tab::Notes, Some(r)) if !r.notes.trim().is_empty() => {
                                Some((r.notes.clone(), track.id.clone()))
                            }
                            _ => None,
                        },
                        |d, (notes, tid)| {
                            let copy = notes.clone();
                            d.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .w_full()
                                    .min_w_0()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_size(sp(12.5))
                                                    .text_color(rgb(FAINT))
                                                    .child(t("更新说明", "Release notes")),
                                            )
                                            .child(div().flex_1())
                                            // GPUI 0.2 没有文本选中，长文本得给个一键复制的出口。
                                            .child(
                                                chip(
                                                    format!("copy-notes-{tid}"),
                                                    t("复制全文", "Copy all"),
                                                    MUTED,
                                                    0xF2F4F8,
                                                )
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    let c = copy.clone();
                                                    this.copy(c, What::Notes, cx);
                                                })),
                                            ),
                                    )
                                    // 全文比概览更长，这里更该能逐段挑着复制。
                                    .child(self.selectable_body(
                                        format!("notes:{tid}"),
                                        &notes,
                                        text_w,
                                        sp(14.),
                                        window,
                                        cx,
                                    )),
                            )
                        },
                    )
                    .when_some(
                        match (self.tab, view.as_ref()) {
                            (Tab::History, Some(r)) if !r.history.is_empty() => {
                                Some(r.history.clone())
                            }
                            _ => None,
                        },
                        |d, hist| d.child(history_card(&hist)),
                    ),
            )
    }

    /// 底部队列：只在有任务时出现。
    ///
    /// 布局上它是 `render` 里根 flex 列的最后一个正常子项，**不是浮层**：
    /// 展开时主内容区跟着变矮并出现滚动条，收起来只剩一行标题。
    /// 之前这里无上限地长高，把页面下半截挤出去，看着就像被盖住了。
    fn queue_panel(&self, vh: f32, cx: &mut Context<Self>) -> AnyElement {
        // 面板本身不再自己决定开合：开关在顶栏那颗「下载队列」上，这里只管画。
        // 收起时返回空元素，底部一行都不占。
        if self.tasks.is_empty() || !self.queue_show {
            return div().into_any_element();
        }
        let running = self.active_count();
        let finished = self
            .tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Done | TaskStatus::Failed | TaskStatus::Canceled
                )
            })
            .count();

        // 标题行固定一行高：收起状态下整个队列就只占这一行，
        // 不让它跟着任务数量一起往上长。
        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .pl_5()
            .pr_3()
            .h(sp(42.))
            .child(
                div()
                    .text_size(sp(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT))
                    .child(t("下载队列", "Downloads")),
            )
            // 全部结束时不再显示「0 进行中」——那一行只是噪音。
            .when(running > 0, |d| {
                d.child(badge(
                    tf!("{} 进行中", "{} active", running),
                    ACCENT,
                    ACCENT_SOFT,
                ))
            })
            .when(running == 0 && finished > 0, |d| {
                d.child(
                    div()
                        .text_size(sp(12.))
                        .text_color(rgb(MUTED))
                        .child(tf!("{} 项已结束", "{} finished", finished)),
                )
            })
            .child(div().flex_1())
            .when(finished > 0, |d| {
                d.child(
                    chip("btn-clear", t("清空已结束", "Clear finished"), MUTED, 0xF2F4F8)
                        .on_click(cx.listener(|this, _, _, cx| this.clear_finished(cx))),
                )
            })
            // 顶栏那颗按钮已经能收起，这里再留一个同样的出口：面板摊在最底下时
            // 手边的「收起」比回到顶栏去找按钮少一次视线转移。
            .child(
                chip("queue-toggle", t("收起", "Hide"), MUTED, 0xF2F4F8).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.queue_show = false;
                        cx.notify();
                    }),
                ),
            );

        div()
            .flex()
            .flex_col()
            .bg(rgb(CARD))
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(head)
            .child(
                div()
                    .id("queue-list")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_5()
                    .pb_3()
                    // 最多占窗口三成高：再多主内容就只剩一条缝了，其余的
                    // 在这里自己滚。没有这条上限时任务一多会把详情页顶出窗口。
                    .max_h(px((vh * 0.30).max(110.)))
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(self.tasks.iter().take(8).map(|task| {
                        let id = task.spec.id.clone();
                let id2 = id.clone();
                let pause_label = if task.status == TaskStatus::Running {
                    crate::i18n::t("暂停", "Pause")
                } else if task.status == TaskStatus::Paused {
                    crate::i18n::t("继续", "Resume")
                } else {
                    ""
                };
                let finished = matches!(
                    task.status,
                    TaskStatus::Done | TaskStatus::Failed | TaskStatus::Canceled
                );
                let detail = match task.status {
                    TaskStatus::Running => format!(
                        "{} / {} · {}",
                        util::fmt_bytes(task.downloaded),
                        util::fmt_bytes(task.total),
                        util::fmt_speed(task.speed)
                    ),
                    TaskStatus::Done => format!(
                        "{} · {}",
                        util::fmt_bytes(task.downloaded),
                        match task.sha_ok {
                            Some(true) => t("SHA-256 校验通过", "SHA-256 verified"),
                            Some(false) => t("SHA-256 不一致", "SHA-256 mismatch"),
                            None => t("无官方摘要", "no checksum"),
                        }
                    ),
                    TaskStatus::Failed => task
                        .error
                        .clone()
                        .unwrap_or_else(|| t("下载失败", "download failed").into()),
                    TaskStatus::Paused => tf!(
                        "已暂停 · {} / {}",
                        "paused · {} / {}",
                        util::fmt_bytes(task.downloaded),
                        util::fmt_bytes(task.total)
                    ),
                    _ => String::new(),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(sp(13.))
                                    .text_color(rgb(TEXT))
                                    .text_ellipsis()
                                    .min_w_0()
                                    .flex_1()
                                    .child(format!("{} · {}", task.spec.app, task.spec.file)),
                            )
                            // 大小 / 速度 / 校验结果挤在同一行：队列是底部的附属信息，
                            // 每项占两行会把主内容区压得只剩一条缝。
                            .when(!detail.is_empty(), |d| {
                                d.child(
                                    div()
                                        .max_w(sp(330.))
                                        .text_size(sp(12.))
                                        .text_color(rgb(MUTED))
                                        .text_ellipsis()
                                        .flex_shrink_0()
                                        .child(detail),
                                )
                            })
                            .child(
                                div()
                                    .text_size(sp(12.))
                                    .text_color(rgb(status_color(task.status)))
                                    .flex_shrink_0()
                                    .child(task.status.label()),
                            )
                            .when(!finished && !pause_label.is_empty(), |d| {
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("pause-{id}")))
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .bg(rgb(0xF2F4F8))
                                        .text_size(sp(12.))
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .child(pause_label)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_pause(&id, cx)
                                        })),
                                )
                            })
                            .when(task.status == TaskStatus::Done, |d| {
                                // 完成的给「打开」：直接交给系统默认程序，比让用户
                                // 自己去找目录再双击快。
                                let Some(path) = task.path.clone() else {
                                    return d;
                                };
                                let dir = std::path::Path::new(&path)
                                    .parent()
                                    .map(|p| p.to_string_lossy().to_string());
                                let p1 = path.clone();
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("open-{id2}")))
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .bg(rgb(0xF2F4F8))
                                        .text_size(sp(12.))
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .child(t("打开", "Open"))
                                        .on_click(cx.listener(move |_, _, _, _| {
                                            let _ = open::that_detached(&p1);
                                        })),
                                )
                                .when_some(dir, |d, dir| {
                                    let d2 = dir;
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("dir-{id2}")))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .bg(rgb(0xF2F4F8))
                                            .text_size(sp(12.))
                                            .text_color(rgb(TEXT))
                                            .cursor_pointer()
                                            .child(t("目录", "Folder"))
                                            .on_click(cx.listener(move |_, _, _, _| {
                                                let _ = open::that_detached(&d2);
                                            })),
                                    )
                                })
                            })
                            .when(
                                matches!(task.status, TaskStatus::Failed | TaskStatus::Canceled),
                                |d| {
                                    let id3 = task.spec.id.clone();
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("retry-{id3}")))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .bg(rgb(ACCENT_SOFT))
                                            .text_size(sp(12.))
                                            .text_color(rgb(ACCENT))
                                            .cursor_pointer()
                                            .child(t("重试", "Retry"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.retry_task(&id3, cx)
                                            })),
                                    )
                                },
                            )
                            .when(!finished, |d| {
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("cancel-{id2}")))
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .bg(rgb(RED_SOFT))
                                        .text_size(sp(12.))
                                        .text_color(rgb(RED))
                                        .cursor_pointer()
                                        .child(t("取消", "Cancel"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel_task(&id2, cx)
                                        })),
                                )
                            }),
                    )
                    // 进度条只在进行中/暂停时有意义。已结束的任务再横一条满格
                    // 绿条，视觉上比信息本身还响，反而抢注意力。
                    .when(!finished, |d| {
                        d.child(
                            div()
                                .h(px(2.))
                                .w_full()
                                .rounded_full()
                                // 比页面底色深一档，0% 时也能看出这是条进度轨道
                                .bg(rgb(0xDDE3EE))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(task.frac()))
                                        .rounded_full()
                                        .bg(rgb(status_color(task.status))),
                                ),
                        )
                    })
                        }))
                )
            .into_any_element()
    }

    fn detail_header(
        &self,
        track: &Track,
        view: Option<&Release>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tab = self.tab;
        div()
            .flex()
            .flex_col()
            .gap_3()
            .px_5()
            .pt_4()
            .pb_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(sp(21.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(track.name.clone()),
                    )
                    .child(badge(group_label(&track.group), MUTED, 0xF2F4F8))
                    .when(track.kind != crate::model::SourceKind::GitHub, |d| {
                        // GitHub 分组和来源名一样，重复显示一个 badge 只是噪音。
                        d.child(badge(track.kind.label(), MUTED, 0xF2F4F8))
                    })
                    .when_some(track.last_checked, |d, ts| {
                        d.child(
                            div()
                                .text_size(sp(12.))
                                .text_color(rgb(FAINT))
                                .child(tf!(
                                    "上次检查 {}",
                                    "checked {}",
                                    util::fmt_secs_ago(ts)
                                )),
                        )
                    })
                    .when_some(track.last_error.clone(), |d, err| {
                        d.child(badge(t("解析失败", "Failed"), RED, RED_SOFT))
                            .child(
                                div()
                                    .text_size(sp(12.5))
                                    .text_color(rgb(RED))
                                    .text_ellipsis()
                                    .child(err),
                            )
                    })
                    .child(div().flex_1())
                    .when(self.resolving.contains(&track.id), |d| {
                        d.child(
                            div()
                                .text_size(sp(12.5))
                                .text_color(rgb(ACCENT))
                                .child(t("检查中…", "Checking…")),
                        )
                    })
                    .when_some(view.map(|r| r.version.clone()), |d, v| {
                        d.child(
                            div()
                                .text_size(sp(13.5))
                                .text_color(rgb(MUTED))
                                .child(ver_label(&v)),
                        )
                    })
                    .when(track.has_update(), |d| {
                        // 「有新版本」这件事本身就该有个收尾动作：没有它，橙色的
                        // 提示只能等下一次真的出了新版本才变化，用户没法把已看过
                        // 的版本清掉。
                        let id = track.id.clone();
                        d.child(
                            btn("btn-mark-read", t("标为已读", "Mark read"), ORANGE_SOFT, ORANGE)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mark_read(&id, cx);
                                })),
                        )
                    })
                    .child({
                        let id = track.id.clone();
                        btn("btn-recheck", t("重新检查", "Refresh"), 0xF2F4F8, TEXT).on_click(cx.listener(
                            move |this, _, _, cx| this.resolve_one(&id, cx),
                        ))
                    })
                    .child({
                        let id = track.id.clone();
                        if self.confirm_delete.as_deref() == Some(track.id.as_str()) {
                            // 二次确认放在原地：弹一个确认对话框对移除一行来说太重。
                            btn("btn-del-yes", t("确认删除", "Confirm"), RED_SOFT, RED).on_click(cx.listener(
                                move |this, _, _, cx| this.delete_track(&id, cx),
                            ))
                        } else {
                            btn("btn-del", t("删除", "Delete"), RED_SOFT, RED).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.confirm_delete = Some(id.clone());
                                    cx.notify();
                                },
                            ))
                        }
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .children([Tab::Overview, Tab::Notes, Tab::History].into_iter().map(|t| {
                        let active = tab == t;
                        div()
                            .id(SharedString::from(format!("tab-{}", t.label())))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .text_size(sp(13.5))
                            .when(active, |d| {
                                d.bg(rgb(CARD))
                                    .text_color(rgb(TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                            })
                            .when(!active, |d| {
                                d.text_color(rgb(MUTED))
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .cursor_pointer()
                            })
                            .child(t.label())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tab = t;
                                cx.notify();
                            }))
                    })),
            )
    }

    fn empty_card(&self, track: &Track) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .gap_2()
            .p_5()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_size(sp(14.5))
                    .text_color(rgb(MUTED))
                    .child(t("还没有解析结果", "No release resolved yet")),
            )
            .child(
                div()
                    .text_size(sp(13.))
                    .text_color(rgb(FAINT))
                    .whitespace_normal()
                    .child(tf!("来源：{}", "Source: {}", track.url)),
            )
            .into_any_element()
    }

    fn release_card(
        &self,
        track: &Track,
        r: &Release,
        sel: usize,
        text_w: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let url = r.page_url.clone();
        let copy_url = r.page_url.clone();
        let ver_text = ver_label(&r.version);
        let published = r
            .published
            .clone()
            .unwrap_or_else(|| t("未知", "unknown").to_string());
        div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .gap_3()
            .p_5()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .min_w_0()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .gap_1()
                            .flex_1()
                            .child(
                                div()
                                    .text_size(sp(28.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(ver_text.clone()),
                            )
                            .child(
                                div()
                                    .text_size(sp(13.))
                                    .text_color(rgb(MUTED))
                                    .text_ellipsis()
                                    .child(tf!(
                                        "{} · 发布于 {} · 来源 {}",
                                        "{} · published {} · {}",
                                        r.title,
                                        published,
                                        r.provider
                                    )),
                            ),
                    )
                    .when(sel > 0, |d| {
                        let cand = r.candidates.get(sel);
                        // 滚动构建（Nightly / Snapshot）直接报频道名，比笼统的「预览版」
                        // 更能说明「这条每天都变、装它是有代价的」。
                        let label = match cand.and_then(|c| c.channel()) {
                            Some(ch) => capitalize(&ch),
                            None => match cand.map(|c| c.prerelease).unwrap_or(false) {
                                true => t("预览版", "Preview").to_string(),
                                false => t("备选版本", "Alternative").to_string(),
                            },
                        };
                        d.child(badge(label, ORANGE, ORANGE_SOFT))
                    })
                    .when(sel == 0 && track.has_update(), |d| {
                        d.child(badge(t("发现新版本", "New version"), ORANGE, ORANGE_SOFT))
                    })
                    .child(
                        chip(
                            format!("copy-ver-{}", track.id),
                            t("复制版本", "Copy version"),
                            MUTED,
                            0xF2F4F8,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy(ver_text.clone(), What::Version, cx);
                        })),
                    )
                    .child(
                        chip(
                            format!("copy-page-{}", track.id),
                            t("复制链接", "Copy link"),
                            MUTED,
                            0xF2F4F8,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy(copy_url.clone(), What::PageLink, cx);
                        })),
                    )
                    .child(
                        btn("btn-open-page", t("打开来源页", "Open page"), 0xF2F4F8, TEXT).on_click(cx.listener(
                            move |this, _, _, cx| {
                                let _ = open::that_detached(&url);
                                this.toast(
                                    t(
                                        "已在浏览器打开来源页",
                                        "Opened the source page in your browser",
                                    ),
                                    cx,
                                );
                            },
                        )),
                    ),
            )
            // 概览页只放摘要：完整正文在「更新说明」页签，否则光说明就把附件
            // 顶到屏幕外了（nyaterm 的 release body 有几十行）。到「更新说明」
            // 页签里正文由下面的卡片承担，这里就不再重复一遍。
            .when(!r.summary.trim().is_empty() && self.tab != Tab::Notes, |d| {
                let compact = self.tab == Tab::Overview;
                let full: String = r.summary.clone();
                let shown = if compact { clip(&full, 260) } else { full.clone() };
                let tid = track.id.clone();
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        // 这段是概览的主体，也是唯一值得逐字复制的部分 —— 做成
                        // 可拖选。给确定宽度，否则 taffy 会用 min-content 宽度量高度：
                        // 换行看着是对的，但卡片底部会多出一大块空白。
                        .child(self.selectable_body(
                            format!("summary:{}:{sel}", track.id),
                            &shown,
                            text_w,
                            sp(14.),
                            window,
                            cx,
                        ))
                        .when(compact && full.chars().count() > 260, |d| {
                            // 放在 flex_row 里，否则列方向的 stretch 会把这颗胶囊拉成整行。
                            d.child(
                                div().flex().flex_row().child(
                                    chip(
                                        format!("more-{tid}"),
                                        tf!(
                                            "查看完整说明（共 {} 字）",
                                            "Read the full notes ({} chars)",
                                            full.chars().count()
                                        ),
                                        ACCENT,
                                        ACCENT_SOFT,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.tab = Tab::Notes;
                                        cx.notify();
                                    })),
                                ),
                            )
                        }),
                )
            })
            .children((r.candidates.len() > 1).then(|| {
                let mut row = div().flex().flex_row().gap_2().items_center().child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(rgb(FAINT))
                        .child(t("版本候选", "Candidates")),
                );
                for (i, c) in r.candidates.iter().enumerate() {
                    let active = i == sel;
                    let id = track.id.clone();
                    row = row.child(
                        div()
                            .id(SharedString::from(format!("cand-{i}")))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(sp(12.5))
                            .when(c.prerelease, |d| d.text_color(rgb(ORANGE)))
                            .when(active, |d| d.bg(rgb(ACCENT_SOFT)))
                            .when(!active, |d| d.hover(|s| s.bg(rgb(HOVER))).cursor_pointer())
                            .child(c.tab_label())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.candidate_sel.insert(id.clone(), i);
                                cx.notify();
                            })),
                    );
                }
                row
            }))
            .into_any_element()
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 事件回调里拿不到 `&mut Window`，聚焦统一推迟到这一帧来做。
        if let Some(field) = self.pending_focus.take() {
            let focus = self.field(field).focus.clone();
            focus.focus(window);
        } else if std::mem::take(&mut self.refocus_root) {
            // 关掉弹窗后焦点会悬空（元素已经不绘制了），快捷键随之失效，交还给
            // 根节点。**只在刚关弹窗的那一刻做**：早先这里写的是「根节点没有焦点
            // 就抢回来」，而任何输入框聚焦时根节点本来就没有焦点 —— 于是主界面的
            // 筛选框敲一个字就会被抢走焦点，第二下再也进不去。
            self.root_focus.focus(window);
        }
        // 窗口标题跟着语言走。`&mut Window` 只有绘制阶段拿得到，所以按需设一次，
        // 而不是每帧都去打扰系统。
        let lang = i18n::current();
        if self.title_lang != Some(lang) {
            window.set_window_title(&format!(
                "App Monitor · {}",
                t("软件追踪下载器", "release tracker")
            ));
            self.title_lang = Some(lang);
        }
        // 正文文本按确定宽度换行：taffy 给自动宽度的换行文本量高度时用的是
        // min-content 宽度（最长单词的宽度），于是卡片下半截会多出几百像素空白。
        // 这里按窗口宽度减去侧栏、内边距与边框算一个安全宽度传下去；370 是侧栏
        // 272 加上两侧内边距与卡片内边距，缩放时整块一起缩。
        let vw: f32 = window.viewport_size().width.into();
        let vh: f32 = window.viewport_size().height.into();
        let text_width = px((vw - 370. * scale()).max(240.));
        let font = ui_font();
        let toast = self.toast.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .font_family(font)
            .text_color(rgb(TEXT))
            .track_focus(&self.root_focus)
            // AM_LOG_MOUSE=1：把鼠标事件与缩放因子打到 stderr，排查「点了没反应」时用。
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(
                |_, ev: &gpui::MouseDownEvent, window, _| {
                    if std::env::var_os("AM_LOG_MOUSE").is_some() {
                        eprintln!(
                            "[mouse] down at {:?} scale {}",
                            ev.position,
                            window.scale_factor()
                        );
                    }
                },
            ))
            // 拖选的兜底：输入框内部由那颗输入框自己处理，鼠标一旦拖出框外就只剩
            // 这里能收到 —— 否则选区会卡在边框上，用户往右拖多远都不再变长。
            .when(self.field_drag.is_some(), |d| {
                d.on_mouse_move(cx.listener(
                    |this, ev: &gpui::MouseMoveEvent, window, cx| {
                        if let Some(id) = this.field_drag {
                            this.drag_field_to(id, ev.position, window, cx);
                        }
                    },
                ))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.end_field_drag(cx)),
                )
            })
            // 快捷键的 action 监听必须挂在元素上：`Window::on_action` 只能在
            // dispatch 树构建期间注册，窗口刚建好时注册会 panic。
            .on_action(cx.listener(|this, _: &AddTrack, _, cx| {
                this.open_dialog(Dialog::Add, cx);
                this.focus_field(FieldId::AddInput, cx);
            }))
            .on_action(
                cx.listener(|this, _: &OpenSettings, _, cx| {
                    this.open_dialog(Dialog::Settings, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &OpenHistory, _, cx| {
                    this.open_dialog(Dialog::History, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &TabOverview, _, cx| {
                this.tab = Tab::Overview;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TabNotes, _, cx| {
                this.tab = Tab::Notes;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TabVersions, _, cx| {
                this.tab = Tab::History;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FocusFilter, _, cx| {
                this.focus_field(FieldId::Filter, cx)
            }))
            .children((!self.issues.is_empty()).then(|| {
                let small = |id: &'static str, text: &'static str| {
                    div()
                        .id(id)
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(0xFFFFFF))
                        .text_size(sp(12.))
                        .text_color(rgb(TEXT))
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.85))
                        .child(text)
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_5()
                    .py_2()
                    .bg(rgb(ORANGE_SOFT))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(rgb(ORANGE))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("!"),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(sp(13.))
                                    .text_color(rgb(TEXT))
                                    .child(tf!(
                                        "{} 个数据文件未能读取，本次已改用默认内容",
                                        "{} data file(s) could not be read — this session fell back to defaults",
                                        self.issues.len()
                                    )),
                            )
                            .child(
                                small("issue-open", t("打开数据目录", "Open data folder")).on_click(cx.listener(
                                    |_, _, _, _| {
                                        let dir = crate::store::data_dir();
                                        let _ = std::fs::create_dir_all(dir);
                                        let _ = open::that_detached(dir);
                                    },
                                )),
                            )
                            .child(
                                small("issue-dismiss", t("知道了", "Got it")).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.issues.clear();
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                    .children(self.issues.iter().map(|i| {
                        let kept = match &i.backup {
                            Some(p) => {
                                let name = p
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| p.to_string_lossy().to_string());
                                tf!("原文件已备份为 {}", "the original was kept as {}", name)
                            }
                            // 备份失败代表原文件仍在原地，程序这次不会覆盖它。
                            None => t(
                                "原文件保持不动",
                                "the original file was left untouched",
                            )
                            .to_string(),
                        };
                        let what = damage_text(&i.what);
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(MUTED))
                            .whitespace_normal()
                            .child(tf!(
                                "· {} {}，本次使用{}。{}",
                                "· {} — {}, fell back to {}. {}",
                                i.file,
                                what,
                                fallback_text(i.fallback),
                                kept
                            ))
                    }))
            }))
            .child(self.top_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    // 没有 min_h_0 时这一行按内容撑高：内容一长就顶出窗口，
                    // 后面兄弟节点（队列、消息条）会画在它溢出的部分之上，
                    // 看起来就像「被盖住了」。给定 0 下界它才会老实收缩。
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.sidebar(cx))
                    .child(self.main_panel(text_width, window, cx)),
            )
            .child(self.queue_panel(vh, cx))
            // 悬停详情浮层：不参与布局，压在正文之上、弹窗之下。
            .child(self.dl_pop_layer(vw, vh))
            // 已下载版本的清单：同样悬空，但它可以点，所以压在弹窗之下、正文之上。
            .child(self.done_pop_layer(vw, vh, cx))
            // toast 是**浮层**：`absolute` 不参与 flex 布局，出现和消失都不会
            // 让页面跳一下，也不会把底部队列往上顶。放顶部居中是刻意的——
            // 底部已经被队列占着，右下角还会压住「暂停 / 取消」这些按钮。
            // 它排在弹窗之前，所以弹窗仍然盖在它上面。
            .children(toast.filter(|(_, at)| at.elapsed().as_secs() < 4).map(|(msg, _)| {
                div()
                    .absolute()
                    .top(sp(140.))
                    .right(sp(20.))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(
                        div()
                            .max_w(sp(420.))
                            .px_4()
                            .py_2()
                            .rounded_lg()
                            .bg(gpui::rgba(0x111827F2))
                            .border_1()
                            .border_color(gpui::rgba(0xFFFFFF1F))
                            .text_size(sp(13.))
                            .text_color(rgb(0xFFFFFF))
                            .child(msg),
                    )
            }))
            .child(self.dialog_layer(px((vh * 0.9).max(320.)), cx))
            // 输入框的右键菜单：它由弹窗里的某颗输入框拉起，必须压在弹窗之上，
            // 否则菜单会被半透明的遮罩压暗、还挡不住下面那颗按钮。
            .child(self.field_ctx_layer(vw, vh, cx))
            // 菜单压在所有东西之上：它是「此刻唯一可点的东西」，被弹窗或 toast
            // 挡住的话用户会以为右键没生效。
            .child(self.ctx_layer(vw, vh, cx))
            // 语言菜单压在最上面：它由顶栏或设置里的那颗下拉按钮拉起，两者都
            // 可能被弹窗盖着，菜单再被盖住就成了「点了没反应」。
            .child(self.lang_menu_layer(vw, vh, cx))
    }
}

// ---------------------------------------------------------------------------
// 小部件
// ---------------------------------------------------------------------------

/// 损坏文件横幅里的「原因」。
///
/// 中文与 `Display` 的措辞保持一致（命令行 `--check` 打的是那一套），但不再直接
/// 借用 `Display`：日韩也得有说法，而 `Display` 只有中文一种。
fn damage_text(d: &crate::store::Damage) -> String {
    use crate::store::Damage;
    match d {
        Damage::Io(e) => tf!("读取失败：{}", "it could not be read: {}", e),
        Damage::Empty => t(
            "文件是空的，可能是上次写入被中断",
            "the file was empty, most likely an interrupted write",
        )
        .to_string(),
        Damage::BadJson(e) => tf!(
            "内容不是合法的 JSON（{}）",
            "its content is not valid JSON ({})",
            e
        ),
    }
}

/// 出问题后这次会话改用了什么。
fn fallback_text(f: crate::store::Fallback) -> String {
    use crate::store::Fallback;
    match f {
        Fallback::DefaultConfig => t("默认设置", "default settings").to_string(),
        Fallback::DefaultTracks => {
            t("内置的默认追踪清单", "the built-in list").to_string()
        }
        Fallback::EmptyHistory => t("空历史", "an empty history").to_string(),
    }
}

/// 界面字体：每个语言用自己最地道的系统字体。
///
/// 中文继续用雅黑（字形与字重按它在调），英文 Segoe UI，日文 Yu Gothic UI，
/// 韩文 Malgun Gothic —— 这三个都是 Win10 起自带的，不用打包字体文件。
/// 其它平台沿用原来那一份：指定了不存在的字体时渲染层会回退到系统默认。
fn ui_font() -> &'static str {
    if !cfg!(windows) {
        return "Microsoft YaHei UI";
    }
    match i18n::current() {
        i18n::Lang::Zh => "Microsoft YaHei UI",
        i18n::Lang::En => "Segoe UI",
        i18n::Lang::Ja => "Yu Gothic UI",
        i18n::Lang::Ko => "Malgun Gothic",
    }
}

fn track_status(track: &Track) -> (String, u32) {
    if let Some(err) = &track.last_error {
        let short: String = err.chars().take(18).collect();
        return (tf!("解析失败 · {}", "failed · {}", short), RED);
    }
    if let Some(r) = &track.release {
        if track.has_update() {
            return (tf!("↑ 新版本 {}", "↑ new {}", r.version), ORANGE);
        }
        return (ver_label(&r.version), GREEN);
    }
    (t("等待首次检查", "waiting for first check").to_string(), FAINT)
}

/// 版本号前面那个 `v`：只有数字开头的才加。`nightly 20261031` 前面挂个 v 会变成
/// 「vnightly」，而它根本不是版本号。
fn ver_label(v: &str) -> String {
    let v = v.trim();
    match v.chars().next() {
        Some(c) if c.is_ascii_digit() => format!("v{v}"),
        _ => v.to_string(),
    }
}

/// `nightly` → `Nightly`：频道名在数据里是小写标识符，直接贴上标签不够体面。
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn badge(text: impl Into<SharedString>, fg: u32, bg: u32) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(bg))
        .text_size(sp(12.))
        .text_color(rgb(fg))
        .child(text.into())
}

/// 按配置建 HTTP 客户端。代理写错时**不**静默退回直连：返回错误让调用方
/// 提示用户，否则「明明配了代理却没走代理」这种问题查起来很费时间。
fn make_http(cfg: &Config) -> (Arc<Http>, Option<String>) {
    let token = if cfg.github_token.trim().is_empty() {
        None
    } else {
        Some(cfg.github_token.trim().to_string())
    };
    match Http::new(&cfg.proxy, token.clone()) {
        Ok(h) => (Arc::new(h), None),
        Err(e) => match Http::new("", token) {
            Ok(h) => (Arc::new(h), Some(e)),
            // 连直连都建不起来（TLS 后端初始化失败），此时只能用默认配置硬撑。
            Err(_) => (
                Arc::new(Http::new("", None).expect("http client")),
                Some(e),
            ),
        },
    }
}

/// 复选式开关：调用方只负责翻转状态，重绘由这里统一触发。
fn toggle(
    id: &'static str,
    text: &'static str,
    on: bool,
    cx: &mut Context<App>,
    apply: fn(&mut App),
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(if on { ACCENT_SOFT } else { 0xF2F4F8 }))
        .cursor_pointer()
        .child(
            div()
                .w(sp(12.))
                .h(sp(12.))
                .rounded_sm()
                .bg(rgb(if on { ACCENT } else { 0xFFFFFF }))
                .border_1()
                .border_color(rgb(if on { ACCENT } else { BORDER })),
        )
        .child(div().text_size(sp(12.5)).text_color(rgb(TEXT)).child(text))
        .on_click(cx.listener(move |this, _, _, cx| {
            apply(this);
            cx.notify();
        }))
}

fn btn(id: &'static str, text: &'static str, bg: u32, fg: u32) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(bg))
        .text_size(sp(13.))
        .text_color(rgb(fg))
        .hover(|s| s.opacity(0.88))
        .cursor_pointer()
        .child(text)
}

/// 按字符（不是字节）截断，超长补省略号。
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 次要动作的小胶囊（复制、打开链接、标签切换这类）。id 需要动态拼接，所以
/// 不接受 `&'static str` —— 附件行的 id 里带文件名。
fn chip(
    id: impl Into<SharedString>,
    text: impl Into<SharedString>,
    fg: u32,
    bg: u32,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(bg))
        .text_size(sp(12.))
        .text_color(rgb(fg))
        .hover(|s| s.opacity(0.85))
        .cursor_pointer()
        .child(text.into())
}

impl App {
    // -- 输入框 ------------------------------------------------------------

    /// 单行输入框。`placeholder` 仅在空且未聚焦时显示，避免光标处叠字。
    fn text_input(
        &self,
        field: FieldId,
        placeholder: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tf = self.field(field);
        let focused = self.active_field == Some(field);
        let empty = tf.text().is_empty();
        let (before, after) = tf.split();
        let mask = |s: &str| {
            if tf.secret {
                "•".repeat(s.chars().count())
            } else {
                s.to_string()
            }
        };
        let body = if empty && !focused {
            placeholder.to_string()
        } else if focused {
            // 用竖线标出插入点：GPUI 0.2 的自绘光标要 Window 句柄，这里够用。
            format!("{}|{}", mask(before), mask(after))
        } else {
            format!("{}{}", mask(before), mask(after))
        };
        let color = if empty && !focused { FAINT } else { TEXT };
        let height = if tf.multiline { sp(78.) } else { sp(30.) };
        let focus = tf.focus.clone();
        // 输入框没有拖选，选区只可能来自 Ctrl+A 或右键「全选」，所以要么是整段、
        // 要么没有。Token 用圆点渲染，字节偏移换算不过去，索性不画高亮。
        let selection = tf
            .edit
            .selection
            .filter(|_| focused && !tf.secret)
            .map(|(a, b)| (a.min(b), a.max(b)))
            .filter(|(a, b)| a < b);
        // 光标那个竖线插在 `cursor` 处、自己占一个字节：落在它之后的偏移都要往后
        // 挪一位，否则高亮会从中间断开一格。
        let cursor = tf.edit.cursor.min(tf.text().len());
        let total = body.len();
        let highlight = selection
            .map(|(a, b)| {
                let map = |o: usize| if o > cursor { o + 1 } else { o };
                (map(a), map(b))
            })
            .filter(|(a, b)| a < b && *b <= total);
        let content: AnyElement = match highlight {
            Some((a, b)) => {
                let font = gpui::Font {
                    family: SharedString::from(ui_font().to_string()),
                    features: gpui::FontFeatures::default(),
                    fallbacks: None,
                    weight: FontWeight::NORMAL,
                    style: gpui::FontStyle::Normal,
                };
                let run = |len: usize, bg: Option<gpui::Hsla>| gpui::TextRun {
                    len,
                    font: font.clone(),
                    color: rgb(TEXT).into(),
                    background_color: bg,
                    underline: None,
                    strikethrough: None,
                };
                let mut runs = Vec::with_capacity(3);
                if a > 0 {
                    runs.push(run(a, None));
                }
                // 与正文选区同一个口径（主色 25%）：浅底上够看清，又不会把字糊住。
                runs.push(run(b - a, Some(gpui::rgba(0x2563EB40).into())));
                if b < total {
                    runs.push(run(total - b, None));
                }
                gpui::StyledText::new(body)
                    .with_runs(runs)
                    .into_any_element()
            }
            None => div().child(body).into_any_element(),
        };

        let geom = self.geom(field);
        let cell = geom.clone();

        // 外面再包一层只为量到里面那个 div 的矩形：`on_children_prepainted` 给的
        // 正是子元素的 bounds，比在框里塞 canvas 干净（canvas 会参与布局）。
        div().on_children_prepainted(move |bounds, _, _| {
            if let Some(b) = bounds.first() {
                *cell.borrow_mut() = Some(*b);
            }
        })
        .child(
        div()
            .id(SharedString::from(format!("input-{field:?}")))
            .w_full()
            .h(height)
            .px_2()
            .py_2()
            .rounded_md()
            .bg(rgb(0xFFFFFF))
            .border_1()
            .border_color(rgb(if focused { ACCENT } else { BORDER }))
            .text_size(sp(13.))
            .text_color(rgb(color))
            .whitespace_normal()
            .overflow_hidden()
            .cursor_text()
            .track_focus(&focus)
            .child(content)
            .on_click(cx.listener(move |this, _, _, cx| this.focus_field(field, cx)))
            // 左键按下即定位光标，并从这里开始一段拖选。选区是手画的：GPUI 0.2.2
            // 的文本元素不认拖选，但排版结果能把 x 换回字节偏移（见 field_index_at）。
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.focus_field(field, cx);
                    this.start_field_drag(field, ev.position, window, cx);
                }),
            )
            // 拖到框外也要接着选：根节点上还有一份同样的处理（见 render），
            // 这里管框内，那边管框外。
            .on_mouse_move(cx.listener(
                move |this, ev: &gpui::MouseMoveEvent, window, cx| {
                    this.drag_field_to(field, ev.position, window, cx);
                },
            ))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.end_field_drag(cx)),
            )
            // 右键菜单：剪切 / 复制 / 粘贴 / 全选。事件停在这里，否则会冒泡给
            // 弹窗底下那层，被当成「点别处」处理掉。
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.focus_field(field, cx);
                    this.open_field_ctx(field, ev.position.x.into(), ev.position.y.into(), cx);
                }),
            )
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                this.on_field_key(field, ev, cx)
            })),
        )
    }

    // -- 弹窗 --------------------------------------------------------------

    /// 遮罩 + 白卡片 + 标题行。点遮罩不关闭（避免误触丢掉刚粘的地址）。
    fn modal_shell(
        &self,
        title: &'static str,
        width: f32,
        max_h: Pixels,
        body: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x0F172A73))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(width))
                    // 高度跟着窗口走而不是钉死 600：设置项多了一屏放不下，
                    // 配合下面的滚动条才不会把底部的「保存」裁掉。
                    .max_h(max_h)
                    // flex 子项默认 min-height:auto，内容高过 max_h 时它会顶着
                    // 卡片继续长高（max-height 形同虚设），必须显式允许它缩小。
                    .min_h_0()
                    .rounded_lg()
                    .bg(rgb(CARD))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .px_5()
                            .py_3()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(sp(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(title),
                            )
                            .child(
                                div()
                                    .id("dialog-close")
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_size(sp(13.))
                                    .text_color(rgb(MUTED))
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .cursor_pointer()
                                    .child("✕")
                                    .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                            ),
                    )
                    .child(
                        div()
                            .id("modal-body")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            // 卡片的 max_h 只把外框裁短，里面的 flex 布局拿不到
                            // 「有界的高度」，内容会顶出去被 overflow_hidden 切掉。
                            // 给内容自己再来一次限高，滚动条才会真的出现。
                            .max_h(max_h - sp(56.))
                            .overflow_y_scroll()
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    fn add_dialog(&self, max_h: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let hint = t(
            "每行一个地址，支持 GitHub 仓库 / releases 页、软件站页面、系统镜像页；\
             也可以直接贴一段含网址的文字，会自动挑出其中的链接。",
            "One URL per line — GitHub repos and release pages, download-site pages and OS \
             image pages all work. Pasting a paragraph that contains links works too.",
        );
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_size(sp(13.))
                    .text_color(rgb(MUTED))
                    .whitespace_normal()
                    .child(hint),
            )
            .child(self.text_input(FieldId::AddInput, "https://github.com/owner/repo", cx))
            .when(!self.add_skipped.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(ORANGE_SOFT))
                        .child(
                            div()
                                .text_size(sp(12.5))
                                .text_color(rgb(ORANGE))
                                .child(tf!(
                                    "有 {} 行没能识别（已忽略）：",
                                    "{} line(s) could not be parsed (ignored):",
                                    self.add_skipped.len()
                                )),
                        )
                        .children(self.add_skipped.iter().take(4).map(|s| {
                            div()
                                .text_size(sp(12.))
                                .text_color(rgb(MUTED))
                                .text_ellipsis()
                                .child(s.clone())
                        })),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        btn("btn-add-ok", t("解析并添加", "Add"), ACCENT, 0xFFFFFF)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_add(cx))),
                    )
                    .child(
                        btn("btn-add-cancel", t("取消", "Cancel"), 0xF2F4F8, TEXT)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(FAINT))
                            .child(t("Ctrl+V 粘贴 · Esc 关闭", "Ctrl+V to paste · Esc to close")),
                    ),
            );
        self.modal_shell(
            t("添加追踪项", "Add tracked items"),
            620.,
            max_h,
            body.into_any_element(),
            cx,
        )
    }

    fn settings_dialog(&self, max_h: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let label = |text: &'static str| {
            div()
                .text_size(sp(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(MUTED))
                .child(text)
        };
        // 小方按钮：加减计数都靠它，避免再引入一个输入框。
        let step_btn = |id: &'static str, text: &'static str| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .bg(rgb(0xF2F4F8))
                .text_size(sp(14.5))
                .text_color(rgb(TEXT))
                .hover(|s| s.bg(rgb(HOVER)))
                .cursor_pointer()
                .child(text)
        };
        let conc = self.cfg.max_concurrent;
        let interval = self.cfg.check_interval_min;
        // 显示生效值而不是配置值：`AM_UI_SCALE` 这类临时覆盖下两者会不一致。
        let ui_scale = scale();

        let dir_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().flex_1().child(self.text_input(
                FieldId::DownloadDir,
                t("下载根目录", "Download folder"),
                cx,
            )))
            .child(btn("btn-pick-dir", t("浏览…", "Browse…"), 0xF2F4F8, TEXT).on_click(cx.listener(
                |this, _, _, cx| {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title(t("选择下载根目录", "Choose the download folder"))
                        .pick_folder()
                    {
                        this.set_dir.set(dir.to_string_lossy().to_string());
                        cx.notify();
                    }
                },
            )));

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(label(t("下载目录", "Download folder")))
            .child(dir_row)
            .child(label(t(
                "GitHub Token（可选，提高 API 限额）",
                "GitHub token (optional — raises the API rate limit)",
            )))
            .child(self.text_input(
                FieldId::Token,
                t("ghp_…（留空则用匿名访问）", "ghp_… (empty = anonymous)"),
                cx,
            ))
            .child(label(t(
                "代理（可选，例如 http://127.0.0.1:7890）",
                "Proxy (optional, e.g. http://127.0.0.1:7890)",
            )))
            .child(self.text_input(
                FieldId::Proxy,
                t("留空表示直连", "empty = direct connection"),
                cx,
            ))
            .child(label(t("同时下载数", "Concurrent downloads")))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(step_btn("btn-conc-less", "−").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cfg.max_concurrent =
                                this.cfg.max_concurrent.saturating_sub(1).max(1);
                            cx.notify();
                        },
                    )))
                    .child(
                        div()
                            .min_w(sp(48.))
                            .text_size(sp(13.))
                            .text_color(rgb(TEXT))
                            .child(format!("{conc}")),
                    )
                    .child(step_btn("btn-conc-more", "+").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cfg.max_concurrent = (this.cfg.max_concurrent + 1).min(8);
                            cx.notify();
                        },
                    ))),
            )
            .child(label(t("自动检查间隔（0 = 关闭）", "Auto-check interval (0 = off)")))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(step_btn("btn-iv-less", "−").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cfg.check_interval_min =
                                this.cfg.check_interval_min.saturating_sub(15);
                            cx.notify();
                        },
                    )))
                    .child(
                        div()
                            .min_w(sp(78.))
                            .text_size(sp(13.))
                            .text_color(rgb(TEXT))
                            .child(if interval == 0 {
                                t("已关闭", "off").to_string()
                            } else {
                                count_text(interval as usize, " 分钟", " min", " min", " 分", "분")
                            }),
                    )
                    .child(step_btn("btn-iv-more", "+").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.cfg.check_interval_min = (this.cfg.check_interval_min + 15).min(1440);
                            cx.notify();
                        },
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(toggle(
                        "toggle-auto",
                        t("启动时自动检查", "Check on startup"),
                        self.cfg.auto_check_on_start,
                        cx,
                        |this| this.cfg.auto_check_on_start = !this.cfg.auto_check_on_start,
                    ))
                    .child(toggle(
                        "toggle-os",
                        t("优先当前系统", "Prefer this OS"),
                        self.cfg.prefer_current_os,
                        cx,
                        |this| this.cfg.prefer_current_os = !this.cfg.prefer_current_os,
                    )),
            )
            .child(label(t("界面语言", "Language")))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    // 和顶栏那颗同一个控件：语言一多，一排按钮立刻摊不开，
                    // 两处都收成下拉，新语言只需要往 `Lang` 里加一项。
                    .child(self.lang_picker(
                        "btn-lang-set",
                        LangAnchor::Set,
                        self.lang_anchor_set.clone(),
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(FAINT))
                            .child(t("切换后立即生效", "Applied immediately")),
                    ),
            )
            .child(label(t(
                "界面缩放（高 DPI 屏上嫌字小就调大）",
                "UI scale (raise it if the text feels small)",
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(step_btn("btn-scale-less", "−").on_click(cx.listener(
                        |this, _, _, cx| this.nudge_scale(-SCALE_STEP, cx),
                    )))
                    .child(
                        div()
                            .min_w(sp(78.))
                            .text_size(sp(13.))
                            .text_color(rgb(TEXT))
                            .child(format!("{}%", (ui_scale * 100.0).round() as i32)),
                    )
                    .child(step_btn("btn-scale-more", "+").on_click(cx.listener(
                        |this, _, _, cx| this.nudge_scale(SCALE_STEP, cx),
                    )))
                    .child(
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(FAINT))
                            .child(t(
                                "只影响界面，不影响下载内容",
                                "affects the UI only",
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        btn("btn-set-ok", t("保存", "Save"), ACCENT, 0xFFFFFF)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_settings(cx))),
                    )
                    .child(
                        btn("btn-set-cancel", t("取消", "Cancel"), 0xF2F4F8, TEXT)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(FAINT))
                            .child(tf!(
                                "v{} · 数据目录 {}",
                                "v{} · data folder {}",
                                env!("CARGO_PKG_VERSION"),
                                crate::store::data_dir().display()
                            )),
                    ),
            );
        self.modal_shell(
            t("设置", "Settings"),
            640.,
            max_h,
            body.into_any_element(),
            cx,
        )
    }

    fn history_dialog(&self, max_h: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<DownloadRecord> = self.history.iter().take(80).cloned().collect();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_5()
            .when(rows.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(sp(13.))
                        .text_color(rgb(FAINT))
                        .child(t("还没有下载记录", "No downloads yet")),
                )
            })
            .children(rows.iter().map(|r| {
                let color = if r.status == "完成" {
                    if r.sha_ok == Some(false) { ORANGE } else { GREEN }
                } else {
                    RED
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0xFAFBFD))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(sp(13.))
                                    .text_color(rgb(TEXT))
                                    .text_ellipsis()
                                    .child(format!("{} · {}", r.app, r.file)),
                            )
                            .child(
                                div()
                                    .text_size(sp(12.))
                                    .text_color(rgb(color))
                                    .child(status_text(&r.status)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(MUTED))
                            .text_ellipsis()
                            .child(format!(
                                "v{} · {} · {}{}{}",
                                r.version,
                                util::fmt_bytes(r.size),
                                // `fmt_date` 是 UTC；不标出来会让人以为时间错了 8 小时。
                                format!("{} UTC", util::fmt_date(r.finished_at)),
                                if r.path.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · {}", r.path)
                                },
                                match r.sha_ok {
                                    Some(true) => t(" · SHA-256 通过", " · SHA-256 ok"),
                                    Some(false) => t(" · SHA-256 不一致", " · SHA-256 mismatch"),
                                    None => "",
                                }
                            )),
                    )
                    .when_some(r.error.clone(), |d, e| {
                        d.child(
                            div()
                                .text_size(sp(12.))
                                .text_color(rgb(RED))
                                .text_ellipsis()
                                .child(e),
                        )
                    })
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2
                    ()
                    .child(
                        btn("btn-hist-clear", t("清空历史", "Clear history"), RED_SOFT, RED)
                            .on_click(cx.listener(
                            |this, _, _, cx| {
                                this.history.clear();
                                crate::store::write_history(&this.history);
                                this.toast(t("下载历史已清空", "Download history cleared"), cx);
                            },
                        )),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(sp(12.))
                            .text_color(rgb(FAINT))
                            .child(count_text(
                                self.history.len(),
                                " 条记录",
                                " record",
                                " records",
                                " 件",
                                "개",
                            )),
                    ),
            );
        self.modal_shell(
            t("下载历史", "Download history"),
            720.,
            max_h,
            body.into_any_element(),
            cx,
        )
    }

    /// `max_h` 由 `render` 按窗口高度算好传进来：`max_h(relative(..))` 在
    /// 绝对定位的遮罩里没能约束住卡片（内容照旧把卡片顶高，底部的按钮被
    /// 推到窗口外），换成确定的像素值最省事。
    fn dialog_layer(&self, max_h: Pixels, cx: &mut Context<Self>) -> AnyElement {
        match self.dialog {
            Some(Dialog::Add) => self.add_dialog(max_h, cx),
            Some(Dialog::Settings) => self.settings_dialog(max_h, cx),
            Some(Dialog::History) => self.history_dialog(max_h, cx),
            None => div().into_any_element(),
        }
    }

    /// 附件列表：直链给「下载」，页面/网盘给「打开」，都带上类型与摘要标记。
    fn asset_card(
        &self,
        track: &Track,
        r: &Release,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let track_id = track.id.clone();
        let version = r.version.clone();
        let page_url = r.page_url.clone();
        let assets: Vec<Asset> = r.assets.clone();
        div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(rgb(CARD))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(sp(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(t("附件", "Files")),
                    )
                    .child(
                        div()
                            .text_size(sp(12.5))
                            .text_color(rgb(FAINT))
                            .child(count_text(assets.len(), " 个", " total", " total", " 件", "개")),
                    ),
            )
            .when(assets.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(sp(13.))
                        .text_color(rgb(FAINT))
                        .child(t(
                            "该来源没有列出可下载文件",
                            "This source lists no downloadable files",
                        )),
                )
            })
            .children(assets.into_iter().map(|a| {
                let direct = a.kind == AssetKind::Direct;
                let row_id = format!("asset-{}-{}", track_id, a.name);
                let btn_id = format!("dl-{}-{}", track_id, a.name);
                let (tid, ver, pg) = (track_id.clone(), version.clone(), page_url.clone());
                let asset_for_click = a.clone();
                // 「直链 / 页面」标签也做成可点的：点它跟点右侧主按钮等价，
                // 否则用户点那两个字（它看起来像链接）会以为程序没反应。
                let badge_asset = a.clone();
                let (btid, bver, bpg) = (track_id.clone(), version.clone(), page_url.clone());
                let copy_text = a.url.clone();
                let copy_name = a.name.clone();
                div()
                    .id(SharedString::from(row_id))
                    .flex()
                    .flex_row()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .gap_1()
                            .flex_1()
                            .child(
                                div()
                                    .text_size(sp(13.5))
                                    .text_color(rgb(TEXT))
                                    .text_ellipsis()
                                    .child(a.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(sp(12.))
                                    .text_color(rgb(FAINT))
                                    .text_ellipsis()
                                    .child(format!(
                                        "{}{}{}",
                                        a.size
                                            .map(util::fmt_bytes)
                                            .unwrap_or_else(|| t("大小未知", "size unknown").into()),
                                        if a.note.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" · {}", a.note)
                                        },
                                        if a.sha256.is_some() { " · SHA-256" } else { "" }
                                    )),
                            ),
                    )
                    .child(
                        chip(
                            format!("kind-{}-{}", track_id, a.name),
                            if direct {
                                t("直链", "direct")
                            } else {
                                t("页面", "page")
                            },
                            if direct { GREEN } else { MUTED },
                            if direct { GREEN_SOFT } else { 0xF2F4F8 },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.start_asset(&btid, &bver, &badge_asset, &bpg, cx);
                        })),
                    )
                    .child(
                        chip(
                            format!("copy-{}-{}", track_id, a.name),
                            t("复制", "Copy"),
                            MUTED,
                            0xF2F4F8,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.copy(copy_text.clone(), What::AssetLink(copy_name.clone()), cx);
                        })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(btn_id))
                            .flex_shrink_0()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(if direct { ACCENT } else { 0xF2F4F8 }))
                            .text_size(sp(12.5))
                            .text_color(rgb(if direct { 0xFFFFFF } else { TEXT }))
                            .hover(|s| s.opacity(0.88))
                            .cursor_pointer()
                            .child(if direct {
                                t("下载", "Download")
                            } else {
                                t("打开", "Open")
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.start_asset(&tid, &ver, &asset_for_click, &pg, cx);
                            })),
                    )
            }))
    }
}

fn history_card(hist: &[crate::model::VersionEntry]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .gap_3()
        .p_5()
        .rounded_lg()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(BORDER))
        .children(hist.iter().map(|h| {
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(sp(14.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .child(h.version.clone()),
                        )
                        .child(
                            div()
                                .text_size(sp(12.))
                                .text_color(rgb(FAINT))
                                .child(h.date.clone().unwrap_or_default()),
                        ),
                )
                .when(!h.title.trim().is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(sp(13.))
                            .text_color(rgb(MUTED))
                            .text_ellipsis()
                            .child(h.title.clone()),
                    )
                })
        }))
}

/// 程序入口：共用同一份数据目录与 provider 层，只有绘制换了实现。
pub fn run() {
    Application::new().run(|cx: &mut GpuiApp| {
        cx.bind_keys([
            gpui::KeyBinding::new("ctrl-n", AddTrack, None),
            gpui::KeyBinding::new("ctrl-,", OpenSettings, None),
            gpui::KeyBinding::new("ctrl-h", OpenHistory, None),
            gpui::KeyBinding::new("ctrl-f", FocusFilter, None),
            gpui::KeyBinding::new("ctrl-1", TabOverview, None),
            gpui::KeyBinding::new("ctrl-2", TabNotes, None),
            gpui::KeyBinding::new("ctrl-3", TabVersions, None),
        ]);
        let bounds = Bounds::centered(None, size(px(1180.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    // 真正的标题在 `render` 里按语言设，这里先给个不带语言的字。
                    title: Some("App Monitor".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| App::new(cx)),
        )
        .expect("open_window");
        cx.activate(true);
    });
}


#[cfg(test)]
mod tests {
    use super::{EditState, Selection, picked};

    #[test]
    fn typing_appends_at_cursor() {
        let mut e = EditState::new("");
        e.insert("h", false);
        e.insert("i", false);
        assert_eq!(e.text, "hi");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn backspace_respects_utf8_boundary() {
        // 「中」占 3 字节；退格必须整体退掉一个字，不能切到 UTF-8 中间。
        let mut e = EditState::new("中a");
        e.backspace();
        assert_eq!(e.text, "中");
        assert_eq!(e.cursor, 3);
        e.backspace();
        assert_eq!(e.text, "");
        assert_eq!(e.cursor, 0);
        // 已经空了再退格不应 panic
        e.backspace();
        assert_eq!(e.text, "");
    }

    #[test]
    fn insert_in_the_middle_moves_cursor() {
        let mut e = EditState::new("ac");
        e.home();
        e.move_right();
        e.insert("b", false);
        assert_eq!(e.text, "abc");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn space_is_inserted_like_any_other_char() {
        // 空格在 GPUI 里走的是命名的键（`key` = "space"），键盘层补一个字面量进来，
        // 这条只保证编辑器本身不挑字符。
        let mut e = EditState::new("a b");
        e.insert(" ", false);
        assert_eq!(e.text, "a b ");
        assert_eq!(e.cursor, 4);
    }

    #[test]
    fn select_all_then_cut_empties_the_field() {
        let mut e = EditState::new("https://a/b");
        assert!(e.selected().is_none(), "没选之前没有可复制的");
        e.select_all();
        let (text, n) = e.selected().expect("全选之后整段都是选区");
        assert_eq!(text, "https://a/b");
        assert_eq!(n, 11);
        assert!(e.cut_selection());
        assert_eq!(e.text, "");
        assert_eq!(e.cursor, 0);
        assert!(!e.cut_selection(), "再切一次已经没有选区可切");
    }

    #[test]
    fn drag_selects_a_range_and_backspace_only_removes_that() {
        let mut e = EditState::new("hello world");
        e.start_select(6);
        e.extend_to(11);
        let (text, n) = e.selected().expect("拖出一段就有选区");
        assert_eq!((text.as_str(), n), ("world", 5));
        // 退格只删选区：早先这条会是整段清空（那时选区只可能来自 Ctrl+A）。
        e.backspace();
        assert_eq!(e.text, "hello ");
        assert_eq!(e.cursor, 6);
    }

    #[test]
    fn dragging_backwards_keeps_the_range_ordered() {
        let mut e = EditState::new("abcdef");
        e.start_select(4);
        e.extend_to(1);
        assert_eq!(e.selected().unwrap().0, "bcd");
        e.insert("X", false);
        assert_eq!(e.text, "aXef");
    }

    #[test]
    fn shift_arrows_extend_and_shrink_from_the_anchor() {
        let mut e = EditState::new("abc");
        e.home();
        e.move_caret_to(e.next_index(), true);
        assert_eq!(e.selected().unwrap().0, "a");
        e.move_caret_to(e.next_index(), true);
        assert_eq!(e.selected().unwrap().0, "ab");
        // 往回走是收窄，锚点不动
        e.move_caret_to(e.prev_index(), true);
        assert_eq!(e.selected().unwrap().0, "a");
        // 不带 Shift 就把选区丢掉
        e.move_caret_to(e.next_index(), false);
        assert!(e.selected().is_none());
    }

    #[test]
    fn selected_snaps_multibyte_offsets() {
        // 偏移落在「中」中间时必须退回字符边界，否则 `get(..)` 会 panic。
        let mut e = EditState::new("中文");
        e.selection = Some((1, 6));
        let (text, n) = e.selected().expect("两端都夹到边界后仍有一段");
        assert_eq!(text, "中文");
        assert_eq!(n, 2);
    }

    #[test]
    fn picked_handles_reversed_selection_and_edges() {
        let text = "安装 说明文字".to_string();
        // 从右往左拖也是同一段，起点终点不必有序
        let back = Selection {
            owner: "n".into(),
            anchor: text.len(),
            head: 0,
            text: text.clone(),
        };
        let (s, n) = picked(&back).expect("反向拖选也要能取到内容");
        assert_eq!(s, text);
        assert_eq!(n, 7, "字数按字符数算，不按字节");

        // 只有一个光标位（点了没拖）时没有可复制的
        let caret = Selection {
            owner: "n".into(),
            anchor: 3,
            head: 3,
            text: text.clone(),
        };
        assert!(picked(&caret).is_none());

        // 故意传一个落在 UTF-8 中间的偏移：snap 到字边界，不能 panic
        let mid = Selection {
            owner: "n".into(),
            anchor: 1,
            head: 4,
            text,
        };
        assert_eq!(picked(&mid).map(|(s, _)| s), Some("安".to_string()));
    }

    #[test]
    fn select_all_then_typing_replaces_everything() {
        let mut e = EditState::new("https://old.example/");
        e.select_all();
        e.insert("https://new.example/", false);
        assert_eq!(e.text, "https://new.example/");
        assert_eq!(e.cursor, e.text.len());
        assert!(e.selection.is_none());
    }

    #[test]
    fn single_line_input_flattens_pasted_newlines() {
        let mut e = EditState::new("");
        e.insert("a\r\nb\nc", false);
        assert_eq!(e.text, "a b c");
    }

    #[test]
    fn multiline_input_keeps_line_breaks() {
        let mut e = EditState::new("");
        e.insert("https://a/1\r\nhttps://b/2\n", true);
        // 只去掉 \r，保留 \n
        assert_eq!(e.text, "https://a/1\nhttps://b/2\n");
    }

    #[test]
    fn delete_removes_char_after_cursor() {
        let mut e = EditState::new("ab中");
        e.home();
        e.delete();
        assert_eq!(e.text, "b中");
        e.delete();
        assert_eq!(e.text, "中");
        e.delete();
        assert_eq!(e.text, "");
        // 末尾再删不应 panic
        e.delete();
    }

    #[test]
    fn cursor_navigation_stops_at_edges() {
        let mut e = EditState::new("中a");
        e.home();
        e.move_left();
        assert_eq!(e.cursor, 0);
        e.end();
        e.move_right();
        assert_eq!(e.cursor, e.text.len());
        e.move_left();
        assert_eq!(e.cursor, 3, "从末尾左移应退到「中」之后");
    }

    #[test]
    fn set_replaces_text_and_puts_cursor_at_end() {
        let mut e = EditState::new("old");
        e.select_all();
        e.set("new-dir");
        assert_eq!(e.text, "new-dir");
        assert_eq!(e.cursor, 7);
        assert!(e.selection.is_none());
    }
}
