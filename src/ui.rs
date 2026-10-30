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
    IntoElement, ParentElement, Pixels, Render, SharedString, Stateful, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::FluentBuilder, px, relative,
    rgb, size,
};

use crate::download::DlSpec;
use crate::engine::{Engine, UiMsg};
use crate::http::Http;
use crate::i18n::{self, t};
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
    /// 全选区间（Ctrl+A 之后），下一次输入会整体替换。
    selection: Option<(usize, usize)>,
}

impl EditState {
    fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.len();
        Self {
            text,
            cursor,
            selection: None,
        }
    }

    fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.selection = None;
    }

    fn split(&self) -> (&str, &str) {
        let cursor = self.cursor.min(self.text.len());
        match self.text.get(..cursor) {
            Some(before) => (before, &self.text[cursor..]),
            None => (&self.text, ""),
        }
    }

    fn insert(&mut self, s: &str, multiline: bool) {
        if let Some((a, b)) = self.selection.take() {
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
        if self.selection.take().is_some() {
            self.text.clear();
            self.cursor = 0;
            return;
        }
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
        if self.cursor >= self.text.len() {
            return;
        }
        let mut j = self.cursor + 1;
        while j < self.text.len() && !self.text.is_char_boundary(j) {
            j += 1;
        }
        self.text.replace_range(self.cursor..j, "");
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut i = self.cursor - 1;
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        self.cursor = i;
    }

    fn move_right(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let mut j = self.cursor + 1;
        while j < self.text.len() && !self.text.is_char_boundary(j) {
            j += 1;
        }
        self.cursor = j;
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.text.len();
    }

    fn select_all(&mut self) {
        self.selection = Some((0, self.text.len()));
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

    fn move_left(&mut self) {
        self.edit.move_left();
    }

    fn move_right(&mut self) {
        self.edit.move_right();
    }

    fn home(&mut self) {
        self.edit.home();
    }

    fn end(&mut self) {
        self.edit.end();
    }

    fn select_all(&mut self) {
        self.edit.select_all();
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
    fn zh(&self) -> String {
        match self {
            What::Version => "版本号".into(),
            What::PageLink => "来源页链接".into(),
            What::Notes => "更新说明".into(),
            What::AssetLink(name) => format!("「{name}」的链接"),
            What::Selection(n) => format!("选中的 {n} 个字"),
        }
    }

    fn en(&self) -> String {
        match self {
            What::Version => "version number".into(),
            What::PageLink => "source page link".into(),
            What::Notes => "release notes".into(),
            What::AssetLink(name) => format!("link to \"{name}\""),
            What::Selection(n) => format!("selected {n} character(s)"),
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
    /// 选区上右键弹出的菜单。松手即复制之外再给一条显式路径：用户习惯先选中、
    /// 再右键点「复制」，缺了它就会被当成「这个界面不能复制」。
    ctx: Option<CtxMenu>,
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
            root_focus: cx.focus_handle(),
            add_skipped: Vec::new(),
            confirm_delete: None,
            last_auto: Instant::now(),
            title_lang: None,
            sel: None,
            sel_geom: Rc::new(RefCell::new(None)),
            ctx: None,
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
        app
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

    fn focus_field(&mut self, id: FieldId, cx: &mut Context<Self>) {
        self.active_field = Some(id);
        self.pending_focus = Some(id);
        cx.notify();
    }

    /// 一个够用的单行编辑器：可打印字符、退格、删除、方向键、Ctrl+A / Ctrl+V。
    fn on_field_key(&mut self, id: FieldId, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform || ks.modifiers.control;
        let mut handled = true;

        if cmd && ks.key == "v" {
            if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                self.field_mut(id).insert(&text);
            }
        } else if cmd && ks.key == "a" {
            self.field_mut(id).select_all();
        } else if cmd {
            handled = false;
        } else {
            match ks.key.as_str() {
                "backspace" => self.field_mut(id).backspace(),
                "delete" => self.field_mut(id).delete(),
                "left" => self.field_mut(id).move_left(),
                "right" => self.field_mut(id).move_right(),
                "home" => self.field_mut(id).home(),
                "end" => self.field_mut(id).end(),
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
        cx.notify();
    }

    fn close_dialog(&mut self, cx: &mut Context<Self>) {
        self.dialog = None;
        self.active_field = None;
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
                if i18n::is_en() {
                    format!("No trackable URL found{detail}")
                } else {
                    format!("没能识别出可追踪的地址{detail}")
                },
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
                if i18n::is_en() {
                    format!("Nothing new — all {dup} are already tracked")
                } else {
                    format!("没有新增条目（{dup} 个已在列表里）")
                },
                cx,
            );
            return;
        }
        if let Some(id) = first_new {
            self.select(&id, cx);
        }
        self.dialog = None;
        self.toast(
            if dup > 0 {
                if i18n::is_en() {
                    format!("Added {added} item(s), skipped {dup} duplicate(s)")
                } else {
                    format!("已添加 {added} 个追踪项，跳过 {dup} 个重复项")
                }
            } else if i18n::is_en() {
                format!("Added {added} item(s), checking versions…")
            } else {
                format!("已添加 {added} 个追踪项，开始检查版本…")
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
            if i18n::is_en() {
                format!("Deleted \"{name}\" and its local record")
            } else {
                format!("已删除「{name}」")
            },
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
        cx.notify();
    }

    fn toast(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        self.toast = Some((msg.into(), Instant::now()));
        cx.notify();
    }

    /// 写剪贴板。GPUI 0.2 还没有文本选中，凡是可能想复制走的文本都给一个按钮。
    fn copy(&mut self, text: impl Into<String>, what: What, cx: &mut Context<Self>) {
        let text = text.into();
        let msg = if text.trim().is_empty() {
            if i18n::is_en() {
                format!("Nothing to copy — no {}", what.en())
            } else {
                format!("没有可复制的{}", what.zh())
            }
        } else {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            if i18n::is_en() {
                format!("Copied {}", what.en())
            } else {
                format!("已复制{}", what.zh())
            }
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
                if i18n::is_en() {
                    format!(
                        "{total} item{}",
                        if total == 1 { "" } else { "s" }
                    )
                } else {
                    format!("{total} 个追踪项")
                },
                MUTED,
                0xF2F4F8,
            ))
            .when(updates > 0, |d| {
                d.child(badge(
                    if i18n::is_en() {
                        format!(
                            "{updates} update{}",
                            if updates == 1 { "" } else { "s" }
                        )
                    } else {
                        format!("{updates} 项有更新")
                    },
                    ORANGE,
                    ORANGE_SOFT,
                ))
            })
            .child(div().flex_1())
            .when(!self.resolving.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(sp(12.5))
                        .text_color(rgb(ACCENT))
                        .child(if i18n::is_en() {
                            format!("Checking {}…", self.resolving.len())
                        } else {
                            format!("检查中 {}…", self.resolving.len())
                        }),
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
            .child(self.lang_switch(cx))
    }

    /// 顶栏最右侧的语言切换：`中 | EN` 两个小格。
    ///
    /// 放在最外层而不是只藏在设置里，是因为「界面是外语，我找不到改语言的地方」
    /// 是个死胡同 —— 用户不认识那些字，自然也猜不到哪颗按钮通向设置。
    fn lang_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cur = i18n::current();
        let seg = |id: &'static str, want: i18n::Lang| {
            let active = cur == want;
            div()
                .id(id)
                .flex_shrink_0()
                .px_2()
                .py_1()
                .text_size(sp(12.))
                .text_color(rgb(if active { 0xFFFFFF } else { MUTED }))
                .bg(rgb(if active { ACCENT } else { 0xFFFFFF }))
                .cursor_pointer()
                .when(!active, |d| d.hover(|s| s.bg(rgb(0xF2F4F8))))
                .child(want.short())
                .on_click(cx.listener(move |this, _, _, cx| this.set_lang(want, cx)))
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(rgb(BORDER))
            .child(seg("lang-zh", i18n::Lang::Zh))
            .child(seg("lang-en", i18n::Lang::En))
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
                                    .child(group),
                            )
                            .children(rows.into_iter().map(|i| {
                                let t = &self.tracks[i];
                                let is_sel = selected.as_deref() == Some(t.id.as_str());
                                let (sub, color) = track_status(t);
                                let t_id = t.id.clone();
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
                                    .child(
                                        div()
                                            .text_size(sp(12.))
                                            .text_color(rgb(color))
                                            .text_ellipsis()
                                            .child(sub),
                                    )
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
                    .min_w_0()
                    .gap_3()
                    .px_5()
                    .pb_5()
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
    fn queue_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.tasks.is_empty() {
            return div().into_any_element();
        }
        let running = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Queued | TaskStatus::Paused))
            .count();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_5()
            .py_3()
            .bg(rgb(CARD))
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(sp(13.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child(t("下载队列", "Downloads")),
                    )
                    // 全部结束时不再显示「0 进行中」——那一行只是噪音。
                    .when(running > 0, |d| {
                        d.child(badge(
                            if i18n::is_en() {
                                format!("{running} active")
                            } else {
                                format!("{running} 进行中")
                            },
                            MUTED,
                            0xF2F4F8,
                        ))
                    })
                    .child(div().flex_1())
                    .child(
                        btn("btn-clear", t("清空已结束", "Clear finished"), 0xF2F4F8, TEXT)
                            .on_click(cx.listener(|this, _, _, cx| this.clear_finished(cx))),
                    ),
            )
            .children(self.tasks.iter().take(3).map(|task| {
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
                    TaskStatus::Paused => {
                        if i18n::is_en() {
                            format!(
                                "paused · {} / {}",
                                util::fmt_bytes(task.downloaded),
                                util::fmt_bytes(task.total)
                            )
                        } else {
                            format!(
                                "已暂停 · {} / {}",
                                util::fmt_bytes(task.downloaded),
                                util::fmt_bytes(task.total)
                            )
                        }
                    }
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
                            .child(
                                div()
                                    .text_size(sp(12.))
                                    .text_color(rgb(status_color(task.status)))
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
                    .when(!detail.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(sp(12.))
                                .text_color(rgb(MUTED))
                                .text_ellipsis()
                                .child(detail),
                        )
                    })
                    .child(
                        div()
                            .h(px(3.))
                            .w_full()
                            .rounded_full()
                            .bg(rgb(0xE9EDF4))
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
                    .child(badge(track.group.clone(), MUTED, 0xF2F4F8))
                    .when(track.kind != crate::model::SourceKind::GitHub, |d| {
                        // GitHub 分组和来源名一样，重复显示一个 badge 只是噪音。
                        d.child(badge(track.kind.label(), MUTED, 0xF2F4F8))
                    })
                    .when_some(track.last_checked, |d, ts| {
                        d.child(
                            div()
                                .text_size(sp(12.))
                                .text_color(rgb(FAINT))
                                .child(if i18n::is_en() {
                                    format!("checked {}", util::fmt_secs_ago(ts))
                                } else {
                                    format!("上次检查 {}", util::fmt_secs_ago(ts))
                                }),
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
                                .child(format!("v{v}")),
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
                    .child(if i18n::is_en() {
                        format!("Source: {}", track.url)
                    } else {
                        format!("来源：{}", track.url)
                    }),
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
        let ver_text = format!("v{}", r.version);
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
                                    .child(if i18n::is_en() {
                                        format!(
                                            "{} · published {} · {}",
                                            r.title, published, r.provider
                                        )
                                    } else {
                                        format!(
                                            "{} · 发布于 {} · 来源 {}",
                                            r.title, published, r.provider
                                        )
                                    }),
                            ),
                    )
                    .when(sel > 0, |d| {
                        d.child(badge(
                            if r.candidates.get(sel).map(|c| c.prerelease).unwrap_or(false) {
                                t("预览版", "Preview")
                            } else {
                                t("备选版本", "Alternative")
                            },
                            ORANGE,
                            ORANGE_SOFT,
                        ))
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
                                        if i18n::is_en() {
                                            format!(
                                                "Read the full notes ({} chars)",
                                                full.chars().count()
                                            )
                                        } else {
                                            format!(
                                                "查看完整说明（共 {} 字）",
                                                full.chars().count()
                                            )
                                        },
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
        } else if self.dialog.is_none() && !self.root_focus.is_focused(window) {
            // 关掉弹窗后焦点会悬空，快捷键随之失效，交还给根节点。
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
                                    .child(if i18n::is_en() {
                                        format!(
                                            "{} data file(s) could not be read — this session fell back to defaults",
                                            self.issues.len()
                                        )
                                    } else {
                                        format!(
                                            "{} 个数据文件未能读取，本次已改用默认内容",
                                            self.issues.len()
                                        )
                                    }),
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
                                if i18n::is_en() {
                                    format!("the original was kept as {name}")
                                } else {
                                    format!("原文件已备份为 {name}")
                                }
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
                            .child(if i18n::is_en() {
                                format!(
                                    "· {} — {}, fell back to {}. {}",
                                    i.file,
                                    what,
                                    fallback_text(i.fallback),
                                    kept
                                )
                            } else {
                                format!(
                                    "· {} {}，本次使用{}。{}",
                                    i.file,
                                    what,
                                    fallback_text(i.fallback),
                                    kept
                                )
                            })
                    }))
            }))
            .child(self.top_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.sidebar(cx))
                    .child(self.main_panel(text_width, window, cx)),
            )
            .child(self.queue_panel(cx))
            // toast 要排在弹窗**之前**：它是整行一条，画在弹窗后面时会把
            // 弹窗底部（保存/取消）盖住。
            .children(toast.filter(|(_, at)| at.elapsed().as_secs() < 4).map(|(msg, _)| {
                div()
                    .flex()
                    .flex_row()
                    .px_5()
                    .py_2()
                    .bg(rgb(0x1F2937))
                    .text_size(sp(13.))
                    .text_color(rgb(0xFFFFFF))
                    .child(msg)
            }))
            .child(self.dialog_layer(px((vh * 0.9).max(320.)), cx))
            // 菜单压在所有东西之上：它是「此刻唯一可点的东西」，被弹窗或 toast
            // 挡住的话用户会以为右键没生效。
            .child(self.ctx_layer(vw, vh, cx))
    }
}

// ---------------------------------------------------------------------------
// 小部件
// ---------------------------------------------------------------------------

/// 损坏文件横幅里的「原因」。中文直接用 `Display` —— 和命令行 `--check` 是
/// 同一套措辞，不用维护两份；英文在这里给。
fn damage_text(d: &crate::store::Damage) -> String {
    use crate::store::Damage;
    if !i18n::is_en() {
        return d.to_string();
    }
    match d {
        Damage::Io(e) => format!("it could not be read: {e}"),
        Damage::Empty => "the file was empty, most likely an interrupted write".to_string(),
        Damage::BadJson(e) => format!("its content is not valid JSON ({e})"),
    }
}

/// 出问题后这次会话改用了什么。
fn fallback_text(f: crate::store::Fallback) -> String {
    use crate::store::Fallback;
    if !i18n::is_en() {
        return f.to_string();
    }
    match f {
        Fallback::DefaultConfig => "default settings".to_string(),
        Fallback::DefaultTracks => "the built-in list".to_string(),
        Fallback::EmptyHistory => "an empty history".to_string(),
    }
}

/// 界面字体：英文用 Segoe UI（更地道），中文继续用雅黑（字形与字重按它在调）。
fn ui_font() -> &'static str {
    if i18n::is_en() && cfg!(windows) {
        "Segoe UI"
    } else {
        "Microsoft YaHei UI"
    }
}

fn track_status(track: &Track) -> (String, u32) {
    if let Some(err) = &track.last_error {
        let short: String = err.chars().take(18).collect();
        return (
            if i18n::is_en() {
                format!("failed · {short}")
            } else {
                format!("解析失败 · {short}")
            },
            RED,
        );
    }
    if let Some(r) = &track.release {
        if track.has_update() {
            return (
                if i18n::is_en() {
                    format!("↑ new {}", r.version)
                } else {
                    format!("↑ 新版本 {}", r.version)
                },
                ORANGE,
            );
        }
        return (format!("v{}", r.version), GREEN);
    }
    (t("等待首次检查", "waiting for first check").to_string(), FAINT)
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
            .child(body)
            .on_click(cx.listener(move |this, _, _, cx| this.focus_field(field, cx)))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                this.on_field_key(field, ev, cx)
            }))
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
                                .child(if i18n::is_en() {
                                    format!(
                                        "{} line(s) could not be parsed (ignored):",
                                        self.add_skipped.len()
                                    )
                                } else {
                                    format!(
                                        "有 {} 行没能识别（已忽略）：",
                                        self.add_skipped.len()
                                    )
                                }),
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
        // 语言按钮：点了立刻生效，不像别的设置那样要按「保存」—— 换错语言之后
        // 弹窗里的字已经变了，得能马上切回来。三态：中文 / English / 跟随系统。
        let lang_btn = |id: &'static str, want: Option<i18n::Lang>, cx: &mut Context<Self>| {
            let following = self.cfg.lang.trim().is_empty();
            let active = match want {
                // 跟随系统时也不额外高亮实际语言，否则两颗按钮一起亮，看不出
                // 「现在到底是选的还是跟着系统走的」。
                Some(w) => !following && i18n::current() == w,
                None => following,
            };
            let text = match want {
                Some(w) => w.label(),
                None => t("跟随系统", "System"),
            };
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .bg(rgb(if active { ACCENT_SOFT } else { 0xF2F4F8 }))
                .text_size(sp(13.))
                .text_color(rgb(if active { ACCENT } else { TEXT }))
                .cursor_pointer()
                .child(text)
                .on_click(cx.listener(move |this, _, _, cx| match want {
                    Some(w) => this.set_lang(w, cx),
                    None => this.set_lang_auto(cx),
                }))
        };

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
                            } else if i18n::is_en() {
                                format!("{interval} min")
                            } else {
                                format!("{interval} 分钟")
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
                    .child(lang_btn("btn-lang-zh", Some(i18n::Lang::Zh), cx))
                    .child(lang_btn("btn-lang-en", Some(i18n::Lang::En), cx))
                    .child(lang_btn("btn-lang-auto", None, cx)),
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
                            .child(if i18n::is_en() {
                                format!(
                                    "v{} · data folder {}",
                                    env!("CARGO_PKG_VERSION"),
                                    crate::store::data_dir().display()
                                )
                            } else {
                                format!(
                                    "v{} · 数据目录 {}",
                                    env!("CARGO_PKG_VERSION"),
                                    crate::store::data_dir().display()
                                )
                            }),
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
                            .child(if i18n::is_en() {
                                format!(
                                    "{} record{}",
                                    self.history.len(),
                                    if self.history.len() == 1 { "" } else { "s" }
                                )
                            } else {
                                format!("共 {} 条", self.history.len())
                            }),
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
                            .child(if i18n::is_en() {
                                format!("{} total", assets.len())
                            } else {
                                format!("{} 个", assets.len())
                            }),
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
