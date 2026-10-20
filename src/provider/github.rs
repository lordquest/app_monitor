//! GitHub provider.
//!
//! Uses the public **atom feed** (`/releases.atom`) plus the **expanded asset
//! fragment** (`/releases/expanded_assets/<tag>`) instead of the REST API:
//!
//! * no 60 req/h unauthenticated rate limit (the API gets exhausted quickly on
//!   shared IPs, which is exactly what happened while building this);
//! * the fragment contains size, upload date and the `sha256:` digest GitHub
//!   computes for every asset, which the API only exposes as `digest` on newer
//!   responses.
//!
//! When the HTML endpoints are unavailable (private repo with token), we fall back
//! to the REST API.

use std::sync::OnceLock;

use regex::Regex;

use crate::html;
use crate::http::Http;
use crate::model::{
    arch_of, current_os, platform_of, Asset, AssetKind, Candidate, Release, Track, VersionEntry,
};
use crate::util;

fn entry_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?is)<entry>(.*?)</entry>").unwrap())
}

fn title_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap())
}

fn updated_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?is)<updated>(.*?)</updated>").unwrap())
}

fn link_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?is)<link[^>]+rel="alternate"[^>]+href="([^"]+)""#).unwrap())
}

fn content_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?is)<content[^>]*>(.*?)</content>").unwrap())
}

fn href_dl_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?is)href="([^"]*/releases/download/[^"]+)""#).unwrap())
}

fn size_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(Bytes|KB|MB|GB|TB)").unwrap())
}

fn sha_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)sha256:([0-9a-f]{64})").unwrap())
}

fn iso_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(\d{4}-\d{2}-\d{2})T\d{2}:\d{2}:\d{2}Z").unwrap())
}

pub fn parse_repo(url: &str) -> Option<(String, String)> {
    let host = util::host_of(url);
    if !util::is_github_host(&host) {
        return None;
    }
    let path = url.split("://").nth(1)?.splitn(2, '/').nth(1)?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    Some((
        segs[0].trim_end_matches(".git").to_string(),
        segs[1].trim_end_matches(".git").to_string(),
    ))
}

pub async fn resolve(http: &Http, track: &Track) -> Result<Release, String> {
    let (owner, repo) = parse_repo(&track.url)
        .ok_or_else(|| format!("不是有效的 GitHub 仓库地址: {}", track.url))?;
    match resolve_atom(http, &owner, &repo).await {
        Ok(r) => Ok(r),
        Err(atom_err) => resolve_api(http, &owner, &repo)
            .await
            .map_err(|api_err| format!("{atom_err}；API 回退也失败：{api_err}")),
    }
}

#[derive(Clone)]
struct AtomEntry {
    title: String,
    tag: String,
    url: String,
    date: Option<String>,
    notes: String,
}

fn parse_atom(body: &str) -> Vec<AtomEntry> {
    let mut out = Vec::new();
    for c in entry_re().captures_iter(body) {
        let block = &c[1];
        let title = title_re()
            .captures(block)
            .map(|c| html::to_text(&html::decode_entities(&c[1])))
            .unwrap_or_default();
        let mut url = link_re()
            .captures(block)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let date = updated_re()
            .captures(block)
            .map(|c| c[1].chars().take(10).collect::<String>());
        if url.is_empty() {
            // Very old feeds only carry the id: `tag:github.com,2008:Repository/1/<tag>`.
            if let Some(id) = block
                .split("Repository/")
                .nth(1)
                .and_then(|s| s.split('<').next())
            {
                let tag = id.splitn(2, '/').nth(1).unwrap_or("").trim();
                if !tag.is_empty() {
                    url = format!("https://github.com/x/y/releases/tag/{tag}");
                }
            }
        }
        let tag = url
            .split("/tag/")
            .nth(1)
            .map(|s| s.trim_matches('/').to_string())
            .unwrap_or_default();
        let notes_html = content_re()
            .captures(block)
            .map(|c| html::decode_entities(&c[1]))
            .unwrap_or_default();
        let notes = html::to_text(&notes_html);
        if tag.is_empty() && title.is_empty() {
            continue;
        }
        out.push(AtomEntry {
            title: if title.is_empty() { tag.clone() } else { title },
            tag,
            url,
            date,
            notes,
        });
    }
    out
}

