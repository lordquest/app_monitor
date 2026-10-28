//! 界面层：GPUI 实现，v0.2 起是唯一的 UI（egui 版已移除）。
//!
//! GPUI 的 Windows 后端是 Direct3D 11 + DirectComposition，实际要求 Win10
//! 及以上；v0.2 之前那套「DX12 → WARP → glow」的兜底链随 eframe 一起删掉了，
//! 换来的是更小的依赖树、更少的内存占用和一套真正跨平台的渲染路径
//! （macOS 走 Metal，Linux 走 Vulkan/blade）。Win7/8 停在 v0.1.2。
//!
//! 非 UI 层（model / store / provider / download / engine / util）与界面无关，
//! 这里只负责把同一份数据画出来并接住事件。

use std::collections::HashMap;
use std::time::Instant;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::model::{Asset, AssetKind, Config, DownloadRecord, Release, Track};
use crate::store::Store;
use crate::util;

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
            TaskStatus::Queued => "排队中",
            TaskStatus::Running => "下载中",
            TaskStatus::Paused => "已暂停",
            TaskStatus::Done => "已完成",
            TaskStatus::Failed => "失败",
            TaskStatus::Canceled => "已取消",
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
            Tab::Overview => "概览",
            Tab::Notes => "更新说明",
            Tab::History => "历史版本",
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
}

