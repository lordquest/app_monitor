//! Application shell: state, message pumping, top bar / sidebar / download queue.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, FontId, Margin, RichText, Rounding, Stroke, Vec2};

use crate::download::DlSpec;
use crate::engine::{Engine, UiMsg};
use crate::http::Http;
use crate::model::{Asset, AssetKind, Config, DownloadRecord, Release, Track};
use crate::store::Store;
use crate::util;

// ---------------------------------------------------------------------------
// Palette — light, calm, one accent colour.
// ---------------------------------------------------------------------------

pub const BG: Color32 = Color32::from_rgb(0xF4, 0xF6, 0xF9);
pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
pub const SIDEBAR: Color32 = Color32::from_rgb(0xFA, 0xFB, 0xFD);
pub const BORDER: Color32 = Color32::from_rgb(0xE2, 0xE6, 0xED);
pub const TEXT: Color32 = Color32::from_rgb(0x1B, 0x20, 0x27);
pub const MUTED: Color32 = Color32::from_rgb(0x6B, 0x74, 0x85);
pub const FAINT: Color32 = Color32::from_rgb(0x9A, 0xA3, 0xB2);
pub const ACCENT: Color32 = Color32::from_rgb(0x25, 0x63, 0xEB);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(0xE8, 0xF0, 0xFE);
pub const GREEN: Color32 = Color32::from_rgb(0x15, 0x80, 0x3D);
pub const GREEN_SOFT: Color32 = Color32::from_rgb(0xE7, 0xF6, 0xEC);
pub const ORANGE: Color32 = Color32::from_rgb(0xB4, 0x53, 0x09);
pub const ORANGE_SOFT: Color32 = Color32::from_rgb(0xFD, 0xF1, 0xE3);
pub const RED: Color32 = Color32::from_rgb(0xB9, 0x1C, 0x1C);
pub const RED_SOFT: Color32 = Color32::from_rgb(0xFD, 0xEC, 0xEC);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Notes,
    History,
}

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
    pub(crate) fn label(self) -> &'static str {
        match self {
            TaskStatus::Queued => "排队中",
            TaskStatus::Running => "下载中",
            TaskStatus::Paused => "已暂停",
            TaskStatus::Done => "已完成",
            TaskStatus::Failed => "失败",
            TaskStatus::Canceled => "已取消",
        }
    }
    pub(crate) fn color(self) -> Color32 {
        match self {
            TaskStatus::Running => ACCENT,
            TaskStatus::Done => GREEN,
            TaskStatus::Failed => RED,
            TaskStatus::Paused => ORANGE,
            TaskStatus::Canceled => FAINT,
            TaskStatus::Queued => MUTED,
        }
    }
}

pub struct Task {
    pub spec: DlSpec,
    pub status: TaskStatus,
    pub downloaded: u64,
    pub total: u64,
    pub speed: f64,
    pub error: Option<String>,
    pub sha_ok: Option<bool>,
    pub path: Option<String>,
    pub started_at: i64,
    pub cancel: Arc<AtomicBool>,
    pub paused: Arc<AtomicBool>,
}

