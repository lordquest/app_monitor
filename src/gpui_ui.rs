//! 实验性 GPUI 前端（`app-monitor --gpui`，需 `--features gpui-ui` 编译）。
//!
//! 与 egui 版并存而不是替换，原因有两个：
//!
//! 1. GPUI 的 Windows 后端走 Direct3D 11 + DirectComposition，实际上要求
//!    Windows 10 及以上；egui 版还承担 Win7/8 的 glow(OpenGL) 兜底。
//! 2. 迁移是逐屏推进的，先把只读骨架跑起来，再逐个接上解析、下载、弹窗，
//!    中途随时可以退回 egui 版（它仍是默认入口）。
//!
//! 非 UI 层（model / store / provider / download / engine / util）完全复用，
//! 这里只负责把同一份数据用 GPUI 画出来。

use std::collections::HashMap;
use std::time::Instant;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext, Application, Bounds, Context, Div, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Stateful, StatefulInteractiveElement, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, prelude::FluentBuilder, px, relative,
    rgb, size,
};

use crate::app::TaskStatus;
use crate::download::DlSpec;
use crate::engine::{Engine, UiMsg};
use crate::http::Http;
use crate::model::{Asset, AssetKind, Config, DownloadRecord, Release, Track};
use crate::store::Store;
use crate::util;

