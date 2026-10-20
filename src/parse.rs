//! Turn a free-form paste into tracked items.
//!
//! The user's input is deliberately loose — repo URLs with a nickname, a site root
//! followed by a list of product names, `@scene#54:"日常开发"` markers copied out of
//! another tool. The parser below is line based:
//!
//! * a line containing a URL opens a "scope" (repo / site / article);
//! * bare text lines inherit the most recent site scope (so a site root followed by
//!   four product names yields four tracked items);
//! * `@scene#N:"group"` is stripped and used as the group name for that item.

use std::sync::OnceLock;

use regex::Regex;

use crate::model::{SourceKind, Track};
use crate::util;

fn url_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"https?://[^\s"'<>（）()\[\]{}，。；]+"#).unwrap())
}

fn scene_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // `@scene#54:"日常开发"` (quoted) or `@scene#7:工具` (bare) both work.
    R.get_or_init(|| Regex::new(r#"@scene#\d+(?:\s*[:：]\s*"?([^"\s]+)"?)?"#).unwrap())
}

pub struct Parsed {
    pub tracks: Vec<Track>,
    /// Lines we could not attach to a source (shown in the add dialog).
    pub skipped: Vec<String>,
}

fn group_for(host: &str) -> &'static str {
    let h = host.to_lowercase();
    if h.contains("423down") || h.contains("masuit") {
        "软件站"
    } else if h.contains("pc528") {
        "系统镜像"
    } else if h.contains("github") {
        "GitHub 项目"
    } else {
        "网页追踪"
    }
}

fn strip_github(url: &str) -> Option<(String, String)> {
    let host = util::host_of(url);
    if !util::is_github_host(&host) {
        return None;
    }
    let path = url.split("://").nth(1)?.splitn(2, '/').nth(1).unwrap_or("");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    const RESERVED: [&str; 12] = [
        "orgs",
        "topics",
        "search",
        "marketplace",
        "features",
        "about",
        "pricing",
        "login",
        "collections",
        "sponsors",
        "apps",
        "settings",
    ];
    if RESERVED.contains(&segs[0].to_lowercase().as_str()) {
        return None;
    }
    let owner = segs[0].trim_end_matches(".git");
    let repo = segs[1].trim_end_matches(".git");
    Some((owner.to_string(), repo.to_string()))
}

fn is_site_root(url: &str) -> bool {
    if let Some(rest) = url.split("://").nth(1) {
        let path = rest.splitn(2, '/').nth(1).unwrap_or("");
        let path = path.split(['?', '#']).next().unwrap_or("");
        path.is_empty() || path == "/"
    } else {
        false
    }
}

fn base_of(url: &str) -> String {
    let host = util::host_of(url);
    let scheme = url.split("://").next().unwrap_or("https");
    format!("{}://{}", scheme, host)
}

/// Parse a pasted block into tracks.
pub fn parse_input(text: &str) -> Parsed {
    let mut tracks: Vec<Track> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut site: Option<(String, &'static str)> = None; // (base url, group)
    let mut pending_group: Option<String> = None;

    for raw in text.lines() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') || raw.starts_with("//") {
            continue;
        }
        // `@scene#54:"日常开发"` — either on its own line or trailing the item line.
        let mut group = pending_group.take();
        let mut line = raw.to_string();
        if let Some(c) = scene_re().captures(raw) {
            if let Some(g) = c.get(1) {
                let g = g.as_str().trim();
                if !g.is_empty() {
                    group = Some(g.to_string());
                }
            }
            line = scene_re().replace_all(raw, " ").to_string();
        }
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            pending_group = group;
            continue;
        }

        if let Some(m) = url_re().find(&line) {
            let url_raw = m.as_str().to_string();
            let url = util::normalize_url(&url_raw);
            let hint = {
                let before = &line[..m.start()];
                let after = &line[m.end()..];
                format!("{}{}", before, after).trim().to_string()
            };
            site = None;

            if let Some((owner, repo)) = strip_github(&url) {
                let repo_url = format!("https://github.com/{owner}/{repo}");
                let name = if hint.is_empty() {
                    repo.clone()
                } else {
                    hint.clone()
                };
                push_unique(
                    &mut tracks,
                    Track::new(
                        name.clone(),
                        name,
                        repo_url,
                        SourceKind::GitHub,
                        group.unwrap_or_else(|| "GitHub 项目".into()),
                        raw,
                    ),
                );
                continue;
            }

            if is_site_root(&url) {
                let base = base_of(&url);
                let g = group_for(&util::host_of(&base));
                site = Some((base.clone(), g));
                if !hint.is_empty() {
                    push_unique(
                        &mut tracks,
                        Track::new(
                            hint.clone(),
                            hint.clone(),
                            base,
                            SourceKind::Site,
                            group.unwrap_or_else(|| g.to_string()),
                            raw,
                        ),
                    );
                } else if group.is_some() {
                    // A bare site root produced no item, so keep the `@scene#`
                    // group for the product line(s) that follow it.
                    pending_group = group;
                }
                continue;
            }

            // Concrete page (article / download page).
            let name = if hint.is_empty() {
                // Fall back to the last path segment; the provider refines it with
                // the real page title once resolved.
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("网页")
                    .trim_end_matches(".html")
                    .to_string()
            } else {
                hint.clone()
            };
            let group = group.unwrap_or_else(|| group_for(&util::host_of(&url)).to_string());
            push_unique(
                &mut tracks,
                Track::new(
                    name.clone(),
                    name,
                    url,
                    SourceKind::Article,
                    group,
                    raw,
                ),
            );
            continue;
        }

        // Bare text: either a product name under the current site, or unusable.
        if let Some((base, g)) = &site {
            push_unique(
                &mut tracks,
                Track::new(
                    line.clone(),
                    line,
                    base.clone(),
                    SourceKind::Site,
                    group.unwrap_or_else(|| g.to_string()),
                    raw,
                ),
            );
        } else {
            skipped.push(raw.to_string());
        }
    }

    Parsed { tracks, skipped }
}