impl Task {
    fn new(spec: DlSpec) -> Self {
        Self {
            spec,
            status: TaskStatus::Queued,
            downloaded: 0,
            total: 0,
            speed: 0.0,
            error: None,
            sha_ok: None,
            path: None,
            started_at: util::now_secs(),
            cancel: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }
    pub(crate) fn frac(&self) -> f32 {
        if self.total == 0 {
            if self.status == TaskStatus::Done {
                1.0
            } else {
                0.0
            }
        } else {
            (self.downloaded as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }
}

pub struct SettingsDraft {
    pub dir: String,
    pub token: String,
    pub proxy: String,
    pub concurrent: usize,
    pub interval: u64,
    pub auto_start: bool,
    pub prefer_os: bool,
}

impl From<&Config> for SettingsDraft {
    fn from(cfg: &Config) -> Self {
        Self {
            dir: cfg.download_dir.clone(),
            token: cfg.github_token.clone(),
            proxy: cfg.proxy.clone(),
            concurrent: cfg.max_concurrent,
            interval: cfg.check_interval_min,
            auto_start: cfg.auto_check_on_start,
            prefer_os: cfg.prefer_current_os,
        }
    }
}

/// Which item / candidate tab the GUI opens on, set from `--select` / `--cand`.
/// Only a launch hint: it never writes to `tracks.json`.
static START_HINT: OnceLock<(Option<String>, usize)> = OnceLock::new();

pub fn set_start_hint(name: Option<String>, candidate: usize) {
    let _ = START_HINT.set((name, candidate));
}

pub struct App {
    pub(crate) engine: Engine,
    pub(crate) http: Arc<Http>,
    pub(crate) cfg: Config,
    pub(crate) tracks: Vec<Track>,
    pub(crate) history: Vec<DownloadRecord>,
    pub(crate) selected: Option<String>,
    pub(crate) filter: String,
    pub(crate) tab: Tab,
    pub(crate) tasks: Vec<Task>,
    pub(crate) resolving: HashSet<String>,
    pub(crate) show_add: bool,
    pub(crate) add_text: String,
    pub(crate) add_preview: Vec<Track>,
    pub(crate) add_skipped: Vec<String>,
    pub(crate) show_settings: bool,
    pub(crate) show_history: bool,
    pub(crate) show_about: bool,
    pub(crate) settings: SettingsDraft,
    pub(crate) history_filter: String,
    pub(crate) toast: Option<(String, Instant)>,
    pub(crate) last_auto: Instant,
    pub(crate) ctx: Option<egui::Context>,
    pub(crate) remove_target: Option<String>,
    /// Track id → index into `Release::candidates` currently shown in the detail
    /// page. 0 is the primary (stable) pick.
    pub(crate) candidate_sel: HashMap<String, usize>,
}

impl App {
    pub fn new() -> Self {
        let store = Store::load();
        let http = make_http(&store.config);
        let mut app = Self {
            engine: Engine::new(),
            http,
            settings: SettingsDraft::from(&store.config),
            cfg: store.config,
            tracks: store.tracks,
            history: store.history,
            selected: None,
            filter: String::new(),
            tab: Tab::Overview,
            tasks: Vec::new(),
            resolving: HashSet::new(),
            show_add: false,
            add_text: String::new(),
            add_preview: Vec::new(),
            add_skipped: Vec::new(),
            show_settings: false,
            show_history: false,
            show_about: false,
            history_filter: String::new(),
            toast: None,
            last_auto: Instant::now(),
            ctx: None,
            remove_target: None,
            candidate_sel: HashMap::new(),
        };
        app.selected = app.tracks.first().map(|t| t.id.clone());
        if let Some((Some(name), cand)) = START_HINT.get() {
            let needle = name.to_lowercase();
            if let Some(t) = app
                .tracks
                .iter()
                .find(|t| t.name.to_lowercase().contains(&needle))
            {
                app.selected = Some(t.id.clone());
                app.candidate_sel.insert(t.id.clone(), *cand);
            }
        }
        app.save_tracks();
        if app.cfg.auto_check_on_start {
            app.resolve_all();
        }
        app
    }

    // ---- persistence ----

    pub(crate) fn save_tracks(&self) {
        let store = Store {
            config: self.cfg.clone(),
            tracks: self.tracks.clone(),
            history: Vec::new(),
            seeded: false,
        };
        store.save_tracks();
    }

    pub(crate) fn save_history(&self) {
        let store = Store {
            config: self.cfg.clone(),
            tracks: Vec::new(),
            history: self.history.clone(),
            seeded: false,
        };
        store.save_history();
    }

    pub(crate) fn save_config(&self) {
        let store = Store {
            config: self.cfg.clone(),
            tracks: Vec::new(),
            history: Vec::new(),
            seeded: false,
        };
        store.save_config();
    }

    pub(crate) fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    // ---- resolution ----

    pub(crate) fn resolve(&mut self, id: &str) {
        if self.resolving.contains(id) {
            return;
        }
        let Some(track) = self.tracks.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        self.resolving.insert(id.to_string());
        self.engine.start_resolve(self.http.clone(), track);
    }

    pub(crate) fn resolve_all(&mut self) {
        let ids: Vec<String> = self
            .tracks
            .iter()
            .filter(|t| t.enabled)
            .map(|t| t.id.clone())
            .collect();
        for id in ids {
            self.resolve(&id);
        }
    }

    pub(crate) fn track(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub(crate) fn track_mut(&mut self, id: &str) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Which candidate tab is showable right now, clamped to what the release has.
    /// A refresh can shrink the list (fewer release lines), so never trust the map.
    pub(crate) fn candidate_index(&self, track_id: &str, release: &Release) -> usize {
        let n = release.candidates.len();
        if n == 0 {
            return 0;
        }
        (*self.candidate_sel.get(track_id).unwrap_or(&0)).min(n - 1)
    }

    // ---- downloads ----

    /// Queue (or open) one asset. `version` comes from the release *view* the user
    /// is looking at, which may be a candidate tab rather than the primary pick.
    pub(crate) fn start_asset(&mut self, track_id: &str, version: &str, asset: &Asset) {
        let Some(track) = self.track(track_id).cloned() else {
            return;
        };
        let Some(release) = track.release.clone() else {
            return;
        };
        let asset = asset.clone();
        if asset.kind != AssetKind::Direct {
            let _ = open::that_detached(&asset.url);
            self.toast(if asset.kind == AssetKind::Pan {
                "已在浏览器中打开网盘链接"
            } else {
                "已在浏览器中打开页面"
            });
            return;
        }
        let version = if version.trim().is_empty() {
            release.version.clone()
        } else {
            version.trim().to_string()
        };
        let dir = PathBuf::from(self.cfg.download_dir.trim())
            .join(crate::download::sanitize(&track.name))
            .join(crate::download::sanitize(&version));
        // Send the browser-equivalent referer for the release actually chosen.
        let page = release
            .candidates
            .iter()
            .find(|c| c.version == version)
            .map(|c| c.page_url.clone())
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| release.page_url.clone());
        let id = format!("dl-{}", util::now_secs() as u128 * 1000 + (self.tasks.len() as u128 % 1000));
        let spec = DlSpec {
            id,
            track_id: track.id.clone(),
            app: track.name.clone(),
            version,
            file: asset.name.clone(),
            url: asset.url.clone(),
            dir,
            sha256: asset.sha256.clone(),
            referer: Some(if page.is_empty() {
                track.url.clone()
            } else {
                page
            }),
        };
        if self
            .tasks
            .iter()
            .any(|t| t.spec.url == spec.url && matches!(t.status, TaskStatus::Running | TaskStatus::Queued))
        {
            self.toast("该文件已在下载队列中");
            return;
        }
        self.tasks.insert(0, Task::new(spec));
        self.toast("已加入下载队列");
        self.pump_queue();
    }

    pub(crate) fn pump_queue(&mut self) {
        let max = self.cfg.max_concurrent.max(1);
        let running = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count();
        if running >= max {
            return;
        }
        let mut budget = max - running;
        let mut starts: Vec<(DlSpec, Arc<AtomicBool>, Arc<AtomicBool>)> = Vec::new();
        for t in self.tasks.iter_mut() {
            if budget == 0 {
                break;
            }
            if t.status == TaskStatus::Queued {
                t.status = TaskStatus::Running;
                starts.push((t.spec.clone(), t.cancel.clone(), t.paused.clone()));
                budget -= 1;
            }
        }
        for (spec, cancel, paused) in starts {
            self.engine
                .start_download(self.http.clone(), spec, cancel, paused);
        }
    }

    pub(crate) fn pause_task(&mut self, id: &str) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
            match t.status {
                TaskStatus::Running => {
                    t.paused.store(true, Ordering::Relaxed);
                    t.cancel.store(true, Ordering::Relaxed);
                }
                TaskStatus::Queued => t.status = TaskStatus::Paused,
                _ => {}
            }
        }
    }

    pub(crate) fn resume_task(&mut self, id: &str) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
            if matches!(
                t.status,
                TaskStatus::Paused | TaskStatus::Failed | TaskStatus::Canceled
            ) {
                t.status = TaskStatus::Queued;
                t.error = None;
                t.cancel = Arc::new(AtomicBool::new(false));
                t.paused = Arc::new(AtomicBool::new(false));
            }
        }
        self.pump_queue();
    }

    pub(crate) fn cancel_task(&mut self, id: &str) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
            match t.status {
                TaskStatus::Running => {
                    t.paused.store(false, Ordering::Relaxed);
                    t.cancel.store(true, Ordering::Relaxed);
                }
                TaskStatus::Queued | TaskStatus::Paused => t.status = TaskStatus::Canceled,
                _ => {}
            }
        }
    }

    pub(crate) fn retry_task(&mut self, id: &str) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
            t.status = TaskStatus::Queued;
            t.error = None;
            t.downloaded = 0;
            t.total = 0;
            t.speed = 0.0;
            t.cancel = Arc::new(AtomicBool::new(false));
            t.paused = Arc::new(AtomicBool::new(false));
        }
        self.pump_queue();
    }

    pub(crate) fn clear_finished(&mut self) {
        self.tasks.retain(|t| {
            !matches!(
                t.status,
                TaskStatus::Done | TaskStatus::Failed | TaskStatus::Canceled
            )
        });
    }

    fn record(&mut self, task: &Task, status: &str, error: Option<String>) {
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
        self.save_history();
    }

    // ---- messages ----

    fn drain(&mut self, ctx: &egui::Context) {
        let mut dirty = false;
        while let Ok(msg) = self.engine.rx.try_recv() {
            dirty = true;
            match msg {
                UiMsg::Resolved(id, release) => {
                    let release: Release = *release;
                    let mut new_version = None;
                    if let Some(t) = self.track_mut(&id) {
                        if t.seen_version.is_none() {
                            t.seen_version = Some(release.version.clone());
                        }
                        if let Some(prev) = &t.release {
                            if prev.version != release.version {
                                new_version = Some((prev.version.clone(), release.version.clone()));
                            }
                        }
                        t.last_error = None;
                        t.last_checked = Some(util::now_secs());
                        t.release = Some(release.clone());
                    }
                    self.resolving.remove(&id);
                    self.save_tracks();
                    if let Some((from, to)) = new_version {
                        let name = self
                            .track(&id)
                            .map(|t| t.name.clone())
                            .unwrap_or_default();
                        self.toast(format!("{name}: 发现新版本 {from} → {to}"));
                    }
                }
                UiMsg::ResolveFailed(id, err) => {
                    if let Some(t) = self.track_mut(&id) {
                        t.last_error = Some(err);
                        t.last_checked = Some(util::now_secs());
                    }
                    self.resolving.remove(&id);
                    self.save_tracks();
                }
                UiMsg::DlProgress { id, p } => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        if t.status != TaskStatus::Running {
                            t.status = TaskStatus::Running;
                        }
                        t.downloaded = p.downloaded;
                        if p.total > 0 {
                            t.total = p.total;
                        }
                        t.speed = p.speed;
                    }
                }
                UiMsg::DlDone {
                    id,
                    path,
                    sha_ok,
                    sha_actual: _,
                    downloaded,
                    ..
                } => {
                    let mut rec = None;
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        t.status = TaskStatus::Done;
                        t.downloaded = downloaded;
                        if t.total == 0 {
                            t.total = downloaded;
                        }
                        t.speed = 0.0;
                        t.sha_ok = sha_ok;
                        t.path = Some(path);
                        rec = Some(t.clone_for_record());
                    }
                    if let Some(t) = rec {
                        self.record(&t, "完成", None);
                        self.toast(match sha_ok {
                            Some(true) => "下载完成，SHA-256 校验通过",
                            Some(false) => "下载完成，但校验未通过",
                            None => "下载完成",
                        });
                    }
                    self.pump_queue();
                }
                UiMsg::DlFailed {
                    id,
                    error,
                    downloaded,
                    total,
                } => {
                    let mut rec = None;
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        t.status = TaskStatus::Failed;
                        t.downloaded = downloaded;
                        if total > 0 {
                            t.total = total;
                        }
                        t.speed = 0.0;
                        t.error = Some(error.clone());
                        rec = Some(t.clone_for_record());
                    }
                    if let Some(t) = rec {
                        self.record(&t, "失败", Some(error));
                    }
                    self.pump_queue();
                }
                UiMsg::DlCanceled {
                    id,
                    paused,
                    downloaded,
                    total,
                } => {
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.spec.id == id) {
                        t.status = if paused {
                            TaskStatus::Paused
                        } else {
                            TaskStatus::Canceled
                        };
                        t.downloaded = downloaded;
                        if total > 0 {
                            t.total = total;
                        }
                        t.speed = 0.0;
                    }
                    self.pump_queue();
                }
            }
        }
        if dirty {
            ctx.request_repaint();
        }
        let busy = self
            .tasks
            .iter()
            .any(|t| t.status == TaskStatus::Running)
            || !self.resolving.is_empty();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(160));
        }
    }

    fn tick_auto(&mut self, ctx: &egui::Context) {
        let interval = self.cfg.check_interval_min;
        if interval == 0 {
            return;
        }
        if self.last_auto.elapsed() >= Duration::from_secs(interval * 60) {
            self.last_auto = Instant::now();
            self.resolve_all();
        }
        if self.toast.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    /// Keyboard shortcuts: tabs, dialogs and "download the primary file".
    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, Modifiers};
        let (t1, t2, t3, hist, set, add, dl, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::CTRL, Key::Num1),
                i.consume_key(Modifiers::CTRL, Key::Num2),
                i.consume_key(Modifiers::CTRL, Key::Num3),
                i.consume_key(Modifiers::CTRL, Key::H),
                i.consume_key(Modifiers::CTRL, Key::Comma),
                i.consume_key(Modifiers::CTRL, Key::N),
                i.consume_key(Modifiers::CTRL, Key::D),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if t1 {
            self.tab = Tab::Overview;
        }
        if t2 {
            self.tab = Tab::Notes;
        }
        if t3 {
            self.tab = Tab::History;
        }
        if hist {
            self.show_history = true;
        }
        if set {
            self.settings = SettingsDraft::from(&self.cfg);
            self.show_settings = true;
        }
        if add {
            self.add_text.clear();
            self.add_preview.clear();
            self.add_skipped.clear();
            self.show_add = true;
        }
        if esc {
            self.show_add = false;
            self.show_settings = false;
            self.show_history = false;
            self.show_about = false;
            self.remove_target = None;
        }
        if dl {
            let target = self.selected.clone().and_then(|id| {
                let t = self.track(&id)?;
                // Download from the tab the user is looking at, not always the stable one.
                let sel = *self.candidate_sel.get(&id).unwrap_or(&0);
                let r = t.release.as_ref()?.view_of(sel);
                let a = r.assets.iter().find(|a| a.kind == AssetKind::Direct)?.clone();
                Some((id, r.version, a))
            });
            if let Some((id, version, asset)) = target {
                self.start_asset(&id, &version, &asset);
            } else {
                self.toast("当前条目没有可直接下载的文件");
            }
        }
    }

    // ---- shell ----

    pub(crate) fn active_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Queued | TaskStatus::Paused))
            .count()
    }

    pub(crate) fn update_badge_count(&self) -> usize {
        self.tracks.iter().filter(|t| t.has_update()).count()
    }
}