impl App {
    fn new(cx: &mut Context<Self>) -> Self {
        let store = Store::load();
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
            toast: http_err.map(|e| (format!("代理设置无效，本次改用直连：{e}"), Instant::now())),
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
                                Some(true) => format!("{name} 下载完成，SHA-256 校验通过"),
                                Some(false) => format!("{name} 下载完成，但 SHA-256 不一致"),
                                None => format!("{name} 下载完成（无官方摘要）"),
                            },
                            Instant::now(),
                        ));
                        self.record_history(&task, "完成", None);
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
                        self.record_history(&task, "失败", Some(error));
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
                        self.record_history(&task, "已取消", None);
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
            self.toast("全部条目都在检查中", cx);
            return;
        }
        for t in pending {
            self.resolving.insert(t.id.clone());
            self.engine.start_resolve(self.http.clone(), t);
        }
        self.toast("已开始检查全部追踪项", cx);
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
                        "已在浏览器中打开网盘链接"
                    } else {
                        "已在浏览器中打开页面"
                    },
                    cx,
                );
            } else {
                self.toast("该链接为磁力/电驴协议，请用「复制」后粘贴到下载工具", cx);
            }
            return;
        }
        let version = if version.trim().is_empty() {
            "未标注版本".to_string()
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
            self.toast("该文件已在下载队列中", cx);
            return;
        }
        let missing_sha = spec.sha256.is_none();
        self.tasks.insert(0, DlTask::new(spec));
        self.toast(
            if missing_sha {
                "已加入下载队列（该文件无官方摘要，下载后无法校验）"
            } else {
                "已加入下载队列"
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
            self.toast("先粘贴要追踪的页面、仓库或软件站地址", cx);
            return;
        }
        let parsed = crate::parse::parse_input(&text);
        if parsed.tracks.is_empty() {
            self.add_skipped = parsed.skipped.clone();
            let detail = if parsed.skipped.is_empty() {
                String::new()
            } else {
                format!("（{}）", parsed.skipped.join(" / "))
            };
            self.toast(format!("没能识别出可追踪的地址{detail}"), cx);
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
                format!("没有新增条目（{dup} 个已在列表里）"),
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
                format!("已添加 {added} 个追踪项，跳过 {dup} 个重复项")
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
            self.toast("下载目录不能为空", cx);
            return;
        }
        self.cfg.download_dir = dir;
        self.cfg.proxy = self.set_proxy.text().trim().to_string();
        self.cfg.github_token = self.set_token.text().trim().to_string();
        let (http, err) = make_http(&self.cfg);
        if let Some(e) = err {
            // 代理填错时不要静默退回直连：那会让用户以为设置生效了。
            self.toast(format!("代理或 Token 无效：{e}"), cx);
            return;
        }
        self.http = http;
        crate::store::write_config(&self.cfg);
        self.dialog = None;
        self.toast("设置已保存", cx);
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
        self.toast(format!("已删除「{name}」"), cx);
    }

    fn record_history(&mut self, task: &DlTask, status: &str, error: Option<String>) {
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
            status: status.to_string(),
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
    fn copy(&mut self, text: impl Into<String>, what: &str, cx: &mut Context<Self>) {
        let text = text.into();
        if text.trim().is_empty() {
            self.toast(format!("没有可复制的{what}"), cx);
            return;
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        self.toast(format!("已复制{what}"), cx);
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
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("App Monitor"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child("跨平台软件追踪下载器"),
                    ),
            )
            .child(badge(format!("{total} 个追踪项"), MUTED, 0xF2F4F8))
            .when(updates > 0, |d| {
                d.child(badge(format!("{updates} 项有更新"), ORANGE, ORANGE_SOFT))
            })
            .child(div().flex_1())
            .when(!self.resolving.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(ACCENT))
                        .child(format!("检查中 {}…", self.resolving.len())),
                )
            })
            .child(
                btn("btn-check", "检查全部更新", ACCENT, 0xFFFFFF)
                    .on_click(cx.listener(|this, _, _, cx| this.resolve_all(cx))),
            )
            .child(btn("btn-hist", "下载历史", 0xF2F4F8, TEXT).on_click(
                cx.listener(|this, _, _, cx| this.open_dialog(Dialog::History, cx)),
            ))
            .child(btn("btn-settings", "设置", 0xF2F4F8, TEXT).on_click(
                cx.listener(|this, _, _, cx| this.open_dialog(Dialog::Settings, cx)),
            ))
            .child(btn("btn-add", "+ 添加追踪", ACCENT, 0xFFFFFF).on_click(
                cx.listener(|this, _, _, cx| {
                    this.open_dialog(Dialog::Add, cx);
                    // 打开就把焦点给输入框，粘贴不用先点一下。
                    this.focus_field(FieldId::AddInput, cx);
                }),
            ))
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
            .w(px(272.))
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
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(MUTED))
                            .child("追踪列表"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
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
                    .child(self.text_input(FieldId::Filter, "筛选名称或地址", cx)),
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
                                .text_size(px(11.))
                                .text_color(rgb(FAINT))
                                .child(if needle.is_empty() {
                                    "还没有追踪项，点右上角「添加追踪」"
                                } else {
                                    "没有匹配的条目"
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
                                    .text_size(px(10.5))
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
                                            .text_size(px(12.5))
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
                                            .text_size(px(10.5))
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

    fn main_panel(&self, text_w: Pixels, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(track) = self.track().cloned() else {
            return div()
                .flex_1()
                .h_full()
                .bg(rgb(BG))
                .flex()
                .items_center()
                .justify_center()
                .child(div().text_color(rgb(FAINT)).child("左侧还没有追踪项"));
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
                        Some(r) => self.release_card(&track, r, sel, text_w, cx),
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
                                                    .text_size(px(11.))
                                                    .text_color(rgb(FAINT))
                                                    .child("更新说明"),
                                            )
                                            .child(div().flex_1())
                                            // GPUI 0.2 没有文本选中，长文本得给个一键复制的出口。
                                            .child(
                                                chip(
                                                    format!("copy-notes-{tid}"),
                                                    "复制全文",
                                                    MUTED,
                                                    0xF2F4F8,
                                                )
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    let c = copy.clone();
                                                    this.copy(c, "更新说明", cx);
                                                })),
                                            ),
                                    )
                                    .child(notes_card(&notes, text_w)),
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
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("下载队列"),
                    )
                    .child(badge(format!("{running} 进行中"), MUTED, 0xF2F4F8))
                    .child(div().flex_1())
                    .child(
                        btn("btn-clear", "清空已结束", 0xF2F4F8, TEXT)
                            .on_click(cx.listener(|this, _, _, cx| this.clear_finished(cx))),
                    ),
            )
            .children(self.tasks.iter().take(3).map(|t| {
                let id = t.spec.id.clone();
                let id2 = id.clone();
                let pause_label = if t.status == TaskStatus::Running {
                    "暂停"
                } else if t.status == TaskStatus::Paused {
                    "继续"
                } else {
                    ""
                };
                let finished = matches!(
                    t.status,
                    TaskStatus::Done | TaskStatus::Failed | TaskStatus::Canceled
                );
                let detail = match t.status {
                    TaskStatus::Running => format!(
                        "{} / {} · {}",
                        util::fmt_bytes(t.downloaded),
                        util::fmt_bytes(t.total),
                        util::fmt_speed(t.speed)
                    ),
                    TaskStatus::Done => format!(
                        "{} · {}",
                        util::fmt_bytes(t.downloaded),
                        match t.sha_ok {
                            Some(true) => "SHA-256 校验通过",
                            Some(false) => "SHA-256 不一致",
                            None => "无官方摘要",
                        }
                    ),
                    TaskStatus::Failed => t.error.clone().unwrap_or_else(|| "下载失败".into()),
                    TaskStatus::Paused => format!(
                        "已暂停 · {} / {}",
                        util::fmt_bytes(t.downloaded),
                        util::fmt_bytes(t.total)
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
                                    .text_size(px(11.5))
                                    .text_color(rgb(TEXT))
                                    .text_ellipsis()
                                    .min_w_0()
                                    .flex_1()
                                    .child(format!("{} · {}", t.spec.app, t.spec.file)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgb(status_color(t.status)))
                                    .child(t.status.label()),
                            )
                            .when(!finished && !pause_label.is_empty(), |d| {
                                d.child(
                                    div()
                                        .id(SharedString::from(format!("pause-{id}")))
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .bg(rgb(0xF2F4F8))
                                        .text_size(px(10.5))
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .child(pause_label)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_pause(&id, cx)
                                        })),
                                )
                            })
                            .when(t.status == TaskStatus::Done, |d| {
                                // 完成的给「打开」：直接交给系统默认程序，比让用户
                                // 自己去找目录再双击快。
                                let Some(path) = t.path.clone() else {
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
                                        .text_size(px(10.5))
                                        .text_color(rgb(TEXT))
                                        .cursor_pointer()
                                        .child("打开")
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
                                            .text_size(px(10.5))
                                            .text_color(rgb(TEXT))
                                            .cursor_pointer()
                                            .child("目录")
                                            .on_click(cx.listener(move |_, _, _, _| {
                                                let _ = open::that_detached(&d2);
                                            })),
                                    )
                                })
                            })
                            .when(
                                matches!(t.status, TaskStatus::Failed | TaskStatus::Canceled),
                                |d| {
                                    let id3 = t.spec.id.clone();
                                    d.child(
                                        div()
                                            .id(SharedString::from(format!("retry-{id3}")))
                                            .px_2()
                                            .py_1()
                                            .rounded_md()
                                            .bg(rgb(ACCENT_SOFT))
                                            .text_size(px(10.5))
                                            .text_color(rgb(ACCENT))
                                            .cursor_pointer()
                                            .child("重试")
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
                                        .text_size(px(10.5))
                                        .text_color(rgb(RED))
                                        .cursor_pointer()
                                        .child("取消")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel_task(&id2, cx)
                                        })),
                                )
                            }),
                    )
                    .when(!detail.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(10.5))
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
                                    .w(relative(t.frac()))
                                    .rounded_full()
                                    .bg(rgb(status_color(t.status))),
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
                            .text_size(px(19.))
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
                                .text_size(px(10.5))
                                .text_color(rgb(FAINT))
                                .child(format!("上次检查 {}", util::fmt_secs_ago(ts))),
                        )
                    })
                    .when_some(track.last_error.clone(), |d, err| {
                        d.child(badge("解析失败", RED, RED_SOFT))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(rgb(RED))
                                    .text_ellipsis()
                                    .child(err),
                            )
                    })
                    .child(div().flex_1())
                    .when(self.resolving.contains(&track.id), |d| {
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(ACCENT))
                                .child("检查中…"),
                        )
                    })
                    .when_some(view.map(|r| r.version.clone()), |d, v| {
                        d.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(MUTED))
                                .child(format!("v{v}")),
                        )
                    })
                    .child({
                        let id = track.id.clone();
                        btn("btn-recheck", "重新检查", 0xF2F4F8, TEXT).on_click(cx.listener(
                            move |this, _, _, cx| this.resolve_one(&id, cx),
                        ))
                    })
                    .child({
                        let id = track.id.clone();
                        if self.confirm_delete.as_deref() == Some(track.id.as_str()) {
                            // 二次确认放在原地：弹一个确认对话框对移除一行来说太重。
                            btn("btn-del-yes", "确认删除", RED_SOFT, RED).on_click(cx.listener(
                                move |this, _, _, cx| this.delete_track(&id, cx),
                            ))
                        } else {
                            btn("btn-del", "删除", RED_SOFT, RED).on_click(cx.listener(
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
                            .text_size(px(12.))
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
                    .text_size(px(13.))
                    .text_color(rgb(MUTED))
                    .child("还没有解析结果"),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(rgb(FAINT))
                    .whitespace_normal()
                    .child(format!("来源：{}", track.url)),
            )
            .into_any_element()
    }

    fn release_card(
        &self,
        track: &Track,
        r: &Release,
        sel: usize,
        text_w: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let url = r.page_url.clone();
        let copy_url = r.page_url.clone();
        let ver_text = format!("v{}", r.version);
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
                                    .text_size(px(26.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(ver_text.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(rgb(MUTED))
                                    .text_ellipsis()
                                    .child(format!(
                                        "{} · 发布于 {} · 来源 {}",
                                        r.title,
                                        r.published.clone().unwrap_or_else(|| "未知".into()),
                                        r.provider
                                    )),
                            ),
                    )
                    .when(sel > 0, |d| {
                        d.child(badge(
                            if r.candidates.get(sel).map(|c| c.prerelease).unwrap_or(false) {
                                "预览版"
                            } else {
                                "备选版本"
                            },
                            ORANGE,
                            ORANGE_SOFT,
                        ))
                    })
                    .when(sel == 0 && track.has_update(), |d| {
                        d.child(badge("发现新版本", ORANGE, ORANGE_SOFT))
                    })
                    .child(
                        chip(
                            format!("copy-ver-{}", track.id),
                            "复制版本",
                            MUTED,
                            0xF2F4F8,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let t = ver_text.clone();
                            this.copy(t, "版本号", cx);
                        })),
                    )
                    .child(
                        chip(
                            format!("copy-page-{}", track.id),
                            "复制链接",
                            MUTED,
                            0xF2F4F8,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let u = copy_url.clone();
                            this.copy(u, "来源页链接", cx);
                        })),
                    )
                    .child(
                        btn("btn-open-page", "打开来源页", 0xF2F4F8, TEXT).on_click(cx.listener(
                            move |this, _, _, cx| {
                                let _ = open::that_detached(&url);
                                this.toast("已在浏览器打开来源页", cx);
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
                        .child(
                            div()
                                // 给确定宽度，否则 taffy 会用 min-content 宽度量高度：
                                // 换行看着是对的，但卡片底部会多出一大块空白。
                                .w(text_w)
                                .text_size(px(12.5))
                                .text_color(rgb(TEXT))
                                .whitespace_normal()
                                .child(shown),
                        )
                        .when(compact && full.chars().count() > 260, |d| {
                            // 放在 flex_row 里，否则列方向的 stretch 会把这颗胶囊拉成整行。
                            d.child(
                                div().flex().flex_row().child(
                                    chip(
                                        format!("more-{tid}"),
                                        format!(
                                            "查看完整说明（共 {} 字）",
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
                        .text_size(px(11.))
                        .text_color(rgb(FAINT))
                        .child("版本候选"),
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
                            .text_size(px(11.))
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
        // 正文文本按确定宽度换行：taffy 给自动宽度的换行文本量高度时用的是
        // min-content 宽度（最长单词的宽度），于是卡片下半截会多出几百像素空白。
        // 这里按窗口宽度减去侧栏、内边距与边框算一个安全宽度传下去。
        let vw: f32 = window.viewport_size().width.into();
        let text_width = px((vw - 370.).max(240.));
        let toast = self.toast.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .font_family("Microsoft YaHei UI")
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
                        .text_size(px(10.5))
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
                                    .text_size(px(11.5))
                                    .text_color(rgb(TEXT))
                                    .child(format!(
                                        "{} 个数据文件未能读取，本次已改用默认内容",
                                        self.issues.len()
                                    )),
                            )
                            .child(
                                small("issue-open", "打开数据目录").on_click(cx.listener(
                                    |_, _, _, _| {
                                        let dir = crate::store::data_dir();
                                        let _ = std::fs::create_dir_all(dir);
                                        let _ = open::that_detached(dir);
                                    },
                                )),
                            )
                            .child(
                                small("issue-dismiss", "知道了").on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.issues.clear();
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                    .children(self.issues.iter().map(|i| {
                        let kept = match &i.backup {
                            Some(p) => format!(
                                "原文件已备份为 {}",
                                p.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| p.to_string_lossy().to_string())
                            ),
                            // 备份失败代表原文件仍在原地，程序这次不会覆盖它。
                            None => "原文件保持不动".to_string(),
                        };
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(MUTED))
                            .whitespace_normal()
                            .child(format!(
                                "· {} {}，本次使用{}。{}",
                                i.file, i.what, i.fallback, kept
                            ))
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
                    .child(self.main_panel(text_width, cx)),
            )
            .child(self.queue_panel(cx))
            .child(self.dialog_layer(cx))
            .children(toast.filter(|(_, at)| at.elapsed().as_secs() < 4).map(|(msg, _)| {
                div()
                    .flex()
                    .flex_row()
                    .px_5()
                    .py_2()
                    .bg(rgb(0x1F2937))
                    .text_size(px(11.5))
                    .text_color(rgb(0xFFFFFF))
                    .child(msg)
            }))
    }
}

// ---------------------------------------------------------------------------
// 小部件
// ---------------------------------------------------------------------------

fn track_status(t: &Track) -> (String, u32) {
    if let Some(err) = &t.last_error {
        let short: String = err.chars().take(18).collect();
        return (format!("解析失败 · {}", short), RED);
    }
    if let Some(r) = &t.release {
        if t.has_update() {
            return (format!("↑ 新版本 {}", r.version), ORANGE);
        }
        return (format!("v{}", r.version), GREEN);
    }
    ("等待首次检查".to_string(), FAINT)
}

fn badge(text: impl Into<SharedString>, fg: u32, bg: u32) -> Div {
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(rgb(bg))
        .text_size(px(10.5))
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
                .w(px(11.))
                .h(px(11.))
                .rounded_sm()
                .bg(rgb(if on { ACCENT } else { 0xFFFFFF }))
                .border_1()
                .border_color(rgb(if on { ACCENT } else { BORDER })),
        )
        .child(div().text_size(px(11.)).text_color(rgb(TEXT)).child(text))
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
        .text_size(px(11.5))
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
        .text_size(px(10.5))
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
        let height = if tf.multiline { 78.0 } else { 30.0 };
        let focus = tf.focus.clone();

        div()
            .id(SharedString::from(format!("input-{field:?}")))
            .w_full()
            .h(px(height))
            .px_2()
            .py_2()
            .rounded_md()
            .bg(rgb(0xFFFFFF))
            .border_1()
            .border_color(rgb(if focused { ACCENT } else { BORDER }))
            .text_size(px(11.5))
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
                    .max_h(px(600.))
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
                                    .text_size(px(13.5))
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
                                    .text_size(px(11.5))
                                    .text_color(rgb(MUTED))
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .cursor_pointer()
                                    .child("✕")
                                    .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                            ),
                    )
                    .child(body),
            )
            .into_any_element()
    }

    fn add_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let hint = "每行一个地址，支持 GitHub 仓库 / releases 页、软件站页面、系统镜像页；\
                    也可以直接贴一段含网址的文字，会自动挑出其中的链接。";
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .child(
                div()
                    .text_size(px(11.5))
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
                                .text_size(px(11.))
                                .text_color(rgb(ORANGE))
                                .child(format!("有 {} 行没能识别（已忽略）：", self.add_skipped.len())),
                        )
                        .children(self.add_skipped.iter().take(4).map(|s| {
                            div()
                                .text_size(px(10.5))
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
                        btn("btn-add-ok", "解析并添加", ACCENT, 0xFFFFFF)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_add(cx))),
                    )
                    .child(
                        btn("btn-add-cancel", "取消", 0xF2F4F8, TEXT)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(FAINT))
                            .child("Ctrl+V 粘贴 · Esc 关闭"),
                    ),
            );
        self.modal_shell("添加追踪项", 620., body.into_any_element(), cx)
    }

    fn settings_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let label = |text: &'static str| {
            div()
                .text_size(px(11.))
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
                .text_size(px(13.))
                .text_color(rgb(TEXT))
                .hover(|s| s.bg(rgb(HOVER)))
                .cursor_pointer()
                .child(text)
        };
        let conc = self.cfg.max_concurrent;
        let interval = self.cfg.check_interval_min;

        let dir_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().flex_1().child(self.text_input(FieldId::DownloadDir, "下载根目录", cx)))
            .child(btn("btn-pick-dir", "浏览…", 0xF2F4F8, TEXT).on_click(cx.listener(
                |this, _, _, cx| {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("选择下载根目录")
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
            .child(label("下载目录"))
            .child(dir_row)
            .child(label("GitHub Token（可选，提高 API 限额）"))
            .child(self.text_input(FieldId::Token, "ghp_…（留空则用匿名访问）", cx))
            .child(label("代理（可选，例如 http://127.0.0.1:7890）"))
            .child(self.text_input(FieldId::Proxy, "留空表示直连", cx))
            .child(label("同时下载数"))
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
                            .min_w(px(48.))
                            .text_size(px(11.5))
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
            .child(label("自动检查间隔（0 = 关闭）"))
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
                            .min_w(px(78.))
                            .text_size(px(11.5))
                            .text_color(rgb(TEXT))
                            .child(if interval == 0 {
                                "已关闭".to_string()
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
                        "启动时自动检查",
                        self.cfg.auto_check_on_start,
                        cx,
                        |this| this.cfg.auto_check_on_start = !this.cfg.auto_check_on_start,
                    ))
                    .child(toggle(
                        "toggle-os",
                        "优先当前系统",
                        self.cfg.prefer_current_os,
                        cx,
                        |this| this.cfg.prefer_current_os = !this.cfg.prefer_current_os,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        btn("btn-set-ok", "保存", ACCENT, 0xFFFFFF)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_settings(cx))),
                    )
                    .child(
                        btn("btn-set-cancel", "取消", 0xF2F4F8, TEXT)
                            .on_click(cx.listener(|this, _, _, cx| this.close_dialog(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(FAINT))
                            .child(format!(
                                "v{} · 数据目录 {}",
                                env!("CARGO_PKG_VERSION"),
                                crate::store::data_dir().display()
                            )),
                    ),
            );
        self.modal_shell("设置", 640., body.into_any_element(), cx)
    }

    fn history_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<DownloadRecord> = self.history.iter().take(80).cloned().collect();
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .p_5()
            .when(rows.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(FAINT))
                        .child("还没有下载记录"),
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
                                    .text_size(px(11.5))
                                    .text_color(rgb(TEXT))
                                    .text_ellipsis()
                                    .child(format!("{} · {}", r.app, r.file)),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgb(color))
                                    .child(r.status.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
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
                                    Some(true) => " · SHA-256 通过",
                                    Some(false) => " · SHA-256 不一致",
                                    None => "",
                                }
                            )),
                    )
                    .when_some(r.error.clone(), |d, e| {
                        d.child(
                            div()
                                .text_size(px(10.5))
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
                        btn("btn-hist-clear", "清空历史", RED_SOFT, RED).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.history.clear();
                                crate::store::write_history(&this.history);
                                this.toast("下载历史已清空", cx);
                            },
                        )),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(rgb(FAINT))
                            .child(format!("共 {} 条", self.history.len())),
                    ),
            );
        self.modal_shell("下载历史", 720., body.into_any_element(), cx)
    }

    fn dialog_layer(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.dialog {
            Some(Dialog::Add) => self.add_dialog(cx),
            Some(Dialog::Settings) => self.settings_dialog(cx),
            Some(Dialog::History) => self.history_dialog(cx),
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
                            .text_size(px(12.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("附件"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(FAINT))
                            .child(format!("{} 个", assets.len())),
                    ),
            )
            .when(assets.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(rgb(FAINT))
                        .child("该来源没有列出可下载文件"),
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
                                    .text_size(px(12.))
                                    .text_color(rgb(TEXT))
                                    .text_ellipsis()
                                    .child(a.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(rgb(FAINT))
                                    .text_ellipsis()
                                    .child(format!(
                                        "{}{}{}",
                                        a.size
                                            .map(util::fmt_bytes)
                                            .unwrap_or_else(|| "大小未知".into()),
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
                            if direct { "直链" } else { "页面" },
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
                            "复制",
                            MUTED,
                            0xF2F4F8,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let (t, n) = (copy_text.clone(), copy_name.clone());
                            this.copy(t, &format!("「{n}」的链接"), cx);
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
                            .text_size(px(11.))
                            .text_color(rgb(if direct { 0xFFFFFF } else { TEXT }))
                            .hover(|s| s.opacity(0.88))
                            .cursor_pointer()
                            .child(if direct { "下载" } else { "打开" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.start_asset(&tid, &ver, &asset_for_click, &pg, cx);
                            })),
                    )
            }))
    }
}

fn notes_card(notes: &str, text_w: Pixels) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .p_5()
        .rounded_lg()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .w(text_w)
                .text_size(px(12.5))
                .line_height(px(20.))
                .text_color(rgb(TEXT))
                .whitespace_normal()
                .child(notes.to_string()),
        )
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
                                .text_size(px(12.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .child(h.version.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(rgb(FAINT))
                                .child(h.date.clone().unwrap_or_default()),
                        ),
                )
                .when(!h.title.trim().is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(11.5))
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
                    title: Some("App Monitor · 软件追踪下载器".into()),
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
    use super::EditState;

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