/// 一条下载任务。字段与 egui 版一致，但渲染与事件由 GPUI 自己处理。
struct DlTask {
    spec: DlSpec,
    status: TaskStatus,
    downloaded: u64,
    total: u64,
    speed: f64,
    sha_ok: Option<bool>,
    error: Option<String>,
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
// 调色板：与 egui 版保持一致，两套前端看起来是同一个软件。
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

#[derive(Clone, Copy, PartialEq, Eq)]
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

pub struct GpuiApp {
    tracks: Vec<Track>,
    #[allow(dead_code)]
    history: Vec<DownloadRecord>,
    cfg: Config,
    selected: Option<String>,
    tab: Tab,
    candidate_sel: HashMap<String, usize>,
    /// 数据文件损坏时的提示（与 egui 版共用同一套隔离逻辑）。
    issues: Vec<crate::store::LoadIssue>,
    toast: Option<(String, Instant)>,
    engine: Engine,
    http: Arc<Http>,
    tasks: Vec<DlTask>,
    resolving: std::collections::HashSet<String>,
}

impl GpuiApp {
    fn new(cx: &mut Context<Self>) -> Self {
        let store = Store::load();
        let selected = store.tracks.first().map(|t| t.id.clone());
        if store.seeded {
            crate::store::write_tracks(&store.tracks);
        }
        let http = Arc::new(Http::new(&store.config.proxy, None).expect("http client"));
        let mut app = Self {
            tracks: store.tracks,
            history: store.history,
            cfg: store.config,
            selected,
            tab: Tab::Overview,
            candidate_sel: HashMap::new(),
            issues: store.issues,
            toast: None,
            engine: Engine::new(),
            http,
            tasks: Vec::new(),
            resolving: std::collections::HashSet::new(),
        };
        app.spawn_pump(cx);
        // 排障钩子（与 `--download` 同理）：GPUI 版的界面不好在无头环境里点按钮，
        // 这个开关让它在启动后自己下最小的一个直链，用来验证队列与进度渲染。
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
        cx.spawn(|this: gpui::WeakEntity<GpuiApp>, cx: &mut gpui::AsyncApp| {
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
                    if let Some(task) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        task.status = TaskStatus::Done;
                        task.sha_ok = sha_ok;
                        task.downloaded = downloaded;
                        if task.total == 0 {
                            task.total = downloaded;
                        }
                        task.speed = 0.0;
                        task.error = None;
                        let name = task.spec.file.clone();
                        let ok = sha_ok;
                        self.toast = Some((
                            match ok {
                                Some(true) => format!("{name} 下载完成，SHA-256 校验通过"),
                                Some(false) => format!("{name} 下载完成，但 SHA-256 不一致"),
                                None => format!("{name} 下载完成（无官方摘要）"),
                            },
                            Instant::now(),
                        ));
                    }
                    let _ = path;
                    changed = true;
                }
                UiMsg::DlFailed {
                    id,
                    error,
                    downloaded,
                    total,
                } => {
                    if let Some(task) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        task.status = TaskStatus::Failed;
                        task.error = Some(error);
                        task.downloaded = downloaded;
                        task.total = total;
                        task.speed = 0.0;
                    }
                    changed = true;
                }
                UiMsg::DlCanceled {
                    id,
                    paused,
                    downloaded,
                    total,
                } => {
                    if let Some(task) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        task.status = if paused {
                            TaskStatus::Paused
                        } else {
                            TaskStatus::Canceled
                        };
                        task.downloaded = downloaded;
                        task.total = total;
                        task.speed = 0.0;
                    }
                    changed = true;
                }
            }
        }
        if changed {
            crate::store::write_tracks(&self.tracks);
            cx.notify();
        }
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

    /// 与 egui 版 `App::start_asset` 同规则：非直链只交给浏览器，直链落到
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
                            .child("跨平台软件追踪下载器 · GPUI 预览"),
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
            .child(btn("btn-add", "+ 添加追踪", 0xF2F4F8, TEXT).on_click(cx.listener(
                |this, _, _, cx| this.toast("添加弹窗待实现", cx),
            )))
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, t) in self.tracks.iter().enumerate() {
            match groups.iter_mut().find(|(g, _)| g == &t.group) {
                Some((_, v)) => v.push(i),
                None => groups.push((t.group.clone(), vec![i])),
            }
        }
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
                            .child(format!("{}", self.tracks.len())),
                    ),
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

    fn main_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .h_full()
            .bg(rgb(BG))
            .child(self.detail_header(&track, view.as_ref(), cx))
            .child(
                div()
                    .id("detail-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .gap_3()
                    .px_5()
                    .pb_5()
                    .overflow_y_scroll()
                    .child(match view.as_ref() {
                        Some(r) => self.release_card(&track, r, sel, cx),
                        None => self.empty_card(&track),
                    })
                    .when(self.tab == Tab::Overview, |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .when_some(view.clone(), |d, r| {
                                    d.child(self.asset_card(&track, &r, cx))
                                }),
                        )
                    })
                    .when_some(
                        match (self.tab, view.as_ref()) {
                            (Tab::Notes, Some(r)) if !r.notes.trim().is_empty() => Some(r.notes.clone()),
                            _ => None,
                        },
                        |d, notes| d.child(notes_card(&notes)),
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

    /// 底部队列：只在有任务时出现，样式与 egui 版保持一致。
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
                    .child(format!("来源：{}", track.url)),
            )
            .into_any_element()
    }

    fn release_card(
        &self,
        track: &Track,
        r: &Release,
        sel: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let url = r.page_url.clone();
        div()
            .flex()
            .flex_col()
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
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .child(
                                div()
                                    .text_size(px(26.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(format!("v{}", r.version)),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(rgb(MUTED))
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
                        btn("btn-open-page", "打开来源页", 0xF2F4F8, TEXT).on_click(cx.listener(
                            move |this, _, _, cx| {
                                let _ = open::that_detached(&url);
                                this.toast("已在浏览器打开来源页", cx);
                            },
                        )),
                    ),
            )
            .when(!r.summary.trim().is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(rgb(TEXT))
                        .whitespace_normal()
                        .child(r.summary.clone()),
                )
            })
            .children((!r.candidates.is_empty()).then(|| {
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

impl Render for GpuiApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toast = self.toast.clone();
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .font_family("Microsoft YaHei UI")
            .text_color(rgb(TEXT))
            .children(
                (!self.issues.is_empty()).then(|| {
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .px_5()
                        .py_2()
                        .bg(rgb(ORANGE_SOFT))
                        .text_size(px(11.5))
                        .child(div().text_color(rgb(ORANGE)).font_weight(FontWeight::SEMIBOLD).child("!"))
                        .child(
                            div()
                                .text_color(rgb(TEXT))
                                .child(format!(
                                    "{} 个数据文件未能读取，已改用默认内容（原文件已备份为 .corrupt）",
                                    self.issues.len()
                                )),
                        )
                }),
            )
            .child(self.top_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.sidebar(cx))
                    .child(self.main_panel(cx)),
            )
            .child(self.queue_panel(cx))
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

fn btn(id: &'static str, text: &'static str, bg: u32, fg: u32) -> Stateful<Div> {
    div()
        .id(id)
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

impl GpuiApp {
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
                div()
                    .id(SharedString::from(row_id))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
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
                    .child(badge(
                        if direct { "直链" } else { "页面" },
                        if direct { GREEN } else { MUTED },
                        if direct { GREEN_SOFT } else { 0xF2F4F8 },
                    ))
                    .child(
                        div()
                            .id(SharedString::from(btn_id))
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

fn notes_card(notes: &str) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .p_5()
        .rounded_lg()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
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

/// `--gpui` 入口：与 egui 版共用数据目录和 provider 层，只是换了一套绘制。
pub fn run() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1180.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("App Monitor · GPUI 预览".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| GpuiApp::new(cx)),
        )
        .expect("open_window");
        cx.activate(true);
    });
}