async fn resolve_atom(http: &Http, owner: &str, repo: &str) -> Result<Release, String> {
    let feed_url = format!("https://github.com/{owner}/{repo}/releases.atom");
    let feed = http.get_text(&feed_url).await?;
    let mut entries = parse_atom(&feed.body);
    if entries.is_empty() {
        return Err("该仓库没有可用的 Release".into());
    }
    let pick = best_entry(&entries, repo);
    let primary = entries.remove(pick);
    if primary.tag.is_empty() {
        return Err("无法从 feed 中解析版本标签".into());
    }

    // Up to three release lines, primary first; their asset lists are fetched in
    // parallel so the extra candidates cost roughly one round-trip, not three.
    let chosen = candidate_entries(&primary, &entries, repo);
    let fetched = futures_util::future::join_all(
        chosen
            .iter()
            .map(|e| fetch_assets(http, owner, repo, &e.tag)),
    )
    .await;
    let candidates: Vec<Candidate> = chosen
        .iter()
        .zip(fetched)
        .map(|(e, res)| {
            let assets = res.unwrap_or_default();
            Candidate {
                tag: e.tag.clone(),
                version: version_from_tag(&e.tag, &e.title),
                title: e.title.clone(),
                date: e.date.clone(),
                page_url: if e.tag.is_empty() {
                    format!("https://github.com/{owner}/{repo}/releases")
                } else {
                    format!(
                        "https://github.com/{owner}/{repo}/releases/tag/{}",
                        e.tag
                    )
                },
                summary: first_lines(&e.notes, 420),
                notes: truncate(&e.notes, 12000),
                prerelease: is_prerelease(&e.tag),
                assets,
            }
        })
        .collect();

    let head = candidates.first().cloned().unwrap_or_default();
    let mut warnings = asset_warnings(&head.assets);
    // A preview that did not make the tab strip would otherwise vanish silently.
    if let Some(pre) = entries.iter().find(|e| is_prerelease(&e.tag)) {
        if !candidates.iter().any(|c| c.tag == pre.tag) {
            warnings.push(format!(
                "另有更新的预发布版本 {}（{}），未作为候选版本列出",
                version_from_tag(&pre.tag, &pre.title),
                pre.date.clone().unwrap_or_else(|| "日期未知".into())
            ));
        }
    }

    let history: Vec<VersionEntry> = entries
        .into_iter()
        .filter(|e| entry_score(&e.tag, &e.title, repo) >= 0)
        .take(8)
        .map(|e| VersionEntry {
            version: version_from_tag(&e.tag, &e.title),
            title: e.title,
            date: e.date,
            notes: truncate(&e.notes, 2500),
            url: e.url,
        })
        .collect();

    Ok(Release {
        title: head.title.clone(),
        version: head.version.clone(),
        published: head.date.clone(),
        page_url: format!("https://github.com/{owner}/{repo}/releases"),
        provider: "GitHub Releases".into(),
        summary: head.summary.clone(),
        notes: head.notes.clone(),
        assets: head.assets.clone(),
        history,
        fetched_at: util::now_secs(),
        warnings,
        candidates,
    })
}

/// The release lines worth offering as tabs: the stable pick, then the newest
/// preview, then any other line (e.g. `1.2.x` while `1.3.x` is stable). Capped at
/// three so the strip stays a strip.
fn candidate_entries(primary: &AtomEntry, others: &[AtomEntry], repo: &str) -> Vec<AtomEntry> {
    let mut out = vec![primary.clone()];
    let mut lines: Vec<String> = vec![release_line(&primary.tag)];
    // Previews jump ahead of older stable lines — that is the whole point of the
    // strip: "is 2.0 worth trying over 1.3?".
    let mut pool: Vec<(bool, usize, &AtomEntry)> = others
        .iter()
        .enumerate()
        .filter(|(_, e)| !e.tag.is_empty() && entry_score(&e.tag, &e.title, repo) > 0)
        .map(|(i, e)| (is_prerelease(&e.tag), i, e))
        .collect();
    pool.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, _, e) in pool {
        if out.len() >= 3 {
            break;
        }
        let line = release_line(&e.tag);
        if lines.contains(&line) {
            continue;
        }
        lines.push(line);
        out.push(e.clone());
    }
    out
}

