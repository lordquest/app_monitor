//! Web provider: site search + article extraction.
//!
//! Works with WordPress / ZBlog style download sites (423down, pc528, and most
//! similar Chinese software blogs) without per-site code:
//!
//! 1. `GET <site>/?s=<keyword>` — both engines support this;
//! 2. if the site redirects straight to an article, use it;
//! 3. otherwise parse the result list, score every title against the keyword
//!    (ASCII words + CJK bigrams, synonym folding) and pick the best;
//! 4. pull version / date / changelog / download links out of the article.
//!
//! Download links come in three flavours and are labelled accordingly:
//! direct files (`.exe/.zip/…`), net-disk shares (Quark/123pan/Lanzou/Baidu…),
//! and ordinary pages (mirror chooser, login-gated download area).

use std::sync::OnceLock;

use regex::Regex;

use crate::html::{self, Anchor};
use crate::http::Http;
use crate::model::{platform_of, Asset, AssetKind, Release, Track};
use crate::util;

fn h2_anchor_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?is)<h[23][^>]*>.*?<a[^>]+href\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a>.*?</h[23]>"#)
            .unwrap()
    })
}

fn date_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(20\d{2})[年./-]\s?(\d{1,2})[月./-]\s?(\d{1,2})").unwrap())
}

fn ip_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^https?://(?:\d{1,3}\.){3}\d{1,3}").unwrap())
}

const PAN_HOSTS: [&str; 22] = [
    "pan.baidu.com",
    "pan.quark.cn",
    "123pan.com",
    "123684.com",
    "lanzou",
    "lan.z",
    "cloud.189.cn",
    "aliyundrive.com",
    "alipan.com",
    "guangyapan.com",
    "115.com",
    "weiyun.com",
    "ctfile.com",
    "caiyun.139.com",
    "6pan.cn",
    "pan.xunlei.com",
    "jianguoyun.com",
    "wp.123pan.cn",
    "pan.123684.com",
    "share.weiyun.com",
    "drive.uc.cn",
    "fast.uc.cn",
];

const FILE_EXTS: [&str; 14] = [
    ".exe", ".msi", ".msix", ".zip", ".7z", ".rar", ".iso", ".img", ".dmg", ".pkg", ".tar.gz",
    ".tgz", ".apk", ".cab",
];

fn base_url(url: &str) -> String {
    let host = util::host_of(url);
    let scheme = url.split("://").next().unwrap_or("https");
    format!("{}://{}", scheme, host)
}

fn query_of(track: &Track) -> String {
    let q = track.query.trim();
    if q.is_empty() {
        track.name.trim().to_string()
    } else {
        q.to_string()
    }
}

fn encode_query(q: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
    const SET: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'<')
        .add(b'>')
        .add(b'%')
        .add(b'&')
        .add(b'=')
        .add(b'+')
        .add(b'?');
    utf8_percent_encode(q, SET).to_string()
}

fn norm_compare(a: &str, b: &str) -> bool {
    a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'))
}

/// Search entry points, most specific first.
///
/// Most Chinese software blogs are WordPress/ZBlog and answer `/?s=<keyword>`;
/// some (masuit.org) have their own route (`/search?wd=`). Trying a short list
/// beats hard-coding one pattern per site.
fn search_urls(base: &str, q: &str) -> Vec<String> {
    let host = util::host_of(base);
    let qe = encode_query(q);
    let mut out: Vec<String> = Vec::new();
    let mut push = |u: String| {
        if !out.iter().any(|x| x == &u) {
            out.push(u);
        }
    };
    if host.contains("masuit") {
        push(format!("{base}/search?wd={qe}"));
    }
    push(format!("{base}/?s={qe}"));
    push(format!("{base}/search?wd={qe}"));
    push(format!("{base}/search?q={qe}"));
    out
}

