//! Source providers: turn a `Track` into a `Release`.

pub mod github;
pub mod site;

use crate::http::Http;
use crate::model::{AssetKind, Release, SourceKind, Track};
use crate::util;

/// Resolve the newest release for one tracked item.
pub async fn resolve(http: &Http, track: &Track) -> Result<Release, String> {
    let mut r = match track.kind {
        SourceKind::GitHub => github::resolve(http, track).await?,
        SourceKind::Site => site::resolve_site(http, track).await?,
        SourceKind::Article => site::resolve_article(http, track).await?,
    };
    r.fetched_at = util::now_secs();
    // Direct downloads first, current-OS builds before other platforms, bigger
    // files before auxiliary ones (checksums, signatures).
    r.assets.sort_by(|a, b| asset_rank(a).cmp(&asset_rank(b)));
    if r.version.trim().is_empty() {
        r.version = "未知".into();
    }
    if r.summary.chars().count() > 600 {
        r.summary = r.summary.chars().take(600).collect::<String>() + "…";
    }
    Ok(r)
}

fn asset_rank(a: &crate::model::Asset) -> (u8, u8, u8, u64) {
    let kind = match a.kind {
        AssetKind::Direct => 0,
        AssetKind::Pan => 1,
        AssetKind::Page => 2,
    };
    let os = if crate::model::platform_of(&a.name) == Some(crate::model::current_os()) {
        0
    } else if crate::model::platform_of(&a.name).is_none() {
        1
    } else {
        2
    };
    let lower = a.name.to_ascii_lowercase();
    let aux = if lower.contains("checksum")
        || lower.contains("sha256")
        || lower.ends_with(".sha256")
        || lower.ends_with(".sig")
    {
        1
    } else {
        0
    };
    // Larger files first inside a group (the installer beats the checksum file).
    (kind, os, aux, u64::MAX - a.size.unwrap_or(0))
}
