//! A deliberately small HTML toolkit.
//!
//! Full DOM parsing is overkill here: every provider only needs titles, meta tags,
//! anchors and a slab of readable body text. Regex + tag stripping keeps the
//! dependency tree small (no html5ever) and is easy to reason about when a site
//! changes its markup.

use std::sync::OnceLock;

use regex::Regex;

fn re_script() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?is)<(script|style|noscript|svg|iframe)\b[^>]*>.*?</(script|style|noscript|svg|iframe)>")
            .unwrap()
    })
}

fn re_break() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)<\s*(br|/p|/div|/li|/tr|/h[1-6]|/section|/article|/td|/dd|/dt|/pre)\b[^>]*>")
            .unwrap()
    })
}

/// Remove tags with a tolerant scanner.
///
/// A plain `<[^>]+>` regex breaks on attributes containing `>` (pc528 has them):
/// the match ends inside the attribute, leaving fragments like `<span` in the text
/// and swallowing the following content. Tracking quotes fixes that.
fn strip_markup(html: &str) -> String {
    let prepared = re_break().replace_all(html, "\n");
    let mut out = String::with_capacity(prepared.len());
    let mut in_tag = false;
    let mut quote: Option<char> = None;
    for ch in prepared.chars() {
        if in_tag {
            match quote {
                Some(q) => {
                    if ch == q {
                        quote = None;
                    }
                }
                None => {
                    if ch == '"' || ch == '\'' {
                        quote = Some(ch);
                    } else if ch == '>' {
                        in_tag = false;
                    }
                }
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            quote = None;
            out.push(' ');
        } else if ch == '>' {
            // stray closing bracket from a broken tag
        } else {
            out.push(ch);
        }
    }
    out
}

fn re_anchor() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*["']([^"']*)["'][^>]*>(.*?)</a>"#).unwrap())
}

fn re_meta() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?is)<meta\b[^>]*>").unwrap())
}

fn re_attr() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?is)([a-zA-Z:_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap()
    })
}

fn re_title() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap())
}

fn re_h1() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?is)<h1[^>]*>(.*?)</h1>").unwrap())
}