/// Resolve a site root + keyword into a release.
pub async fn resolve_site(http: &Http, track: &Track) -> Result<Release, String> {
    let base = base_url(&track.url);
    let q = query_of(track);

    let mut errors: Vec<String> = Vec::new();
    let mut listing_pages: Vec<(String, String)> = Vec::new(); // (final_url, body)
    let mut cands: Vec<(String, String)> = Vec::new();

    for search_url in search_urls(&base, &q) {
        let page = match http.get_text(&search_url).await {
            Ok(p) => p,
            Err(e) => {
                // A timeout / bot wall is a property of the host, not of this
                // particular pattern: trying the remaining ones just burns time.
                if host_level_error(&e) {
                    return Err(e);
                }
                errors.push(e);
                continue;
            }
        };
        // 423down canonicalises `?s=x` to `/search/x`, so for that site the page
        // markup matters more than the URL. A redirect to the homepage, on the
        // other hand, means this pattern is wrong for the site.
        let redirected = !norm_compare(&page.final_url, &search_url);
        let listing = html::looks_like_search(&page.body)
            || (!redirected && looks_like_listing(&page.body, &base));
        if !listing {
            // The search route jumped straight to the article (423down does this
            // for an exact keyword hit).
            let title = html::page_title(&page.body).unwrap_or_default();
            if util::score_title(&title, &q) >= 0.3 && !norm_compare(&page.final_url, &base) {
                return extract_article(&page.body, &page.final_url, &q, Some(title));
            }
            continue;
        }
        let found = parse_results(&page.body, &page.final_url);
        if found.is_empty() {
            listing_pages.push((page.final_url, page.body));
            continue;
        }
        let strong = found
            .iter()
            .any(|(_, t)| util::score_title(t, &q) >= 0.28);
        listing_pages.push((page.final_url, page.body));
        cands = found;
        if strong {
            break;
        }
    }

    if cands.is_empty() {
        // Fall back to the site map — some sites disable search but ship a full index.
        if let Ok(sm) = http.get_text(&format!("{base}/sitemap.html")).await {
            cands = parse_results(&sm.body, &base);
        }
    }
    if cands.is_empty() {
        if let Some(e) = errors.into_iter().next() {
            return Err(format!("站点搜索失败：{e}"));
        }
        return Err(format!(
            "站点「{}」的搜索结果为空（可能未收录「{q}」）。可直接粘贴该软件的文章链接来追踪。",
            util::host_of(&base)
        ));
    }

    let best = pick_best(cands, &q).ok_or_else(|| {
        format!(
            "站点「{}」没有与「{q}」匹配的条目（可能未收录）。可直接粘贴该软件的文章链接来追踪。",
            util::host_of(&base)
        )
    })?;

    if let Some((url, body)) = listing_pages
        .iter()
        .find(|(u, _)| norm_compare(u, &best.0))
    {
        return extract_article(body, url, &q, Some(best.1));
    }
    let art = match http.get_text(&best.0).await {
        Ok(a) => a,
        Err(e) => {
            // The listing already told us the title; keep that instead of a red
            // card, so a slow article fetch still leaves something usable.
            return Ok(partial_release(best.1, best.0.clone(), e));
        }
    };
    extract_article(&art.body, &art.final_url, &q, Some(best.1))
}

/// Minimal release built from the search-result title when the article page
/// cannot be fetched (timeout, bot protection, …).
fn partial_release(title: String, page_url: String, why: String) -> Release {
    let version = util::guess_version(&title, "");
    Release {
        title,
        version,
        published: None,
        page_url,
        provider: "网页解析".into(),
        summary: String::new(),
        notes: String::new(),
        assets: Vec::new(),
        history: Vec::new(),
        fetched_at: util::now_secs(),
        warnings: vec![format!(
            "正文抓取失败：{why}。版本号取自搜索结果，请点「打开来源」在浏览器中确认"
        )],
        candidates: Vec::new(),
    }
}

/// Errors that are about the host being unreachable or guarded — no point in
/// retrying another search route on the same host.
fn host_level_error(e: &str) -> bool {
    e.contains("超时") || e.contains("人机验证") || e.contains("拒绝了请求") || e.contains("连接")
}