/// `v2.0.0-preview.7` → `2.0`, so a dozen previews collapse into a single tab.
fn release_line(tag: &str) -> String {
    let t = tag.trim().trim_start_matches('v');
    let core: String = t
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = core.split('.');
    let major = parts.next().unwrap_or("");
    let minor = parts.next().unwrap_or("");
    if major.is_empty() {
        return t.to_ascii_lowercase();
    }
    if minor.is_empty() {
        // Date-tagged projects (`20260510`, `2026FM`) have no minor component — one
        // tab per build would just list old snapshots, so they share a single line.
        return "solo".to_string();
    }
    format!("{major}.{minor}")
}

/// Cheap sanity checks about one release's asset list.
fn asset_warnings(assets: &[Asset]) -> Vec<String> {
    let mut w = Vec::new();
    if assets.is_empty() {
        w.push("该 Release 没有可下载的附件（可能只有源码）".to_string());
    } else if current_os() == "windows"
        && !assets.iter().any(|a| platform_of(&a.name) == Some("windows"))
    {
        w.push("该版本没有检测到 Windows 构建".to_string());
    }
    w
}

/// Index of the release that belongs to *this* repository.
///
/// Repos that ship several crates (ripgrep → `ignore-0.4.33`, `globset-0.4.20`, …)
/// put unrelated tags at the top of the feed, so the newest entry is often the
/// wrong one. When no entry looks like a project release we keep the newest.
fn best_entry(entries: &[AtomEntry], repo: &str) -> usize {
    let mut best = 0usize;
    let mut best_score = entry_score(&entries[0].tag, &entries[0].title, repo);
    for (i, e) in entries.iter().enumerate().skip(1) {
        let s = entry_score(&e.tag, &e.title, repo);
        if s > best_score {
            best_score = s;
            best = i;
        }
    }
    if best_score <= 0 {
        0
    } else {
        best
    }
}

/// `v2.0.0-preview.7`, `1.4.0-rc1`, `0.9.0-beta` …
fn is_prerelease(tag: &str) -> bool {
    let t = tag.to_ascii_lowercase();
    ["preview", "alpha", "beta", "-rc", ".rc", "nightly"]
        .iter()
        .any(|m| t.contains(m))
}

fn entry_score(tag: &str, title: &str, repo: &str) -> i32 {    let bare = tag.trim().trim_start_matches('v');
    let repo_l = repo.to_ascii_lowercase();
    let tag_l = tag.to_ascii_lowercase();
    let mut score = 0;
    let pure_version = bare
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
        && bare.chars().all(|c| c.is_ascii_digit() || c == '.');
    if pure_version {
        score += 60;
    }
    // `v2.0.0-preview.7` is not "pure" (`-preview.7` suffix) but it is still a real
    // release of this project, so it must survive the sub-crate penalty below.
    if !pure_version
        && bare
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        && is_prerelease(tag)
    {
        score += 40;
    }
    if tag_l.starts_with(&repo_l) {
        score += 50;
    }
    // `crate-1.2.3` where the prefix is not this repo → a sub-crate release.
    if let Some((prefix, rest)) = tag.split_once('-') {
        let rest_bare = rest.trim_start_matches('v');
        let looks_versioned = rest_bare
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
        if looks_versioned && !prefix.is_empty() && !prefix.eq_ignore_ascii_case(repo) {
            score -= 120;
        }
    }
    if title.to_ascii_lowercase().contains(&repo_l) {
        score += 10;
    }
    score
}

/// `20260510` style tags are dates; the release title (`2026FM`) reads better.
fn version_from_tag(tag: &str, title: &str) -> String {    let t = tag.trim();
    let digits_only = t.chars().all(|c| c.is_ascii_digit());
    if digits_only && t.len() >= 6 && !title.trim().is_empty() {
        title.trim().to_string()
    } else {
        t.trim_start_matches('v').to_string()
    }
}