/// `<h2 class="article-title">…</h2>` — the real title on blog-style templates.
fn re_article_title() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?is)<h[12][^>]*class\s*=\s*["'][^"']*\b(?:article|post|entry)-title\b[^"']*["'][^>]*>(.*?)</h[12]>"#)
            .unwrap()
    })
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '&' {
            if let Some(semi) = bytes[i..].iter().take(12).position(|c| *c == ';') {
                let ent: String = bytes[i + 1..i + semi].iter().collect();
                let repl = match ent.as_str() {
                    "amp" => Some("&".to_string()),
                    "lt" => Some("<".to_string()),
                    "gt" => Some(">".to_string()),
                    "quot" => Some("\"".to_string()),
                    "apos" => Some("'".to_string()),
                    "nbsp" => Some(" ".to_string()),
                    "mdash" => Some("—".to_string()),
                    "ndash" => Some("–".to_string()),
                    "hellip" => Some("…".to_string()),
                    "middot" => Some("·".to_string()),
                    "times" => Some("×".to_string()),
                    "raquo" => Some("»".to_string()),
                    "laquo" => Some("«".to_string()),
                    "ldquo" => Some("“".to_string()),
                    "rdquo" => Some("”".to_string()),
                    "lsquo" => Some("‘".to_string()),
                    "rsquo" => Some("’".to_string()),
                    _ => {
                        if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32).map(|c| c.to_string())
                        } else if let Some(dec) = ent.strip_prefix('#') {
                            dec.parse::<u32>().ok().and_then(char::from_u32).map(|c| c.to_string())
                        } else {
                            None
                        }
                    }
                };
                if let Some(r) = repl {
                    out.push_str(&r);
                    i += semi + 1;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Drop markup and return collapsed, human-readable text.
pub fn to_text(html: &str) -> String {
    let no_script = re_script().replace_all(html, " ");
    let no_tags = strip_markup(&no_script);
    let decoded = decode_entities(&no_tags);
    collapse(&decoded)
}

fn collapse(s: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank = 0;
    for raw in s.replace('\u{3000}', " ").replace('\u{200b}', "").split('\n') {
        let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

pub fn page_title(html: &str) -> Option<String> {
    // Article templates often keep the site slogan in `<h1>` and the real title
    // in `<h2 class="article-title">` (masuit.org and friends).
    if let Some(c) = re_article_title().captures(html) {
        let t = to_text(&c[1]);
        if sensible_title(&t) {
            return Some(clean_title(&t));
        }
    }
    // Some sites (pc528 is one) ship a broken `og:title` like `<r`; only trust it
    // when it looks like actual text.
    if let Some(t) = meta(html, "og:title").filter(|s| sensible_title(s)) {
        return Some(clean_title(&t));
    }
    if let Some(c) = re_h1().captures(html) {
        let t = to_text(&c[1]);
        if sensible_title(&t) {
            return Some(clean_title(&t));
        }
    }
    re_title()
        .captures(html)
        .map(|c| clean_title(&to_text(&c[1])))
        .filter(|s| sensible_title(s))
}

fn sensible_title(s: &str) -> bool {
    let t = s.trim();
    t.chars().count() >= 2 && !t.contains('<') && !t.starts_with('&')
}

/// Titles on these sites carry a " - SiteName" suffix; drop it. Some templates
/// stack two suffixes (`标题_栏目_站点名`), so repeat a couple of times.
pub fn clean_title(t: &str) -> String {
    let mut cur = t.trim().to_string();
    for _ in 0..3 {
        let mut head_opt: Option<String> = None;
        for sep in [" - ", "－", " | ", "_"] {
            if let Some((head, tail)) = cur.rsplit_once(sep) {
                let tail = tail.trim();
                if !tail.is_empty()
                    && tail.chars().count() <= 16
                    && head.trim().chars().count() >= 6
                {
                    head_opt = Some(head.trim().to_string());
                    break;
                }
            }
        }
        match head_opt {
            Some(h) => cur = h,
            None => break,
        }
    }
    cur
}

pub fn meta(html: &str, key: &str) -> Option<String> {
    for tag in re_meta().find_iter(html) {
        let mut name = "";
        let mut content = "";
        for c in re_attr().captures_iter(tag.as_str()) {
            let k = c[1].to_ascii_lowercase();
            let v = c.get(2).or(c.get(3)).or(c.get(4)).map(|m| m.as_str()).unwrap_or("");
            if k == "property" || k == "name" || k == "itemprop" {
                name = v;
            } else if k == "content" {
                content = v;
            }
        }
        if name.eq_ignore_ascii_case(key) && !content.is_empty() {
            return Some(decode_entities(content));
        }
    }
    None
}

#[derive(Clone, Debug)]
pub struct Anchor {
    pub href: String,
    pub text: String,
    /// Raw text right before the anchor — used as a label when the anchor is an image.
    pub context: String,
}

pub fn anchors(html: &str) -> Vec<Anchor> {
    let mut out = Vec::new();
    for c in re_anchor().captures_iter(html) {
        let href = decode_entities(c.get(1).map(|m| m.as_str()).unwrap_or(""));
        let text = to_text(c.get(2).map(|m| m.as_str()).unwrap_or(""));
        let start = c.get(0).map(|m| m.start()).unwrap_or(0);
        let ctx_start = floor_char_boundary(html, start.saturating_sub(120));
        let ctx = to_text(&html[ctx_start..start]);
        out.push(Anchor {
            href: href.trim().to_string(),
            text,
            context: ctx,
        });
    }
    out
}

const BODY_START: [&str; 8] = [
    "entry-content",
    "single-content",
    "article-content",
    "post-content",
    "content-article",
    "class=\"entry\"",
    "content-wrap",
    "<article",
];

const BODY_END: [&str; 10] = [
    "post-related",
    "related-posts",
    "comments-area",
    "comment-area",
    "id=\"comments\"",
    "secret-content-box",
    "article-footer-row",
    "comment-content",
    "class=\"sidebar",
    "<footer",
];

/// Extract the readable article slab (main content only, related/comment noise cut).
pub fn article_html(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let marker = BODY_START
        .iter()
        .filter_map(|m| lower.find(m))
        .min()
        .unwrap_or(0);
    // `find` lands inside the class attribute; skip to the end of that open tag so
    // the extracted text does not start with `content-wrap">`.
    let start = match lower[marker..].find('>') {
        Some(off) => marker + off + 1,
        None => marker,
    };
    let rest_lower = &lower[start..];
    let end_rel = BODY_END
        .iter()
        .filter_map(|m| rest_lower.find(m))
        .min()
        .unwrap_or(rest_lower.len());
    let end = (start + end_rel).min(html.len());
    let start = floor_char_boundary(html, start);
    let end = floor_char_boundary(html, end.max(start));
    html[start..end].to_string()
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn article_text(html: &str, max: usize) -> String {
    let body = article_html(html);
    let mut text = to_text(&body);
    if text.chars().count() > max {
        text = text.chars().take(max).collect::<String>();
        text.push_str("\n…");
    }
    text
}

/// True when the page looks like a search-results listing rather than an article.
pub fn looks_like_search(html: &str) -> bool {
    let t = re_title()
        .captures(html)
        .map(|c| to_text(&c[1]))
        .unwrap_or_default();
    t.contains("搜索结果") || t.contains("搜索") || t.to_lowercase().contains("search result")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_entities() {
        let h = "<div>7-Zip&nbsp;26.04<br>更新：修复 &amp; 优化</div><script>x=1</script>";
        assert_eq!(to_text(h), "7-Zip 26.04\n更新：修复 & 优化");
    }

    #[test]
    fn anchors_work() {
        let h = r#"<p>下载：<a href="/go.php?url=abc" target="_blank">本地下载</a></p>"#;
        let a = anchors(h);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].href, "/go.php?url=abc");
        assert_eq!(a[0].text, "本地下载");
    }

    #[test]
    fn slice_body() {
        let h = "<nav>menu</nav><div class='entry-content'><p>hello</p></div><div class='post-related'>noise</div>";
        let body = article_text(h, 100);
        assert!(body.contains("hello"));
        assert!(!body.contains("noise"));
    }
}