/// Resolve a concrete article URL (falls back to a site search when it moved).
pub async fn resolve_article(http: &Http, track: &Track) -> Result<Release, String> {
    let q = query_of(track);
    match http.get_text(&track.url).await {
        Ok(page) => {
            if looks_like_listing(&page.body, &base_url(&track.url)) && !html::looks_like_search(&page.body) {
                let cands = parse_results(&page.body, &base_url(&track.url));
                if let Some(best) = pick_best(cands, &q) {
                    let art = http.get_text(&best.0).await?;
                    return extract_article(&art.body, &art.final_url, &q, Some(best.1));
                }
            }
            extract_article(&page.body, &page.final_url, &q, None)
        }
        Err(err) => {
            let base = base_url(&track.url);
            let search_url = format!("{base}/?s={}", encode_query(&q));
            match http.get_text(&search_url).await {
                Ok(page) => {
                    let cands = parse_results(&page.body, &base);
                    if let Some(best) = pick_best(cands, &q) {
                        let art = http.get_text(&best.0).await?;
                        extract_article(&art.body, &art.final_url, &q, Some(best.1))
                    } else {
                        Err(format!("{err}（回退到站内搜索也没有找到「{q}」）"))
                    }
                }
                Err(_) => Err(err),
            }
        }
    }
}

fn looks_like_listing(body: &str, base: &str) -> bool {
    if html::looks_like_search(body) {
        return true;
    }
    let mut hits = 0;
    for c in h2_anchor_re().captures_iter(body) {
        let href = c.get(1).map(|m| m.as_str()).unwrap_or("");
        if is_internal(href, base) {
            hits += 1;
        }
    }
    hits >= 3
}

fn is_internal(href: &str, base: &str) -> bool {
    let host = util::host_of(base);
    if href.starts_with('/') {
        return true;
    }
    href.contains(&host)
}

fn parse_results(body: &str, base: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();

    // Preferred: anchors wrapped in a result heading.
    for c in h2_anchor_re().captures_iter(body) {
        let href = c.get(1).map(|m| m.as_str()).unwrap_or("");
        let title = html::to_text(c.get(2).map(|m| m.as_str()).unwrap_or(""));
        push_result(&mut out, base, href, title);
    }
    // Card templates (`<a class="article-title">`): masuit.org, many blog themes.
    if out.len() < 2 {
        for (href, title) in card_links(body) {
            push_result(&mut out, base, &href, title);
        }
    }
    if out.len() >= 2 {
        return out;
    }

    // Fallback: any internal article-looking anchor with a decent label.
    for a in html::anchors(body) {
        if !(a.href.ends_with(".html") || a.href.contains(".html?")) {
            continue;
        }
        if a.text.chars().count() < 4 {
            continue;
        }
        push_result(&mut out, base, &a.href, a.text);
        if out.len() > 120 {
            break;
        }
    }
    out
}