async fn fetch_assets(
    http: &Http,
    owner: &str,
    repo: &str,
    tag: &str,
) -> Result<Vec<Asset>, String> {
    let url = format!(
        "https://github.com/{owner}/{repo}/releases/expanded_assets/{}",
        encode_segment(tag)
    );
    let page = http.get_text(&url).await?;
    let mut out: Vec<Asset> = Vec::new();
    for chunk in page.body.split("<li").skip(1) {
        let block = chunk.split("</li>").next().unwrap_or(chunk);
        let href = match href_dl_re().captures(block) {
            Some(c) => html::decode_entities(&c[1]),
            None => continue,
        };
        let full = if href.starts_with("http") {
            href.clone()
        } else {
            format!("https://github.com{}", href)
        };
        let name = full
            .split('?')
            .next()
            .unwrap_or(&full)
            .rsplit('/')
            .next()
            .unwrap_or("asset")
            .to_string();
        // Auto-update manifests and blockmaps are not installers — they only add
        // noise to the file list.
        let lower_name = name.to_ascii_lowercase();
        if lower_name.ends_with(".json")
            || lower_name.ends_with(".blockmap")
            || lower_name.ends_with(".yml")
            || lower_name.ends_with(".yaml")
        {
            continue;
        }
        let size = size_re().captures(block).and_then(|c| {
            let v: f64 = c[1].parse().ok()?;
            let mult = match c[2].to_ascii_lowercase().as_str() {
                "kb" => 1024.0,
                "mb" => 1024.0 * 1024.0,
                "gb" => 1024.0 * 1024.0 * 1024.0,
                "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
                _ => 1.0,
            };
            Some((v * mult) as u64)
        });
        let sha256 = sha_re().captures(block).map(|c| c[1].to_ascii_lowercase());
        let date = iso_re().captures(block).map(|c| c[1].to_string());
        if out.iter().any(|a| a.url == full) {
            continue;
        }
        out.push(Asset {
            note: platform_note(&name),
            name,
            url: full,
            kind: AssetKind::Direct,
            size,
            sha256,
            date,
        });
    }
    Ok(out)
}

fn platform_note(name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(os) = platform_of(name) {
        parts.push(
            match os {
                "windows" => "Windows",
                "macos" => "macOS",
                "linux" => "Linux",
                "android" => "Android",
                _ => os,
            }
            .to_string(),
        );
    }
    if let Some(a) = arch_of(name) {
        parts.push(a.to_string());
    }
    if parts.is_empty() {
        "附件".into()
    } else {
        parts.join(" · ")
    }
}

fn encode_segment(s: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
    const SET: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}')
        .add(b'%')
        .add(b'/');
    utf8_percent_encode(s, SET).to_string()
}

// ---------------------------------------------------------------------------
// REST API fallback (used for private repos / when the HTML endpoints break)
// ---------------------------------------------------------------------------

