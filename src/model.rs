//! Core data model: tracked items, resolved releases, assets and download history.
//!
//! Everything here is `serde`-serialisable because the whole state is persisted to
//! JSON next to the executable, which makes the app portable: copy the folder and
//! your watch list travels with it.

use serde::{Deserialize, Serialize};

use crate::util;

/// Where a tracked item comes from. Determines which provider resolves it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// `github.com/owner/repo` — releases atom feed + expanded asset list.
    GitHub,
    /// A download site root (`https://www.423down.com/`) — searched by keyword.
    Site,
    /// A concrete article / download page.
    Article,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::GitHub => "GitHub",
            SourceKind::Site => crate::i18n::t("站点搜索", "site search"),
            SourceKind::Article => crate::i18n::t("页面", "page"),
        }
    }
}

/// How the user can actually get the bits.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    /// Plain HTTP(S) file — the app can download it with resume support.
    Direct,
    /// A landing page (article, mirror chooser) — opened in the browser.
    Page,
    /// A net-disk share (Quark / 123pan / Lanzou / Baidu …) — opened in the browser.
    Pan,
}

impl AssetKind {
    pub fn label(self) -> &'static str {
        match self {
            AssetKind::Direct => crate::i18n::t("直链", "direct"),
            AssetKind::Page => crate::i18n::t("页面", "page"),
            AssetKind::Pan => crate::i18n::t("网盘", "net disk"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub kind: AssetKind,
    /// Bytes, when the source publishes it.
    pub size: Option<u64>,
    /// Free-form tag: "Windows · x64", "夸克网盘", "磁力" …
    pub note: String,
    pub sha256: Option<String>,
    /// ISO timestamp published by GitHub; free-form date elsewhere.
    pub date: Option<String>,
}

impl Default for AssetKind {
    fn default() -> Self {
        AssetKind::Page
    }
}

/// One historical release (used for the "版本历史" tab).
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct VersionEntry {
    pub version: String,
    pub title: String,
    pub date: Option<String>,
    pub notes: String,
    pub url: String,
}

/// One alternative release line offered next to the primary pick.
///
/// A repo that ships `2.0.0-preview.7` above its stable `1.3.0` gives the user two
/// very different things to download; instead of hiding the preview inside a
/// warning string we carry it (plus any other release line) as a full candidate so
/// the detail page can switch between them with a tab.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Candidate {
    /// Tag as published (`v2.0.0-preview.7`) — also used to build the notes link.
    pub tag: String,
    pub version: String,
    pub title: String,
    pub date: Option<String>,
    /// Release page for *this* tag (not the repo's `/releases` index).
    pub page_url: String,
    pub summary: String,
    pub notes: String,
    pub assets: Vec<Asset>,
    /// Preview / beta / rc / nightly — drawn with an orange badge.
    pub prerelease: bool,
}

impl Candidate {
    /// Short tab caption: `1.3.0 稳定` / `2.0.0-preview.7 预览`.
    pub fn tab_label(&self) -> String {
        let channel = if self.prerelease {
            crate::i18n::t("预览", "preview")
        } else {
            crate::i18n::t("稳定", "stable")
        };
        if self.version.trim().is_empty() {
            channel.to_string()
        } else if crate::i18n::is_en() {
            // 英文里版本号与频道之间习惯用空格而不是直接连着写。
            format!("{} · {channel}", self.version.trim())
        } else {
            format!("{} {channel}", self.version.trim())
        }
    }
}

/// Everything we know about the newest release of one tracked item.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Release {
    /// Release title as published (may differ from the version string).
    pub title: String,
    pub version: String,
    pub published: Option<String>,
    pub page_url: String,
    pub provider: String,
    /// Short update blurb shown on the overview card.
    pub summary: String,
    /// Full changelog / article body (already converted to readable text).
    pub notes: String,
    pub assets: Vec<Asset>,
    pub history: Vec<VersionEntry>,
    pub fetched_at: i64,
    /// Soft problems worth surfacing ("下载区需登录后可见" …).
    pub warnings: Vec<String>,
    /// Up to three release lines to offer side by side, primary pick at index 0.
    /// Empty for providers that only ever expose one version.
    #[serde(default)]
    pub candidates: Vec<Candidate>,
}

impl Release {
    #[allow(dead_code)]
    pub fn best_asset(&self) -> Option<&Asset> {
        self.assets.iter().find(|a| a.kind == AssetKind::Direct)
    }

    /// A copy of this release whose visible fields come from `candidates[idx]`.
    ///
    /// This lets the detail page keep one code path: pick a candidate, then render
    /// version card / assets / notes exactly as for the primary release.
    pub fn view_of(&self, idx: usize) -> Release {
        let Some(c) = self.candidates.get(idx) else {
            return self.clone();
        };
        Release {
            title: c.title.clone(),
            version: c.version.clone(),
            published: c.date.clone(),
            page_url: c.page_url.clone(),
            provider: self.provider.clone(),
            summary: c.summary.clone(),
            notes: c.notes.clone(),
            assets: c.assets.clone(),
            history: self.history.clone(),
            fetched_at: self.fetched_at,
            warnings: Vec::new(),
            candidates: self.candidates.clone(),
        }
    }
}