/// Links carrying a title-ish class (`article-title`, `post-title`, `entry-title`).
fn card_links(body: &str) -> Vec<(String, String)> {
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| Regex::new(r#"(?is)<a\b([^>]*)>(.*?)</a>"#).unwrap());
    let mut out = Vec::new();
    for c in re.captures_iter(body) {
        let attrs = c.get(1).map(|m| m.as_str()).unwrap_or("");
        let lower = attrs.to_ascii_lowercase();
        if !(lower.contains("article-title")
            || lower.contains("post-title")
            || lower.contains("entry-title"))
        {
            continue;
        }
        let Some(href) = attr_value(attrs, "href") else {
            continue;
        };
        let title = html::to_text(c.get(2).map(|m| m.as_str()).unwrap_or(""));
        if title.trim().is_empty() {
            continue;
        }
        // Keyword highlight spans leave a space behind ("—— HWiNFO").
        let title = title.replace("—— ", "——");
        out.push((href, title));
    }
    out
}

/// Read one quoted attribute out of an open tag's attribute string.
fn attr_value(attrs: &str, key: &str) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| {
        Regex::new(r#"(?is)([a-zA-Z_:][-a-zA-Z0-9_:.]*)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap()
    });
    for c in re.captures_iter(attrs) {
        if c[1].eq_ignore_ascii_case(key) {
            return Some(
                c.get(2)
                    .or_else(|| c.get(3))
                    .map(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    None
}

fn push_result(out: &mut Vec<(String, String)>, base: &str, href: &str, title: String) {
    let url = strip_tracking(&absolute(base, href));
    if url.is_empty() || title.trim().is_empty() {
        return;
    }
    let host = util::host_of(&url);
    if !host.eq_ignore_ascii_case(&util::host_of(base)) {
        return;
    }
    let lower = url.to_lowercase();
    if lower.contains("/about")
        || lower.contains("/contact")
        || lower.contains("/tag/")
        || lower.contains("/category/")
        || lower.contains("/cat/")
        || lower.contains("/author/")
        || lower.contains("/special/")
        || lower.contains("/search/")
        || lower.contains("/misc/")
        || lower.contains("/page/")
        || lower.contains("login")
        || lower.contains("register")
    {
        return;
    }
    if out.iter().any(|(u, _)| u == &url) {
        return;
    }
    out.push((url, title.trim().to_string()));
}

/// Drop search-context / tracking parameters. `…/1555?kw=HWiNFO&t=0HNPC…`
/// makes the server re-render the page with keyword highlighting, which is both
/// slower and occasionally redirected; the canonical article URL is enough.
fn strip_tracking(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| {
            let Some((k, _)) = p.split_once('=') else {
                return !p.is_empty();
            };
            let k = k.trim().to_ascii_lowercase();
            !(k == "kw"
                || k == "t"
                || k == "from"
                || k == "spm"
                || k == "ref"
                || k.starts_with("utm_"))
        })
        .filter(|p| !p.is_empty())
        .collect();
    if kept.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", kept.join("&"))
    }
}

fn pick_best(cands: Vec<(String, String)>, query: &str) -> Option<(String, String)> {
    // Highest relevance wins; on ties the search engine's own ordering is kept
    // (it is usually recency-ranked, which we cannot reproduce reliably).
    let mut best: Option<(f64, (String, String))> = None;
    for (url, title) in cands {
        let s = util::score_title(&title, query);
        if s < 0.28 {
            continue;
        }
        if best.as_ref().map(|(b, _)| s > *b).unwrap_or(true) {
            best = Some((s, (url, title)));
        }
    }
    best.map(|(_, c)| c)
}

/// Join a possibly-relative href onto the page URL.
fn absolute(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return String::new();
    }
    if href.starts_with("//") {
        let scheme = base.split("://").next().unwrap_or("https");
        return format!("{scheme}:{href}");
    }
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let b = base.trim_end_matches('/');
    if href.starts_with('/') {
        let scheme = b.split("://").next().unwrap_or("https");
        let host = b.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
        return format!("{scheme}://{host}{href}");
    }
    // Relative to the document: drop the last path segment of the page URL
    // (`/b/c.html` + `d.zip` → `/b/d.zip`), and resolve leading `../`.
    let scheme_end = b.find("://").map(|i| i + 3).unwrap_or(0);
    let mut dir = match b.rfind('/') {
        Some(pos) if pos >= scheme_end => &b[..pos],
        _ => b,
    };
    let mut rest = href;
    while let Some(stripped) = rest.strip_prefix("../") {
        rest = stripped;
        if let Some(pos) = dir.rfind('/') {
            if pos >= scheme_end {
                dir = &dir[..pos];
            }
        }
    }
    format!("{dir}/{rest}")
}

fn extract_article(
    body: &str,
    page_url: &str,
    query: &str,
    listing_title: Option<String>,
) -> Result<Release, String> {
    if body.chars().count() < 200 {
        return Err("页面内容为空或过短".into());
    }
    let title = listing_title
        // A listing title that still carries markup is worse than no title at all:
        // `guess_version` reads numbers out of it (`font-size: 1.1em` → "1.1"), so
        // fall through to the article page's own title instead.
        .filter(|t| !t.trim().is_empty() && !t.contains('<'))
        .or_else(|| html::page_title(body))
        .unwrap_or_else(|| query.to_string());
    let title = if title.trim().is_empty() { query.to_string() } else { title };

    let notes = html::article_text(body, 16000);
    let version = util::guess_version(&title, &notes);
    let date = page_date(body);
    let mut warnings: Vec<String> = Vec::new();

    let mut assets = collect_assets(body, page_url, &mut warnings);
    if assets.is_empty() {
        warnings.push("未能在页面中自动找到下载链接，请使用「打开页面」查看".into());
    } else if !assets.iter().any(|a| a.kind == AssetKind::Direct) {
        warnings.push("该站只提供网盘或跳转页面，下载需在浏览器中打开".into());
    }
    assets.truncate(40);

    let summary = update_blurb(&notes).unwrap_or_else(|| tidy_summary(&notes, &title));

    Ok(Release {
        title,
        version,
        published: date,
        page_url: page_url.to_string(),
        provider: "网页解析".into(),
        summary,
        notes,
        assets,
        history: Vec::new(),
        fetched_at: util::now_secs(),
        warnings,
        candidates: Vec::new(),
    })
}

fn page_date(body: &str) -> Option<String> {
    // Prefer the last-modified stamp: download pages keep the original post date
    // forever, so "发布于 2019" says nothing about the build we are looking at.
    for key in [
        "article:modified_time",
        "article:updated_time",
        "og:updated_time",
        "article:published_time",
        "og:release_date",
    ] {
        if let Some(t) = html::meta(body, key) {
            if t.len() >= 10 {
                return Some(t.chars().take(10).collect());
            }
        }
    }
    // Prefer a date near the top of the page (article header), not in comments.
    let head: String = body.chars().take(200_000).collect();
    let mut best: Option<(usize, String)> = None;
    for c in date_re().captures_iter(&head) {
        let y: i32 = c[1].parse().unwrap_or(0);
        if !(2000..=2100).contains(&y) {
            continue;
        }
        let m: u32 = c[2].parse().unwrap_or(0);
        let d: u32 = c[3].parse().unwrap_or(0);
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            continue;
        }
        let start = c.get(0).map(|m| m.start()).unwrap_or(usize::MAX);
        best = Some((start, format!("{:04}-{:02}-{:02}", y, m, d)));
        break;
    }
    best.map(|(_, s)| s)
}