impl Task {
    fn clone_for_record(&self) -> Task {
        Task {
            spec: self.spec.clone(),
            status: self.status,
            downloaded: self.downloaded,
            total: self.total,
            speed: self.speed,
            error: self.error.clone(),
            sha_ok: self.sha_ok,
            path: self.path.clone(),
            started_at: self.started_at,
            cancel: self.cancel.clone(),
            paused: self.paused.clone(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ctx = Some(ctx.clone());
        self.drain(ctx);
        self.tick_auto(ctx);
        self.shortcuts(ctx);

        self.top_bar(ctx);
        // Bottom panel first: it spans the full width, so the sidebar must be laid
        // out after it or its footer would be covered.
        self.queue_panel(ctx);
        self.sidebar(ctx);
        self.central(ctx);
        self.windows(ctx);
        self.toast_overlay(ctx);
    }
}

// ---------------------------------------------------------------------------
// Shared widget helpers
// ---------------------------------------------------------------------------

pub(crate) fn card<R>(ui: &mut egui::Ui, f: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(Rounding::same(10.0))
        .inner_margin(Margin::same(14.0))
        .show(ui, f)
        .inner
}

pub(crate) fn badge(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::none()
        .fill(bg)
        .rounding(Rounding::same(4.0))
        .inner_margin(Margin::symmetric(6.0, 2.0))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.0).color(fg));
        });
}