async fn resolve_api(http: &Http, owner: &str, repo: &str) -> Result<Release, String> {
    let body = http
        .get_json(&format!(
            "https://api.github.com/repos/{owner}/{repo}/releases?per_page=8"
        ))
        .await?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("GitHub API 响应解析失败: {e}"))?;
    let arr = v.as_array().ok_or("GitHub API 返回了意外的结构")?;
    let latest = arr.first().ok_or("该仓库没有 Release")?;

    let tag = latest["tag_name"].as_str().unwrap_or("").to_string();
    let title = latest["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&tag)
        .to_string();
    let notes_html = latest["body"].as_str().unwrap_or("");
    let notes = html::to_text(&html::decode_entities(notes_html));

    let mut assets = Vec::new();
    if let Some(list) = latest["assets"].as_array() {
        for a in list {
            let name = a["name"].as_str().unwrap_or("asset").to_string();
            let url = a["browser_download_url"].as_str().unwrap_or("").to_string();
            if url.is_empty() {
                continue;
            }
            assets.push(Asset {
                note: platform_note(&name),
                name,
                url,
                kind: AssetKind::Direct,
                size: a["size"].as_u64(),
                sha256: a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(|s| s.to_string()),
                date: a["updated_at"].as_str().map(|s| s.chars().take(10).collect()),
            });
        }
    }

    let history = arr
        .iter()
        .skip(1)
        .map(|r| {
            let t = r["tag_name"].as_str().unwrap_or("").to_string();
            VersionEntry {
                version: version_from_tag(&t, r["name"].as_str().unwrap_or("")),
                title: r["name"].as_str().unwrap_or(&t).to_string(),
                date: r["published_at"].as_str().map(|s| s.chars().take(10).collect()),
                notes: truncate(&html::to_text(r["body"].as_str().unwrap_or("")), 2500),
                url: r["html_url"].as_str().unwrap_or("").to_string(),
            }
        })
        .collect();

    let warnings = asset_warnings(&assets);
    Ok(Release {
        title,
        version: version_from_tag(&tag, latest["name"].as_str().unwrap_or("")),
        published: latest["published_at"].as_str().map(|s| s.chars().take(10).collect()),
        page_url: format!("https://github.com/{owner}/{repo}/releases"),
        provider: "GitHub API".into(),
        summary: first_lines(&notes, 420),
        notes: truncate(&notes, 12000),
        assets,
        history,
        fetched_at: util::now_secs(),
        warnings,
        candidates: Vec::new(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "\n…"
    }
}

fn first_lines(s: &str, max: usize) -> String {
    let mut lines: Vec<&str> = s
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    // Release notes usually start with a short heading (or a title echo). Skip one
    // such line so the blurb opens with actual content.
    if lines.len() > 2 {
        let first = lines[0];
        let heading_like = first.chars().count() <= 18
            && !first.contains(['。', '：', ':', '，', ',', '.', '!', '?']);
        if heading_like {
            lines.remove(0);
        }
    }
    let compact = lines.join("\n");
    truncate(&compact, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_entry_parsing() {
        let xml = r#"<feed><entry>
            <title>v1.2.3</title>
            <updated>2026-01-02T03:04:05Z</updated>
            <link rel="alternate" type="text/html" href="https://github.com/a/b/releases/tag/v1.2.3"/>
            <content type="html">&lt;p&gt;修复了 &amp;amp; 优化&lt;/p&gt;</content>
        </entry></feed>"#;
        let e = parse_atom(xml);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].tag, "v1.2.3");
        assert_eq!(e[0].date.as_deref(), Some("2026-01-02"));
        assert_eq!(e[0].notes, "修复了 & 优化");
    }

    #[test]
    fn tag_versions() {
        assert_eq!(version_from_tag("v0.2.113", "v0.2.113"), "0.2.113");
        assert_eq!(version_from_tag("20260510", "2026FM"), "2026FM");
    }

    #[test]
    fn skips_sub_crate_releases() {
        let entry = |tag: &str| AtomEntry {
            title: tag.to_string(),
            tag: tag.to_string(),
            url: format!("https://github.com/o/r/releases/tag/{tag}"),
            date: None,
            notes: String::new(),
        };
        // ripgrep publishes `ignore-*`/`globset-*` releases above its own tags.
        let feed = vec![
            entry("ignore-0.4.33"),
            entry("14.1.1"),
            entry("globset-0.4.20"),
        ];
        assert_eq!(best_entry(&feed, "ripgrep"), 1);

        // A product-prefixed tag on a repo without a "clean" release stays newest.
        let prefixed = vec![entry("mpv-lazy-20241013"), entry("mpv-lazy-20240901")];
        assert_eq!(best_entry(&prefixed, "mpv_PlayKit"), 0);
    }

    #[test]
    fn release_lines_and_candidates() {
        assert_eq!(release_line("v2.0.0-preview.7"), "2.0");
        assert_eq!(release_line("1.3.0"), "1.3");
        assert_eq!(release_line("main-snapshot"), "main-snapshot");
        // Date tags have no line — otherwise mpv-lazy gets a tab per build date.
        assert_eq!(release_line("20260510"), release_line("20260210"));

        let entry = |tag: &str| AtomEntry {
            title: tag.to_string(),
            tag: tag.to_string(),
            url: format!("https://github.com/nyakang/nyaterm/releases/tag/{tag}"),
            date: None,
            notes: String::new(),
        };
        // nyaterm ships previews of 2.0 above the stable 1.3, and 1.2 is still around.
        let others = vec![
            entry("v2.0.0-preview.7"),
            entry("v2.0.0-preview.6"),
            entry("v1.2.12"),
            entry("v1.2.11"),
            entry("main-snapshot"),
            entry("v1.3.1"),
        ];
        let cands = candidate_entries(&entry("v1.3.0"), &others, "nyaterm");
        let tags: Vec<&str> = cands.iter().map(|c| c.tag.as_str()).collect();
        // Stable first, then the newest preview, then one tab per line — max three.
        assert_eq!(tags, vec!["v1.3.0", "v2.0.0-preview.7", "v1.2.12"]);
        assert!(is_prerelease(&cands[1].tag));
        // A repo with a single line stays tab-less.
        let single = candidate_entries(&entry("v0.2.113"), &[entry("v0.2.112")], "deepxcode");
        assert_eq!(single.len(), 1);
    }
}
