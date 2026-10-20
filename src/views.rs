//! All the visible surfaces: top bar, sidebar, detail page, download queue and
//! the modal windows (add / settings / history / about).

use std::path::PathBuf;
use std::time::Duration;

use egui::{Align, Align2, Color32, FontId, Layout, Margin, RichText, Rounding, ScrollArea, Stroke, Vec2};

use crate::app::{
    badge, card, ellipsis, primary_button, subtle_button, App, SettingsDraft, Tab, TaskStatus,
    ACCENT, ACCENT_SOFT, BG, BORDER, CARD, FAINT, GREEN, GREEN_SOFT, MUTED, ORANGE, ORANGE_SOFT,
    RED, RED_SOFT, SIDEBAR, TEXT,
};
use crate::model::{Asset, AssetKind, Release, SourceKind, Track};
use crate::parse;
use crate::util;

struct TaskView {
    id: String,
    app: String,
    file: String,
    status: TaskStatus,
    frac: f32,
    downloaded: u64,
    total: u64,
    speed: f64,
    error: Option<String>,
    sha_ok: Option<bool>,
    path: Option<String>,
}

enum Act {
    Pause(String),
    Resume(String),
    Cancel(String),
    Retry(String),
    Open(String),
    OpenDir(PathBuf),
}

impl App {
    // -----------------------------------------------------------------------
    // Top bar
    // -----------------------------------------------------------------------