/// Small coloured dot used by status rows in the sidebar.
#[allow(dead_code)]
pub(crate) fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

pub(crate) fn ellipsis(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

pub(crate) fn primary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(Color32::WHITE).size(13.0))
        .fill(ACCENT)
        .rounding(Rounding::same(6.0))
}

pub(crate) fn subtle_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(TEXT).size(12.5))
        .fill(Color32::from_rgb(0xF2, 0xF4, 0xF8))
        .stroke(Stroke::new(1.0_f32, BORDER))
        .rounding(Rounding::same(6.0))
}

pub(crate) fn make_http(cfg: &Config) -> Arc<Http> {
    let token = if cfg.github_token.trim().is_empty() {
        None
    } else {
        Some(cfg.github_token.trim().to_string())
    };
    Http::new(&cfg.proxy, token)
        .map(Arc::new)
        .unwrap_or_else(|_| Arc::new(Http::new("", None).expect("http")))
}

pub(crate) fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::light();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.faint_bg_color = Color32::from_rgb(0xF6, 0xF7, 0xFA);
    v.extreme_bg_color = Color32::from_rgb(0xF2, 0xF4, 0xF8);
    v.selection.bg_fill = Color32::from_rgb(0xD5, 0xE3, 0xFF);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.inactive.bg_fill = Color32::from_rgb(0xF3, 0xF5, 0xF9);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0xE9, 0xEF, 0xFA);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(0xC9, 0xD5, 0xE8));
    v.widgets.active.bg_fill = ACCENT_SOFT;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.window_rounding = Rounding::same(10.0);
    style.visuals = v;
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    style.spacing.button_padding = Vec2::new(10.0, 5.0);
    style.spacing.interact_size.y = 24.0;
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(14.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(13.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        FontId::new(11.5, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Heading,
        FontId::new(19.0, egui::FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        FontId::new(12.5, egui::FontFamily::Monospace),
    );
    ctx.set_style(style);
}