fn update_blurb(notes: &str) -> Option<String> {
    const MARKERS: [&str; 11] = [
        "更新日志",
        "更新记录",
        "更新说明",
        "更新内容",
        "版本更新",
        "新版变化",
        "更新历史",
        "What's Changed",
        "Changelog",
        "What's new",
        "Change log",
    ];
    let mut best: Option<usize> = None;
    for m in MARKERS {
        if let Some(i) = notes.find(m) {
            best = Some(best.map(|b| b.min(i)).unwrap_or(i));
        }
    }
    let start = best?;
    let mut tail = &notes[start..];
    // Drop a heading-only first line ("更新内容" on its own).
    if let Some(nl) = tail.find('\n') {
        if tail[..nl].trim().chars().count() <= 12 {
            tail = &tail[nl + 1..];
        }
    }
    let tail: String = tail.chars().take(560).collect();
    let tail = tail.trim().to_string();
    // Some pages put an image or a chat-group link under "新版变化"; a blurb that
    // short is worse than the article intro.
    if tail.chars().count() < 40 || tail.contains("交流群") || tail.contains("公众号") {
        return None;
    }
    Some(tail)
}

/// Fallback blurb: skip the title echo and the date / view-count metadata that
/// article headers put in front of the actual text.
fn tidy_summary(notes: &str, title: &str) -> String {
    let nt = util::norm(title);
    let mut out: Vec<&str> = Vec::new();
    for line in notes.lines().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        let line = strip_meta_prefix(line);
        if line.is_empty() {
            continue;
        }
        let ln = util::norm(line);
        if out.is_empty() && !nt.is_empty() && (ln == nt || (ln.len() > 6 && nt.contains(&ln)))
        {
            continue;
        }
        if is_date_or_count(line) || line.starts_with("分类") || line.starts_with("评论")
            || line.starts_with("阅读") || line.contains('<')
        {
            continue;
        }
        // Navigation crumbs and bare links ("History – Crystal Dew World",
        // "https://…") say nothing about what changed.
        if is_bare_url(line) || looks_like_crumb(line) {
            continue;
        }
        // "… 新版变化 官方交流群 145378303" — keep the part after the marker
        // only when it actually describes the update.
        if let Some(tail) = after_marker(line) {
            let noisy = tail.chars().count() < 12
                || tail.contains("交流群")
                || tail.contains("公众号");
            if noisy {
                continue;
            }
            out.push(tail);
        } else {
            // A bare section heading ("功能摘要") means the intro is over.
            if !out.is_empty()
                && line.chars().count() <= 8
                && !line.contains(['，', '。', '：', '；', ',', '.'])
            {
                break;
            }
            out.push(line);
        }
        if out.len() >= 4 {
            break;
        }
    }
    let flat = out.join(" ");
    if flat.chars().count() > 420 {
        flat.chars().take(420).collect::<String>() + "…"
    } else {
        flat
    }
}