fn push_unique(out: &mut Vec<Track>, t: Track) {
    if out
        .iter()
        .any(|x| x.url.eq_ignore_ascii_case(&t.url) && x.query.eq_ignore_ascii_case(&t.query))
    {
        return;
    }
    out.push(t);
}

/// The paste the app ships with — mirrors the user's own watch list.
pub const DEFAULT_INPUT: &str = r#"https://github.com/itmisx/deepx-code  deepxcode
https://github.com/nyakang/nyaterm  nyaterm
https://github.com/Niko1221/Strata  strata
https://www.423down.com/
CrystalDiskInfo
微信电脑版
7-Zip
https://masuit.org/
HWiNFO
https://www.pc528.net/
Windows11 26H2  不忘初心游戏版
https://github.com/hooke007/mpv_PlayKit/releases  mpv-lazy @scene#54:"日常开发"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_full_paste() {
        let p = parse_input(DEFAULT_INPUT);
        assert_eq!(p.tracks.len(), 9, "{:?}", p.tracks.iter().map(|t| &t.name).collect::<Vec<_>>());
        let gh: Vec<_> = p.tracks.iter().filter(|t| t.kind == SourceKind::GitHub).collect();
        assert_eq!(gh.len(), 4);
        assert!(gh.iter().any(|t| t.url == "https://github.com/itmisx/deepx-code"));
        assert!(gh.iter().any(|t| t.name == "mpv-lazy" && t.group == "日常开发"));
        let site: Vec<_> = p.tracks.iter().filter(|t| t.kind == SourceKind::Site).collect();
        assert_eq!(site.len(), 5);
        assert!(site
            .iter()
            .any(|t| t.name == "微信电脑版" && t.url.contains("423down")));
        assert!(site
            .iter()
            .any(|t| t.name == "HWiNFO" && t.url.contains("masuit")));
        assert!(site
            .iter()
            .any(|t| t.name == "Windows11 26H2 不忘初心游戏版" && t.url.contains("pc528")));
    }

    #[test]
    fn article_url() {
        let p = parse_input("https://www.423down.com/5228.html 7-Zip");
        assert_eq!(p.tracks.len(), 1);
        assert_eq!(p.tracks[0].kind, SourceKind::Article);
    }

    #[test]
    fn scene_tags_quoted_and_bare() {
        let p = parse_input(
            "https://github.com/o/r  proj @scene#54:\"日常开发\"\n\
             https://github.com/o/r2  proj2 @scene#7:工具\n\
             @scene#3:待整理\n\
             https://www.423down.com/\nEverything",
        );
        assert_eq!(p.tracks.len(), 3, "{:?}", p.tracks.iter().map(|t| &t.name).collect::<Vec<_>>());
        assert_eq!(p.tracks[0].group, "日常开发");
        assert_eq!(p.tracks[1].group, "工具");
        // A lone `@scene#` line groups the item that follows it, and never becomes
        // a track of its own.
        assert_eq!(p.tracks[2].name, "Everything");
        assert_eq!(p.tracks[2].group, "待整理");
    }
}