/// One row in the watch list.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Track {
    pub id: String,
    /// What the user calls it — used as the search keyword too.
    pub name: String,
    /// Keyword actually used for site search (usually == name).
    pub query: String,
    /// Repo URL, site root, or article URL.
    pub url: String,
    pub kind: SourceKind,
    pub group: String,
    pub enabled: bool,
    /// Original user input line, kept for transparency / re-parsing.
    pub input: String,
    /// Version the user has already seen — a newer one raises the update badge.
    pub seen_version: Option<String>,
    pub last_checked: Option<i64>,
    pub last_error: Option<String>,
    pub release: Option<Release>,
}

impl Track {
    pub fn new(
        name: impl Into<String>,
        query: impl Into<String>,
        url: impl Into<String>,
        kind: SourceKind,
        group: impl Into<String>,
        input: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let url = url.into();
        let id = format!("{}-{}", util::slug(&name), util::short_hash(&url));
        Self {
            id,
            name,
            query: query.into(),
            url,
            kind,
            group: group.into(),
            enabled: true,
            input: input.into(),
            seen_version: None,
            last_checked: None,
            last_error: None,
            release: None,
        }
    }

    /// Newest release is different from the one the user acknowledged.
    pub fn has_update(&self) -> bool {
        match (&self.release, &self.seen_version) {
            (Some(r), Some(seen)) => !r.version.is_empty() && r.version != *seen,
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub download_dir: String,
    /// Optional GitHub token — raises the API limit and enables private repos.
    pub github_token: String,
    /// e.g. `http://127.0.0.1:7890`; empty = direct.
    pub proxy: String,
    pub max_concurrent: usize,
    pub auto_check_on_start: bool,
    pub check_interval_min: u64,
    /// Prefer assets matching the current OS when auto-selecting a file.
    pub prefer_current_os: bool,
    /// 界面缩放系数：字号与几处固定尺寸在「设计值」上乘它。1.0 = 默认。
    ///
    /// 高 DPI 屏上 GPUI 已经把逻辑像素换成物理像素了（150% 时 1 逻辑像素 =
    /// 1.5 物理像素），所以这里**不是** DPI 系数 —— 它是「字号再大一点」的那个
    /// 一点：11 逻辑像素在 4K 屏上照样小，用户得能自己调。
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
    /// 界面语言，`zh` / `en`。空串与未知值都按中文处理。
    #[serde(default = "default_lang")]
    pub lang: String,
}

fn default_ui_scale() -> f32 {
    1.0
}

fn default_lang() -> String {
    "zh".to_string()
}

impl Default for Config {
    fn default() -> Self {
        let download_dir = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("AppMonitor")
            .to_string_lossy()
            .to_string();
        Self {
            download_dir,
            github_token: String::new(),
            proxy: String::new(),
            max_concurrent: 3,
            auto_check_on_start: true,
            check_interval_min: 60,
            prefer_current_os: true,
            ui_scale: default_ui_scale(),
            lang: default_lang(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct DownloadRecord {
    pub id: String,
    pub app: String,
    pub file: String,
    pub version: String,
    pub url: String,
    pub path: String,
    pub size: u64,
    pub started_at: i64,
    pub finished_at: i64,
    /// "完成" / "失败" / "已取消"
    pub status: String,
    pub sha_ok: Option<bool>,
    pub error: Option<String>,
}

/// OS tag used to rank release assets.
pub fn current_os() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Rough platform guess for a release asset filename.
pub fn platform_of(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.contains("windows") || n.contains("win32") || n.contains("win64") || n.ends_with(".exe")
        || n.ends_with(".msi") || n.contains("setup")
    {
        Some("windows")
    } else if n.contains("darwin") || n.contains("macos") || n.contains("osx") || n.ends_with(".dmg")
    {
        Some("macos")
    } else if n.contains("linux") || n.contains("appimage") || n.contains(".deb") || n.ends_with(".rpm")
    {
        Some("linux")
    } else if n.contains("android") || n.ends_with(".apk") {
        Some("android")
    } else {
        None
    }
}

pub fn arch_of(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.contains("arm64") || n.contains("aarch64") {
        Some("arm64")
    } else if n.contains("x86_64")
        || n.contains("amd64")
        || n.contains("x64")
        || n.contains("win64")
    {
        Some("x64")
    } else if n.contains("386") || n.contains("i686") || n.contains("x86") || n.contains("win32") {
        Some("x86")
    } else {
        None
    }
}