fn is_bare_url(line: &str) -> bool {
    let l = line.trim();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("www.")
}

/// Short "Page – Site Name" style strings that came from a link, not prose.
fn looks_like_crumb(line: &str) -> bool {
    let has_dash = line.contains(" – ") || line.contains(" — ") || line.contains(" - ");
    if !has_dash {
        return false;
    }
    let ascii = line.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let cjk = line.chars().filter(|c| !c.is_ascii()).count();
    ascii > 0 && cjk == 0 && line.chars().count() <= 40
}

/// `2026-10-12 分类： 聊天软件 微信电脑版…` → `微信电脑版…`
fn strip_meta_prefix(line: &str) -> &str {
    for marker in ["分类：", "分类:", "栏目：", "栏目:"] {
        if let Some(i) = line.find(marker) {
            let tail = &line[i + marker.len()..];
            let tail = tail.trim_start();
            // Drop the single category word that follows the marker.
            let cut = tail
                .char_indices()
                .find(|(_, c)| c.is_whitespace())
                .map(|(idx, _)| idx)
                .unwrap_or(tail.len());
            return tail[cut..].trim_start();
        }
    }
    line
}

/// Text that follows a "what's new" marker inside a single line.
fn after_marker(line: &str) -> Option<&str> {
    const MARKERS: [&str; 4] = ["新版变化", "更新内容", "更新日志", "更新记录"];
    for m in MARKERS {
        if let Some(i) = line.find(m) {
            return Some(line[i + m.len()..].trim_start_matches([':', '：', '】', ' ']));
        }
    }
    None
}

fn is_date_or_count(line: &str) -> bool {
    !line.is_empty()
        && line
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '-' | '/' | '.' | '年' | '月' | '日' | ' '))
}

fn collect_assets(body: &str, page_url: &str, warnings: &mut Vec<String>) -> Vec<Asset> {
    let article = html::article_html(body);
    let mut anchors = html::anchors(&article);
    if anchors.len() < 3 {
        anchors.extend(html::anchors(body));
    }
    if body.contains("secret-content-box") || (body.contains("回复") && body.contains("可见")) {
        warnings.push("该站下载区需要登录 / 回复后可见，请打开原始页面获取".into());
    }

    let mut out: Vec<Asset> = Vec::new();
    for a in anchors {
        if let Some(asset) = classify(a, page_url) {
            if !out.iter().any(|x| x.url == asset.url) {
                out.push(asset);
            }
        }
    }
    out
}