    pub(crate) fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top")
            .frame(
                egui::Frame::none()
                    .fill(CARD)
                    .inner_margin(Margin::symmetric(18.0, 11.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("App Monitor").size(19.0).strong().color(TEXT));
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new("跨平台软件追踪下载器")
                            .size(12.0)
                            .color(MUTED),
                    );
                    ui.add_space(12.0);
                    let total = self.tracks.len();
                    let updates = self.update_badge_count();
                    badge(ui, &format!("{total} 个追踪项"), MUTED, Color32::from_rgb(0xF2, 0xF4, 0xF8));
                    if updates > 0 {
                        badge(ui, &format!("{updates} 项有更新"), ORANGE, ORANGE_SOFT);
                    }
                    if !self.resolving.is_empty() {
                        ui.spinner();
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("设置").clicked() {
                            self.settings = SettingsDraft::from(&self.cfg);
                            self.show_settings = true;
                        }
                        if ui.button("历史记录").clicked() {
                            self.show_history = true;
                        }
                        ui.add_space(4.0);
                        if ui.add(primary_button("+ 添加追踪")).clicked() {
                            self.add_text.clear();
                            self.add_preview.clear();
                            self.add_skipped.clear();
                            self.show_add = true;
                        }
                        let busy = !self.resolving.is_empty();
                        if ui
                            .add_enabled(!busy, egui::Button::new("检查全部更新"))
                            .clicked()
                        {
                            self.resolve_all();
                            self.toast("已开始检查全部追踪项");
                        }
                    });
                });
            });
    }

    // -----------------------------------------------------------------------
    // Sidebar
    // -----------------------------------------------------------------------

    pub(crate) fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .exact_width(272.0)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::symmetric(10.0, 12.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("追踪列表").size(12.0).strong().color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{}", self.tracks.len()))
                                .size(11.0)
                                .color(FAINT),
                        );
                    });
                });
                ui.add_space(2.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("筛选名称或站点…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(6.0);

                struct Row {
                    id: String,
                    name: String,
                    sub: String,
                    color: Color32,
                    resolving: bool,
                }
                let filter = self.filter.trim().to_lowercase();
                let mut groups: Vec<(String, Vec<Row>)> = Vec::new();
                for t in &self.tracks {
                    if !filter.is_empty()
                        && !t.name.to_lowercase().contains(&filter)
                        && !t.url.to_lowercase().contains(&filter)
                        && !t.group.to_lowercase().contains(&filter)
                    {
                        continue;
                    }
                    let (sub, color) = if let Some(err) = &t.last_error {
                        (
                            format!("解析失败 · {}", ellipsis(err, 20)),
                            RED,
                        )
                    } else if let Some(r) = &t.release {
                        if t.has_update() {
                            (format!("↑ 新版本 {}", ellipsis(&r.version, 14)), ORANGE)
                        } else {
                            (format!("v{}", ellipsis(&r.version, 16)), GREEN)
                        }
                    } else {
                        ("等待首次检查".to_string(), FAINT)
                    };
                    let row = Row {
                        id: t.id.clone(),
                        name: t.name.clone(),
                        sub,
                        color,
                        resolving: self.resolving.contains(&t.id),
                    };
                    match groups.iter_mut().find(|(g, _)| g == &t.group) {
                        Some((_, v)) => v.push(row),
                        None => groups.push((t.group.clone(), vec![row])),
                    }
                }

                let mut clicked: Option<String> = None;
                let mut remove: Option<String> = None;
                let mut refresh: Option<String> = None;
                // Leave room for the footer line, otherwise the scroll area eats it.
                let list_h = (ui.available_height() - 38.0).max(120.0);
                ScrollArea::vertical()
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if groups.is_empty() {
                            ui.add_space(20.0);
                            ui.label(
                                RichText::new("没有匹配的追踪项")
                                    .size(12.0)
                                    .color(FAINT),
                            );
                        }
                        for (group, rows) in groups {
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(format!("{} · {}", group, rows.len()))
                                    .size(10.5)
                                    .color(FAINT),
                            );
                            ui.add_space(2.0);
                            for row in rows {
                                let selected = self.selected.as_deref() == Some(row.id.as_str());
                                let (rect, resp) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), 44.0),
                                    egui::Sense::click(),
                                );
                                let p = ui.painter();
                                if selected {
                                    p.rect_filled(rect, Rounding::same(7.0), ACCENT_SOFT);
                                    p.rect_filled(
                                        egui::Rect::from_min_size(
                                            rect.left_top() + Vec2::new(0.0, 7.0),
                                            Vec2::new(3.0, rect.height() - 14.0),
                                        ),
                                        Rounding::same(2.0),
                                        ACCENT,
                                    );
                                } else if resp.hovered() {
                                    p.rect_filled(
                                        rect,
                                        Rounding::same(7.0),
                                        Color32::from_rgb(0xF0, 0xF3, 0xF8),
                                    );
                                }
                                p.text(
                                    egui::Pos2::new(rect.left() + 11.0, rect.top() + 7.0),
                                    Align2::LEFT_TOP,
                                    ellipsis(&row.name, 20),
                                    FontId::proportional(13.5),
                                    TEXT,
                                );
                                p.text(
                                    egui::Pos2::new(rect.left() + 11.0, rect.top() + 25.0),
                                    Align2::LEFT_TOP,
                                    if row.resolving {
                                        "正在解析…".to_string()
                                    } else {
                                        row.sub.clone()
                                    },
                                    FontId::proportional(10.5),
                                    if row.resolving { ACCENT } else { row.color },
                                );
                                p.circle_filled(
                                    egui::Pos2::new(rect.right() - 12.0, rect.center().y),
                                    3.5,
                                    if row.resolving { ACCENT } else { row.color },
                                );
                                if resp.clicked() {
                                    clicked = Some(row.id.clone());
                                }
                                let id = row.id.clone();
                                let name = row.name.clone();
                                resp.context_menu(|ui| {
                                    if ui.button("刷新此条目").clicked() {
                                        refresh = Some(id.clone());
                                        ui.close_menu();
                                    }
                                    if ui.button("打开来源页面").clicked() {
                                        if let Some(t) = self.track(&id) {
                                            let url = t
                                                .release
                                                .as_ref()
                                                .map(|r| r.page_url.clone())
                                                .unwrap_or_else(|| t.url.clone());
                                            let _ = open::that_detached(url);
                                        }
                                        ui.close_menu();
                                    }
                                    ui.separator();
                                    if ui
                                        .button(RichText::new(format!("移除「{}」", ellipsis(&name, 12))).color(RED))
                                        .clicked()
                                    {
                                        remove = Some(id.clone());
                                        ui.close_menu();
                                    }
                                });
                            }
                        }
                        ui.add_space(10.0);
                    });

                if let Some(id) = clicked {
                    self.selected = Some(id);
                    self.tab = Tab::Overview;
                }
                if let Some(id) = refresh {
                    self.resolve(&id);
                }
                if let Some(id) = remove {
                    self.remove_target = Some(id);
                }

                ui.add_space(4.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} 有更新 / {} 项",
                            self.update_badge_count(),
                            self.tracks.len()
                        ))
                        .size(10.5)
                        .color(FAINT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("数据目录").clicked() {
                            let dir = crate::store::Store {
                                config: self.cfg.clone(),
                                tracks: Vec::new(),
                                history: Vec::new(),
                                seeded: false,
                            }
                            .dir();
                            let _ = std::fs::create_dir_all(&dir);
                            let _ = open::that_detached(dir);
                        }
                    });
                });
            });
    }

    // -----------------------------------------------------------------------
    // Download queue (bottom panel)
    // -----------------------------------------------------------------------

    pub(crate) fn queue_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("queue")
            .frame(
                egui::Frame::none()
                    .fill(CARD)
                    .inner_margin(Margin::symmetric(18.0, 8.0)),
            )
            .show(ctx, |ui| {
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    ui.max_rect().top(),
                    Stroke::new(1.0_f32, BORDER),
                );
                let views: Vec<TaskView> = self
                    .tasks
                    .iter()
                    .map(|t| TaskView {
                        id: t.spec.id.clone(),
                        app: t.spec.app.clone(),
                        file: t.spec.file.clone(),
                        status: t.status,
                        frac: t.frac(),
                        downloaded: t.downloaded,
                        total: t.total,
                        speed: t.speed,
                        error: t.error.clone(),
                        sha_ok: t.sha_ok,
                        path: t.path.clone(),
                    })
                    .collect();
                let mut acts: Vec<Act> = Vec::new();

                ui.horizontal(|ui| {
                    ui.label(RichText::new("下载队列").size(13.0).strong().color(TEXT));
                    let active = self.active_count();
                    badge(
                        ui,
                        &if active > 0 {
                            format!("{active} 个进行中")
                        } else {
                            format!("{} 个任务", self.tasks.len())
                        },
                        if active > 0 { ACCENT } else { MUTED },
                        if active > 0 {
                            ACCENT_SOFT
                        } else {
                            Color32::from_rgb(0xF2, 0xF4, 0xF8)
                        },
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("打开下载目录").clicked() {
                            let dir = PathBuf::from(self.cfg.download_dir.trim());
                            let _ = std::fs::create_dir_all(&dir);
                            let _ = open::that_detached(dir);
                        }
                        if !self.tasks.is_empty() && ui.small_button("清空已结束").clicked() {
                            self.clear_finished();
                        }
                        ui.label(
                            RichText::new(ellipsis(self.cfg.download_dir.trim(), 46))
                                .size(10.5)
                                .color(FAINT),
                        );
                    });
                });

                if views.is_empty() {
                    ui.label(
                        RichText::new("暂无下载任务。在右侧详情中选择文件即可开始下载，支持断点续传与 SHA-256 校验。")
                            .size(11.0)
                            .color(FAINT),
                    );
                    return;
                }

                ui.add_space(2.0);
                ScrollArea::vertical()
                    .max_height(154.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for v in &views {
                            ui.horizontal(|ui| {
                                ui.allocate_ui(Vec2::new(160.0, 36.0), |ui| {
                                    ui.vertical(|ui| {
                                        ui.label(
                                            RichText::new(ellipsis(&v.file, 22))
                                                .size(12.0)
                                                .color(TEXT),
                                        );
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {}",
                                                ellipsis(&v.app, 12),
                                                v.status.label()
                                            ))
                                            .size(10.0)
                                            .color(v.status.color()),
                                        );
                                    });
                                });
                                let bar_w = (ui.available_width() - 290.0).max(160.0);
                                let text = match v.status {
                                    TaskStatus::Running => format!(
                                        "{:.1}%  {} / {}  ·  {}",
                                        v.frac * 100.0,
                                        util::fmt_bytes(v.downloaded),
                                        if v.total > 0 {
                                            util::fmt_bytes(v.total)
                                        } else {
                                            "?".into()
                                        },
                                        util::fmt_speed(v.speed)
                                    ),
                                    TaskStatus::Done => format!(
                                        "完成 · {} {}",
                                        util::fmt_bytes(v.downloaded),
                                        match v.sha_ok {
                                            Some(true) => "· SHA-256 ✓",
                                            Some(false) => "· 校验失败",
                                            None => "",
                                        }
                                    ),
                                    TaskStatus::Failed => format!(
                                        "失败 · {}",
                                        ellipsis(v.error.as_deref().unwrap_or("未知错误"), 40)
                                    ),
                                    TaskStatus::Paused => {
                                        format!("已暂停 · {}", util::fmt_bytes(v.downloaded))
                                    }
                                    _ => v.status.label().to_string(),
                                };
                                let fill = match v.status {
                                    TaskStatus::Done => GREEN,
                                    TaskStatus::Failed => RED,
                                    TaskStatus::Paused => ORANGE,
                                    _ => ACCENT,
                                };
                                ui.add(
                                    egui::ProgressBar::new(v.frac)
                                        .desired_width(bar_w)
                                        .fill(fill)
                                        .text(RichText::new(text).size(10.5).color(TEXT)),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    match v.status {
                                        TaskStatus::Running => {
                                            if ui.small_button("取消").clicked() {
                                                acts.push(Act::Cancel(v.id.clone()));
                                            }
                                            if ui.small_button("暂停").clicked() {
                                                acts.push(Act::Pause(v.id.clone()));
                                            }
                                        }
                                        TaskStatus::Paused
                                        | TaskStatus::Queued
                                        | TaskStatus::Canceled
                                        | TaskStatus::Failed => {
                                            if ui.small_button("重试").clicked() {
                                                acts.push(Act::Retry(v.id.clone()));
                                            }
                                            if v.status != TaskStatus::Failed
                                                && ui.small_button("继续").clicked()
                                            {
                                                acts.push(Act::Resume(v.id.clone()));
                                            }
                                        }
                                        TaskStatus::Done => {
                                            if ui.small_button("打开文件").clicked() {
                                                if let Some(p) = &v.path {
                                                    acts.push(Act::Open(p.clone()));
                                                }
                                            }
                                        }
                                    }
                                    if let Some(p) = &v.path {
                                        if ui.small_button("目录").clicked() {
                                            let dir = PathBuf::from(p)
                                                .parent()
                                                .map(|x| x.to_path_buf())
                                                .unwrap_or_default();
                                            acts.push(Act::OpenDir(dir));
                                        }
                                    }
                                });
                            });
                        }
                    });

                for act in acts {
                    match act {
                        Act::Pause(id) => self.pause_task(&id),
                        Act::Resume(id) => self.resume_task(&id),
                        Act::Cancel(id) => self.cancel_task(&id),
                        Act::Retry(id) => self.retry_task(&id),
                        Act::Open(p) => {
                            let _ = open::that_detached(p);
                        }
                        Act::OpenDir(p) => {
                            let _ = open::that_detached(p);
                        }
                    }
                }
            });
    }

    // -----------------------------------------------------------------------
    // Central detail page
    // -----------------------------------------------------------------------

    pub(crate) fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(18.0, 14.0)),
            )
            .show(ctx, |ui| {
                let track = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.track(id))
                    .cloned();
                match track {
                    None => self.empty_state(ui),
                    Some(t) => {
                        ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| self.detail(ui, &t));
                    }
                }
            });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.add_space(120.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("还没有选择追踪项")
                    .size(18.0)
                    .color(MUTED),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("在左侧选择一项，或粘贴 GitHub 仓库 / 软件站地址与关键词新建追踪")
                    .size(12.0)
                    .color(FAINT),
            );
            ui.add_space(14.0);
            if ui.add(primary_button("+ 添加追踪")).clicked() {
                self.add_text.clear();
                self.show_add = true;
            }
        });
    }

    fn detail(&mut self, ui: &mut egui::Ui, track: &Track) {
        // ---- header ----
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(&track.name)
                        .size(22.0)
                        .strong()
                        .color(TEXT),
                );
                ui.add_space(3.0);
                ui.horizontal(|ui| {
                    badge(ui, &track.group, ACCENT, ACCENT_SOFT);
                    badge(
                        ui,
                        track.kind.label(),
                        MUTED,
                        Color32::from_rgb(0xF2, 0xF4, 0xF8),
                    );
                    badge(
                        ui,
                        &track.host(),
                        MUTED,
                        Color32::from_rgb(0xF2, 0xF4, 0xF8),
                    );
                    if let Some(r) = &track.release {
                        ui.label(
                            RichText::new(format!("来源: {}", r.provider))
                                .size(10.5)
                                .color(FAINT),
                        );
                    }
                });
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let page = track
                    .release
                    .as_ref()
                    .map(|r| r.page_url.clone())
                    .unwrap_or_else(|| track.url.clone());
                if ui.add(subtle_button("打开来源页面")).clicked() {
                    let _ = open::that_detached(&page);
                }
                let busy = self.resolving.contains(&track.id);
                if ui
                    .add_enabled(!busy, subtle_button("刷新"))
                    .clicked()
                {
                    self.resolve(&track.id);
                }
                if busy {
                    ui.spinner();
                    ui.label(RichText::new("解析中…").size(11.0).color(ACCENT));
                }
            });
        });

        ui.add_space(10.0);

        // ---- tabs ----
        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Overview, "概览"),
                (Tab::Notes, "更新说明"),
                (Tab::History, "历史版本"),
            ] {
                let selected = self.tab == tab;
                let resp = ui.add(
                    egui::Button::new(
                        RichText::new(label)
                            .size(13.0)
                            .color(if selected { ACCENT } else { MUTED }),
                    )
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::NONE)
                    .rounding(Rounding::same(0.0)),
                );
                if selected {
                    ui.painter().hline(
                        resp.rect.x_range(),
                        resp.rect.bottom() + 3.0,
                        Stroke::new(2.0_f32, ACCENT),
                    );
                }
                if resp.clicked() {
                    self.tab = tab;
                }
                ui.add_space(6.0);
            }
        });
        ui.add_space(6.0);

        if let Some(err) = &track.last_error {
            let msg = if track.release.is_some() {
                format!("上次刷新失败：{err}（下方显示的是缓存数据）")
            } else {
                format!("解析失败：{err}")
            };
            card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(msg).size(12.0).color(RED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("重试").clicked() {
                            self.resolve(&track.id);
                        }
                    });
                });
            });
            ui.add_space(8.0);
        }

        let Some(base) = track.release.clone() else {
            if track.last_error.is_some() {
                return;
            }
            card(ui, |ui| {
                if self.resolving.contains(&track.id) {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("正在解析版本与下载链接…").size(13.0).color(MUTED));
                    });
                } else {
                    ui.label(RichText::new("尚未获取到版本信息").size(13.0).color(MUTED));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("点击右上角「刷新」开始解析，或检查输入的关键词是否准确。")
                            .size(11.5)
                            .color(FAINT),
                    );
                }
            });
            return;
        };

        // A repo can offer several release lines (stable 1.3 / preview 2.0 / older
        // 1.2). `sel` is the tab the user picked; everything below renders that view.
        let sel = self.candidate_index(&track.id, &base);
        let release = base.view_of(sel);

        self.version_card(ui, track, &release, sel);
        ui.add_space(8.0);

        match self.tab {
            Tab::Overview => self.overview(ui, track, &release),
            Tab::Notes => self.notes_view(ui, &release),
            Tab::History => self.history_view(ui, &release),
        }

        ui.add_space(14.0);
    }

    fn version_card(&mut self, ui: &mut egui::Ui, track: &Track, r: &Release, sel: usize) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    let caption = if sel == 0 { "最新版本" } else { "候选版本" };
                    ui.label(RichText::new(caption).size(11.0).color(FAINT));
                    ui.label(
                        RichText::new(if r.version.trim().is_empty() {
                            "未知"
                        } else {
                            r.version.as_str()
                        })
                        .size(28.0)
                        .strong()
                        .color(TEXT),
                    );
                    ui.horizontal(|ui| {
                        if let Some(d) = &r.published {
                            ui.label(
                                RichText::new(format!("发布于 {d}"))
                                    .size(11.0)
                                    .color(MUTED),
                            );
                        }
                        ui.label(
                            RichText::new(format!(
                                "· 上次检查 {}",
                                util::fmt_secs_ago(track.last_checked.unwrap_or(0))
                            ))
                            .size(11.0)
                            .color(FAINT),
                        );
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                    if sel > 0 {
                        // A candidate is never "the newest version" — just label it.
                        if r.candidates.get(sel).map(|c| c.prerelease).unwrap_or(false) {
                            badge(ui, "预览版", ORANGE, ORANGE_SOFT);
                        } else {
                            badge(ui, "备选版本", MUTED, Color32::from_rgb(0xF2, 0xF4, 0xF8));
                        }
                    } else if track.has_update() {
                        badge(ui, "发现新版本", ORANGE, ORANGE_SOFT);
                    } else if track.seen_version.is_some() {
                        badge(ui, "已是最新", GREEN, GREEN_SOFT);
                    } else {
                        badge(ui, "首次获取", ACCENT, ACCENT_SOFT);
                    }
                });
            });

            if sel == 0 && track.has_update() {
                ui.add_space(8.0);
                egui::Frame::none()
                    .fill(ORANGE_SOFT)
                    .rounding(Rounding::same(7.0))
                    .inner_margin(Margin::symmetric(10.0, 8.0))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(format!(
                                    "版本从 {} 变为 {}，建议先看更新说明再决定是否下载。",
                                    track.seen_version.clone().unwrap_or_default(),
                                    r.version
                                ))
                                .size(12.0)
                                .color(ORANGE),
                            );
                            if ui.small_button("标记已知").clicked() {
                                if let Some(t) = self.track_mut(&track.id) {
                                    t.seen_version = Some(r.version.clone());
                                }
                                self.save_tracks();
                            }
                        });
                    });
            }

            for w in &r.warnings {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("注意：{w}")).size(11.5).color(ORANGE));
            }

            if !r.summary.trim().is_empty() {
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new("更新摘要").size(11.0).strong().color(MUTED));
                ui.add_space(3.0);
                let preview: String = r.summary.lines().take(5).collect::<Vec<_>>().join("\n");
                ui.label(RichText::new(preview).size(12.5).color(TEXT));
                if r.notes.chars().count() > r.summary.chars().count() {
                    ui.add_space(4.0);
                    if ui.link(RichText::new("查看完整更新说明 →").size(11.5)).clicked() {
                        self.tab = Tab::Notes;
                    }
                }
            }

            // ---- version candidates (bottom-right tab strip) ----
            // Repos like nyaterm publish 2.0 previews on top of the stable 1.3; each
            // release line gets its own pill so both can be inspected and downloaded.
            if r.candidates.len() > 1 {
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(7.0);
                let mut pick: Option<usize> = None;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("版本候选").size(10.5).color(FAINT));
                    ui.label(
                        RichText::new("· 点标签切换该版本的文件与更新说明")
                            .size(10.0)
                            .color(FAINT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        for (i, c) in r.candidates.iter().enumerate().rev() {
                            let on = i == sel;
                            let tint = if c.prerelease { ORANGE } else { ACCENT };
                            let (fg, fill) = if on {
                                (tint, if c.prerelease { ORANGE_SOFT } else { ACCENT_SOFT })
                            } else if c.prerelease {
                                // Preview pills stay orange even when inactive — the
                                // whole point is that the user notices them.
                                (ORANGE, Color32::TRANSPARENT)
                            } else {
                                (MUTED, Color32::TRANSPARENT)
                            };
                            let resp = ui.add(
                                egui::Button::new(
                                    RichText::new(c.tab_label()).size(11.5).color(fg),
                                )
                                .fill(fill)
                                .stroke(Stroke::new(
                                    1.0_f32,
                                    if on { tint } else { BORDER },
                                ))
                                .rounding(Rounding::same(6.0))
                                .min_size(Vec2::new(0.0, 24.0)),
                            );
                            if resp.clicked() {
                                pick = Some(i);
                            }
                            ui.add_space(4.0);
                        }
                    });
                });
                if let Some(i) = pick {
                    self.candidate_sel.insert(track.id.clone(), i);
                }
            }
        });
    }

    fn overview(&mut self, ui: &mut egui::Ui, track: &Track, r: &Release) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("下载文件").size(13.0).strong().color(TEXT));
            badge(
                ui,
                &format!("{} 个", r.assets.len()),
                MUTED,
                Color32::from_rgb(0xF2, 0xF4, 0xF8),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!(
                        "保存到 {}/{}",
                        ellipsis(track.name.as_str(), 16),
                        ellipsis(&r.version, 14)
                    ))
                    .size(10.5)
                    .color(FAINT),
                );
            });
        });
        ui.add_space(6.0);

        if r.assets.is_empty() {
            card(ui, |ui| {
                ui.label(
                    RichText::new("没有解析到可下载的文件")
                        .size(12.5)
                        .color(MUTED),
                );
                ui.add_space(3.0);
                ui.label(
                    RichText::new("这类站点通常需要登录或在页面内跳转，请点击「打开来源页面」获取。")
                        .size(11.0)
                        .color(FAINT),
                );
            });
            return;
        }

        let mut download: Option<(String, String, Asset)> = None;
        let mut open_url: Option<String> = None;
        let mut copied: Option<String> = None;

        for (i, a) in r.assets.iter().enumerate() {
            let is_direct = a.kind == AssetKind::Direct;
            let (fg, bg) = match a.kind {
                AssetKind::Direct => (GREEN, GREEN_SOFT),
                AssetKind::Pan => (ACCENT, ACCENT_SOFT),
                AssetKind::Page => (MUTED, Color32::from_rgb(0xF2, 0xF4, 0xF8)),
            };
            egui::Frame::none()
                .fill(if i % 2 == 0 {
                    Color32::from_rgb(0xFB, 0xFC, 0xFE)
                } else {
                    CARD
                })
                .stroke(Stroke::new(1.0_f32, BORDER))
                .rounding(Rounding::same(8.0))
                .inner_margin(Margin::symmetric(12.0, 9.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        badge(ui, a.kind.label(), fg, bg);
                        ui.add_space(2.0);
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(ellipsis(&a.name, 64))
                                    .size(12.5)
                                    .monospace()
                                    .color(TEXT),
                            );
                            ui.horizontal(|ui| {
                                if !a.note.is_empty() {
                                    ui.label(RichText::new(&a.note).size(10.5).color(MUTED));
                                }
                                if let Some(s) = a.size {
                                    ui.label(
                                        RichText::new(format!("· {}", util::fmt_bytes(s)))
                                            .size(10.5)
                                            .color(FAINT),
                                    );
                                }
                                if let Some(d) = &a.date {
                                    ui.label(
                                        RichText::new(format!("· {d}"))
                                            .size(10.5)
                                            .color(FAINT),
                                    );
                                }
                                if let Some(sha) = &a.sha256 {
                                    ui.label(
                                        RichText::new(format!("· sha256:{}…", &sha[..12.min(sha.len())]))
                                            .size(10.0)
                                            .color(FAINT),
                                    );
                                }
                                if is_direct && a.size.is_none() {
                                    ui.label(
                                        RichText::new("· 大小由服务器返回")
                                            .size(10.0)
                                            .color(FAINT),
                                    );
                                }
                            });
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if is_direct {
                                if ui.add(primary_button("下载")).clicked() {
                                    download =
                                        Some((track.id.clone(), r.version.clone(), a.clone()));
                                }
                            } else if ui.add(primary_button("打开链接")).clicked() {
                                open_url = Some(a.url.clone());
                            }
                            if ui.add(subtle_button("复制")).clicked() {
                                copied = Some(a.url.clone());
                            }
                        });
                    });
                });
            ui.add_space(5.0);
        }

        if let Some((tid, ver, asset)) = download {
            self.start_asset(&tid, &ver, &asset);
        }
        if let Some(url) = open_url {
            let _ = open::that_detached(url);
        }
        if let Some(url) = copied {
            ui.output_mut(|o| o.copied_text = url);
            self.toast("链接已复制到剪贴板");
        }

        ui.add_space(4.0);
        ui.label(
            RichText::new("直链支持断点续传；GitHub 附件会按公布值校验 SHA-256。网盘/页面会在浏览器中打开。")
                .size(10.5)
                .color(FAINT),
        );
    }

    fn notes_view(&mut self, ui: &mut egui::Ui, r: &Release) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("更新说明").size(13.0).strong().color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("打开原文").clicked() {
                        let _ = open::that_detached(&r.page_url);
                    }
                });
            });
            ui.add_space(6.0);
            if r.notes.trim().is_empty() {
                ui.label(
                    RichText::new("该来源没有提供文字更新说明。")
                        .size(12.0)
                        .color(FAINT),
                );
            } else {
                ScrollArea::vertical()
                    .id_salt("notes_scroll")
                    .max_height(520.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(RichText::new(&r.notes).size(12.5).color(TEXT));
                    });
            }
        });
    }

    fn history_view(&mut self, ui: &mut egui::Ui, r: &Release) {
        card(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("历史版本").size(13.0).strong().color(TEXT));
                badge(
                    ui,
                    &format!("{} 条", r.history.len()),
                    MUTED,
                    Color32::from_rgb(0xF2, 0xF4, 0xF8),
                );
            });
            ui.add_space(4.0);
            if r.history.is_empty() {
                ui.label(
                    RichText::new("该来源未提供历史版本列表（软件站通常只维护最新一版）。")
                        .size(11.5)
                        .color(FAINT),
                );
            }
            for v in &r.history {
                let head = format!(
                    "{}   {}",
                    if v.version.is_empty() { &v.title } else { &v.version },
                    v.date.clone().unwrap_or_default()
                );
                egui::CollapsingHeader::new(RichText::new(head).size(12.5))
                    .id_salt(format!("hist-{}", v.version))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(if v.notes.trim().is_empty() {
                                "（无说明）"
                            } else {
                                v.notes.as_str()
                            })
                            .size(11.5)
                            .color(MUTED),
                        );
                        if !v.url.is_empty() && ui.small_button("打开该版本页面").clicked() {
                            let _ = open::that_detached(&v.url);
                        }
                    });
            }
        });
    }

    // -----------------------------------------------------------------------
    // Windows
    // -----------------------------------------------------------------------

    pub(crate) fn windows(&mut self, ctx: &egui::Context) {
        self.add_window(ctx);
        self.settings_window(ctx);
        self.history_window(ctx);
        self.about_window(ctx);
        self.remove_window(ctx);
    }

    fn add_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_add;
        egui::Window::new("添加追踪")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(660.0)
            .max_height(640.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "粘贴网址与软件名称。支持一次粘贴多行：站点根地址后的各行名称会自动归属该站点；@scene#N:\"分组\" 会作为分组名。",
                    )
                    .size(11.5)
                    .color(MUTED),
                );
                ui.add_space(6.0);
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut self.add_text)
                        .desired_rows(8)
                        .desired_width(f32::INFINITY)
                        .hint_text(parse::DEFAULT_INPUT),
                );
                if resp.changed() {
                    let parsed = parse::parse_input(&self.add_text);
                    self.add_preview = parsed.tracks;
                    self.add_skipped = parsed.skipped;
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "解析出 {} 项{}",
                            self.add_preview.len(),
                            if self.add_skipped.is_empty() {
                                String::new()
                            } else {
                                format!("，{} 行无法识别（需先给出站点地址）", self.add_skipped.len())
                            }
                        ))
                        .size(11.0)
                        .color(if self.add_preview.is_empty() { FAINT } else { GREEN }),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let enabled = !self.add_preview.is_empty();
                        if ui
                            .add_enabled(enabled, primary_button("加入追踪列表"))
                            .clicked()
                        {
                            let tracks = self.add_preview.clone();
                            let n = self.merge_tracks(tracks);
                            self.show_add = false;
                            self.toast(format!("已添加 {n} 个追踪项，开始解析…"));
                        }
                    });
                });
                if !self.add_preview.is_empty() {
                    ui.add_space(6.0);
                    ui.separator();
                    ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for t in &self.add_preview {
                            ui.horizontal(|ui| {
                                badge(
                                    ui,
                                    match t.kind {
                                        SourceKind::GitHub => "GitHub",
                                        SourceKind::Site => "站点搜索",
                                        SourceKind::Article => "页面",
                                    },
                                    ACCENT,
                                    ACCENT_SOFT,
                                );
                                ui.label(RichText::new(&t.name).size(12.0).strong().color(TEXT));
                                ui.label(
                                    RichText::new(ellipsis(&t.url, 52))
                                        .size(10.5)
                                        .monospace()
                                        .color(FAINT),
                                );
                            });
                        }
                    });
                }
                if !self.add_skipped.is_empty() {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("未识别：".to_string() + &ellipsis(&self.add_skipped.join(" / "), 70))
                            .size(10.5)
                            .color(ORANGE),
                    );
                }
            });
        self.show_add = open;
    }

    fn merge_tracks(&mut self, tracks: Vec<Track>) -> usize {
        let mut added = 0;
        let mut new_ids = Vec::new();
        for t in tracks {
            if self
                .tracks
                .iter()
                .any(|x| x.url.eq_ignore_ascii_case(&t.url) && x.query.eq_ignore_ascii_case(&t.query))
            {
                continue;
            }
            new_ids.push(t.id.clone());
            self.tracks.push(t);
            added += 1;
        }
        if added > 0 {
            self.save_tracks();
            for id in new_ids {
                self.resolve(&id);
            }
            if self.selected.is_none() {
                self.selected = self.tracks.first().map(|t| t.id.clone());
            }
        }
        added
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("下载目录").size(12.0).strong().color(TEXT));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.dir)
                            .desired_width(ui.available_width() - 86.0),
                    );
                    if ui.button("选择…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.settings.dir = dir.to_string_lossy().to_string();
                        }
                    }
                });
                ui.label(
                    RichText::new("文件按「软件名/版本/文件名」存放，可随时在底部状态栏打开。")
                        .size(10.5)
                        .color(FAINT),
                );
                ui.add_space(10.0);

                ui.label(RichText::new("GitHub Token（可选）").size(12.0).strong().color(TEXT));
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.token)
                        .hint_text("ghp_… 仅用于提高 API 额度，可留空")
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                ui.label(
                    RichText::new("默认走 releases 页面与 atom 订阅，不消耗 API 额度；Token 只在回退到 API 时使用。")
                        .size(10.5)
                        .color(FAINT),
                );
                ui.add_space(10.0);

                ui.label(RichText::new("代理").size(12.0).strong().color(TEXT));
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.proxy)
                        .hint_text("http://127.0.0.1:7890（留空为直连）")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.label(RichText::new("同时下载数").size(12.0).color(TEXT));
                    ui.add(egui::Slider::new(&mut self.settings.concurrent, 1..=6));
                    ui.add_space(16.0);
                    ui.label(RichText::new("自动检查间隔").size(12.0).color(TEXT));
                    let mut v = self.settings.interval as i64;
                    if ui
                        .add(egui::DragValue::new(&mut v).speed(5.0).suffix(" 分钟"))
                        .changed()
                    {
                        self.settings.interval = v.clamp(0, 10080) as u64;
                    }
                });
                ui.checkbox(&mut self.settings.auto_start, "启动时自动检查全部追踪项");
                ui.checkbox(&mut self.settings.prefer_os, "下载时优先展示当前系统对应的文件");
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("数据目录：{}", data_dir_hint()))
                        .size(10.5)
                        .color(FAINT),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add(primary_button("保存设置")).clicked() {
                        self.cfg.download_dir = self.settings.dir.trim().to_string();
                        self.cfg.github_token = self.settings.token.trim().to_string();
                        self.cfg.proxy = self.settings.proxy.trim().to_string();
                        self.cfg.max_concurrent = self.settings.concurrent.max(1);
                        self.cfg.check_interval_min = self.settings.interval;
                        self.cfg.auto_check_on_start = self.settings.auto_start;
                        self.cfg.prefer_current_os = self.settings.prefer_os;
                        self.http = crate::app::make_http(&self.cfg);
                        self.save_config();
                        self.toast("设置已保存");
                        self.show_settings = false;
                    }
                    if ui.button("取消").clicked() {
                        self.settings = SettingsDraft::from(&self.cfg);
                        self.show_settings = false;
                    }
                });
            });
        self.show_settings = open;
    }

    fn history_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_history;
        egui::Window::new("下载历史")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(760.0)
            .max_height(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.history_filter)
                            .hint_text("筛选软件名或文件名…")
                            .desired_width(260.0),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("清空历史").clicked() {
                            self.history.clear();
                            self.save_history();
                            self.toast("下载历史已清空");
                        }
                        ui.label(
                            RichText::new(format!("{} 条记录", self.history.len()))
                                .size(10.5)
                                .color(FAINT),
                        );
                    });
                });
                ui.add_space(6.0);
                let filter = self.history_filter.trim().to_lowercase();
                let rows: Vec<_> = self
                    .history
                    .iter()
                    .filter(|r| {
                        filter.is_empty()
                            || r.app.to_lowercase().contains(&filter)
                            || r.file.to_lowercase().contains(&filter)
                    })
                    .cloned()
                    .collect();
                if rows.is_empty() {
                    ui.label(
                        RichText::new("暂无下载记录。")
                            .size(12.0)
                            .color(FAINT),
                    );
                }
                ScrollArea::vertical().max_height(460.0).show(ui, |ui| {
                    for r in rows {
                        egui::Frame::none()
                            .fill(Color32::from_rgb(0xFB, 0xFC, 0xFE))
                            .stroke(Stroke::new(1.0_f32, BORDER))
                            .rounding(Rounding::same(7.0))
                            .inner_margin(Margin::symmetric(10.0, 8.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (fg, bg) = match r.status.as_str() {
                                        "完成" => (GREEN, GREEN_SOFT),
                                        "失败" => (RED, RED_SOFT),
                                        _ => (MUTED, Color32::from_rgb(0xF2, 0xF4, 0xF8)),
                                    };
                                    badge(ui, &r.status, fg, bg);
                                    ui.vertical(|ui| {
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {}",
                                                r.app,
                                                ellipsis(&r.file, 42)
                                            ))
                                            .size(12.0)
                                            .color(TEXT),
                                        );
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {} · {}",
                                                if r.version.is_empty() { "—" } else { &r.version },
                                                util::fmt_bytes(r.size),
                                                util::fmt_date(r.finished_at)
                                            ))
                                            .size(10.5)
                                            .color(FAINT),
                                        );
                                        if let Some(e) = &r.error {
                                            ui.label(
                                                RichText::new(ellipsis(e, 70))
                                                    .size(10.0)
                                                    .color(RED),
                                            );
                                        }
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if !r.path.is_empty() {
                                            if ui.small_button("打开文件").clicked() {
                                                let _ = open::that_detached(&r.path);
                                            }
                                            if ui.small_button("目录").clicked() {
                                                if let Some(p) = PathBuf::from(&r.path).parent() {
                                                    let _ = open::that_detached(p);
                                                }
                                            }
                                        }
                                        if r.sha_ok == Some(true) {
                                            badge(ui, "SHA-256 ✓", GREEN, GREEN_SOFT);
                                        }
                                    });
                                });
                            });
                        ui.add_space(4.0);
                    }
                });
            });
        self.show_history = open;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_about;
        egui::Window::new("关于")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("App Monitor · 跨平台软件追踪下载器").size(16.0).strong());
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "输入 GitHub 仓库或软件站地址 + 关键词，自动解析最新版本、下载链接与更新说明；\n\
                         直链支持断点续传、SHA-256 校验，并记录完整下载历史。",
                    )
                    .size(12.0)
                    .color(MUTED),
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                for line in [
                    "解析来源：GitHub Releases（atom + 资源列表）、软件站站内搜索与文章抽取",
                    "输入语法：URL + 名称；站点地址后续行自动归属该站点；@scene#N:\"分组\" 指定分组",
                    "下载：Range 续传 / 暂停继续 / 速度与剩余时间 / SHA-256 校验",
                    "快捷键：Ctrl+1/2/3 切换页签，Ctrl+D 下载首选文件，Ctrl+H 历史，Ctrl+N 添加，Ctrl+, 设置",
                    "配置、追踪列表与历史都保存在数据目录，可整目录携带",
                ] {
                    ui.label(RichText::new(format!("· {line}")).size(11.5).color(TEXT));
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("数据目录：{}", data_dir_hint()))
                        .size(10.5)
                        .color(FAINT),
                );
                ui.add_space(6.0);
                if ui.small_button("打开数据目录").clicked() {
                    let _ = open::that_detached(data_dir_hint());
                }
            });
        self.show_about = open;
    }

    fn remove_window(&mut self, ctx: &egui::Context) {
        let Some(id) = self.remove_target.clone() else {
            return;
        };
        let name = self
            .track(&id)
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let mut open = true;
        egui::Window::new("移除追踪项")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!("确定移除「{name}」？"))
                        .size(12.5)
                        .color(TEXT),
                );
                ui.label(
                    RichText::new("仅从追踪列表移除，已下载的文件与历史记录不会删除。")
                        .size(10.5)
                        .color(FAINT),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("移除").color(RED)).clicked() {
                        self.tracks.retain(|t| t.id != id);
                        if self.selected.as_deref() == Some(id.as_str()) {
                            self.selected = self.tracks.first().map(|t| t.id.clone());
                        }
                        self.save_tracks();
                        self.remove_target = None;
                        self.toast("已移除");
                    }
                    if ui.button("取消").clicked() {
                        self.remove_target = None;
                    }
                });
            });
        if !open {
            self.remove_target = None;
        }
    }

    pub(crate) fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some((msg, at)) = self.toast.clone() else {
            return;
        };
        if at.elapsed() > Duration::from_secs(4) {
            self.toast = None;
            return;
        }
        egui::Area::new(egui::Id::new("toast"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-26.0, -84.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(Color32::from_rgb(0x1F, 0x29, 0x37))
                    .rounding(Rounding::same(8.0))
                    .inner_margin(Margin::symmetric(14.0, 9.0))
                    .show(ui, |ui| {
                        ui.label(RichText::new(msg).size(12.0).color(Color32::WHITE));
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(300));
    }
}

fn data_dir_hint() -> String {
    crate::store::Store {
        config: crate::model::Config::default(),
        tracks: Vec::new(),
        history: Vec::new(),
        seeded: false,
    }
    .dir()
    .to_string_lossy()
    .to_string()
}