fn classify(a: Anchor, page_url: &str) -> Option<Asset> {
    let mut href = a.href.trim().to_string();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }

    // 423down wraps every outgoing mirror link as /go.php?url=<base64>.
    if let Some(pos) = href.find("go.php?url=") {
        let b64 = &href[pos + "go.php?url=".len()..];
        let b64 = b64.split('&').next().unwrap_or(b64);
        if let Some(decoded) = util::decode_base64(b64) {
            href = decoded;
        }
    }
    if href.starts_with("magnet:") || href.starts_with("ed2k:") {
        let label = if a.text.is_empty() {
            if href.starts_with("magnet:") {
                "磁力链接".to_string()
            } else {
                "电驴链接".to_string()
            }
        } else {
            a.text.clone()
        };
        return Some(Asset {
            name: label,
            url: href,
            kind: AssetKind::Pan,
            size: None,
            note: "P2P".into(),
            sha256: None,
            date: None,
        });
    }

    let url = absolute(page_url, &href);
    if url.is_empty() {
        return None;
    }
    let host = util::host_of(&url);
    let lower_url = url.to_lowercase();
    let label = if a.text.trim().is_empty() {
        a.context.chars().rev().take(40).collect::<String>().chars().rev().collect::<String>()
    } else {
        a.text.trim().to_string()
    };

    let is_pan = PAN_HOSTS.iter().any(|h| host.contains(h));
    let has_ext = FILE_EXTS.iter().any(|e| lower_url.split('?').next().unwrap_or("").ends_with(e));

    if is_pan {
        let brand = pan_brand(&host);
        let name = if has_ext {
            file_name_of(&url)
        } else if !label.is_empty() && !label.starts_with("http") {
            format!("{label} · {brand}")
        } else {
            format!("{brand} 分享链接")
        };
        return Some(Asset {
            name,
            url,
            kind: AssetKind::Pan,
            size: None,
            note: brand.to_string(),
            sha256: None,
            date: None,
        });
    }

    if has_ext && !is_ip_url(&url) {
        let name = file_name_of(&url);
        let note = platform_note(&name);
        return Some(Asset {
            name,
            url,
            kind: AssetKind::Direct,
            size: None,
            note,
            sha256: None,
            date: None,
        });
    }

    // Ordinary page: keep it only when the label says it is a download entry point.
    let label_lower = label.to_lowercase();
    let looks_like_download = ["下载地址", "原版下载", "镜像下载", "下载页", "官网", "download", "mirror"]
        .iter()
        .any(|k| label_lower.contains(k))
        && !label_lower.contains("点此");
    if looks_like_download && !label.is_empty() {
        return Some(Asset {
            name: label.chars().take(60).collect(),
            url,
            kind: AssetKind::Page,
            size: None,
            note: "页面".into(),
            sha256: None,
            date: None,
        });
    }
    None
}

fn is_ip_url(url: &str) -> bool {
    ip_re().is_match(url)
}

fn pan_brand(host: &str) -> &'static str {
    if host.contains("quark") {
        "夸克网盘"
    } else if host.contains("123") {
        "123 网盘"
    } else if host.contains("lanzou") || host.contains("lan.z") {
        "蓝奏云"
    } else if host.contains("baidu") {
        "百度网盘"
    } else if host.contains("189") {
        "天翼云盘"
    } else if host.contains("aliyun") || host.contains("alipan") {
        "阿里云盘"
    } else if host.contains("guangya") {
        "光雅盘"
    } else if host.contains("115") {
        "115 网盘"
    } else if host.contains("weiyun") {
        "微云"
    } else if host.contains("ctfile") {
        "城通网盘"
    } else if host.contains("xunlei") {
        "迅雷云盘"
    } else if host.contains("139") {
        "移动云盘"
    } else {
        "网盘"
    }
}

fn file_name_of(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    let raw = path.rsplit('/').next().unwrap_or("file");
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .to_string();
    if decoded.is_empty() {
        "file".into()
    } else {
        decoded
    }
}

fn platform_note(name: &str) -> String {
    match platform_of(name) {
        Some("windows") => "Windows".into(),
        Some("macos") => "macOS".into(),
        Some("linux") => "Linux".into(),
        Some("android") => "Android".into(),
        _ => "直链".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_urls() {
        assert_eq!(
            absolute("https://www.423down.com/5228.html", "/go.php?url=x"),
            "https://www.423down.com/go.php?url=x"
        );
        assert_eq!(
            absolute("https://a.com/b/c.html", "d.zip"),
            "https://a.com/b/d.zip"
        );
    }

    #[test]
    fn pan_link_decoding() {
        let a = Anchor {
            href: "/go.php?url=aHR0cHM6Ly9wYW4ucXVhcmsuY24vcy85YzM2MWZlMmIwY2M=".into(),
            text: "https://pan.quark.cn/s/9c361fe2b0cc".into(),
            context: String::new(),
        };
        let asset = classify(a, "https://www.423down.com/5228.html").unwrap();
        assert_eq!(asset.kind, AssetKind::Pan);
        assert_eq!(asset.url, "https://pan.quark.cn/s/9c361fe2b0cc");
        assert_eq!(asset.note, "夸克网盘");
    }

    #[test]
    fn direct_file() {
        let a = Anchor {
            href: "https://d.example.com/7z2604-x64.exe".into(),
            text: "本地下载".into(),
            context: String::new(),
        };
        let asset = classify(a, "https://example.com/p.html").unwrap();
        assert_eq!(asset.kind, AssetKind::Direct);
        assert_eq!(asset.note, "Windows");
    }

    /// Verbatim structure of masuit.org's `/search?wd=` result page: no `<h2>`
    /// headings, one `<a class="article-title">` per card, keyword highlighted
    /// with inline `<span>`s.
    const MASUIT_SEARCH: &str = r#"
        <div class="article-card"><div class="article-content">
          <a class="article-title" target="_blank" href="/2307?kw=HWiNFO">
            盘古之殇：华为诺亚盘古大模型研发历程的心酸与黑暗
          </a></div></div>
        <div class="article-card"><div class="article-content">
          <a class="article-title" target="_blank" href="/1555?kw=HWiNFO">
            硬件检测工具——<span style='color:red;background-color:yellow;font-size: 1.1em;font-weight:700;'>HWiNFO</span> 8.53中文版
          </a>
          <div class="meta-row"><span class="author"><a href="/author/ldqk">懒得勤快</a></span></div>
        </div></div>
        <a class="article-title" href="/cat/15">绿色软件</a>
    "#;

    #[test]
    fn masuit_card_results() {
        let cands = parse_results(MASUIT_SEARCH, "https://masuit.org/search/HWiNFO");
        assert_eq!(cands.len(), 2, "{cands:?}");
        // `?kw=` is search-context noise and gets dropped.
        assert!(cands.iter().any(|(u, _)| u == "https://masuit.org/1555"));
        let best = pick_best(cands, "HWiNFO").expect("应命中 HWiNFO 文章");
        assert_eq!(best.0, "https://masuit.org/1555");
        // Highlight `<span>`s around the keyword leave a stray space behind.
        assert_eq!(best.1, "硬件检测工具——HWiNFO 8.53中文版");
    }

    #[test]
    fn masuit_article_extract() {
        let page = r#"
          <html><head>
            <title>硬件检测工具——HWiNFO 8.53中文版_懒得勤快的博客_互联网分享精神</title>
            <meta content="硬件检测工具——HWiNFO 8.53中文版_懒得勤快的博客_互联网分享精神" property="og:title">
            <meta content="2019-06-01 08:14:29" property="article:published_time">
            <meta content="2026-10-12 22:54:06" property="article:modified_time">
          </head><body>
            <section class="article-header"><h2 class="article-title">硬件检测工具——HWiNFO 8.53中文版</h2></section>
            <section class="article-content-card"><div class="article-content">
              <p>HWiNFO 是一款全面的硬件监控和信息检测工具，专为希望深入了解其计算机系统性能和硬件状态的用户设计。</p>
              <h3>功能摘要</h3><p>全面的硬件信息检测。</p>
              <h3>下载地址</h3>
              <p><a href="https://ldqk.lanzoum.com/im3g24bqre4h" target="_blank">https://ldqk.lanzoum.com/im3g24bqre4h</a></p>
            </div></section>
            <div class="article-footer-row">相关推荐</div>
            <div class="comment-content">评论区噪音</div>
          </body></html>
        "#;
        let rel = extract_article(page, "https://masuit.org/1555", "HWiNFO", None).unwrap();
        assert_eq!(rel.title, "硬件检测工具——HWiNFO 8.53中文版");
        assert_eq!(rel.version, "8.53");
        assert_eq!(rel.published.as_deref(), Some("2026-10-12"));
        assert!(rel.notes.contains("下载地址"));
        assert!(!rel.notes.contains("评论区噪音"), "正文不应含评论区: {}", rel.notes);
        assert!(rel
            .assets
            .iter()
            .any(|a| a.kind == AssetKind::Pan && a.note == "蓝奏云"));
    }
}
