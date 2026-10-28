//! Small helpers: time/size formatting, version guessing/comparison, fuzzy title
//! matching, a tiny base64 decoder, ids.

use std::sync::OnceLock;

use regex::Regex;

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n == 0 {
        return "0 B".into();
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else if v >= 100.0 {
        format!("{:.0} {}", v, UNITS[i])
    } else {
        format!("{:.1} {}", v, UNITS[i])
    }
}

pub fn fmt_speed(bps: f64) -> String {
    if bps <= 0.5 {
        "—".into()
    } else {
        format!("{}/s", fmt_bytes(bps as u64))
    }
}

pub fn fmt_secs_ago(ts: i64) -> String {
    use crate::i18n::t;
    if ts <= 0 {
        return t("从未", "never").into();
    }
    let d = now_secs() - ts;
    match d {
        d if d < 0 => t("刚刚", "just now").into(),
        d if d < 60 => t("刚刚", "just now").into(),
        d if d < 3600 => crate::i18n::t1("{} 分钟前", "{} min ago", d / 60),
        d if d < 86400 => crate::i18n::t1("{} 小时前", "{} h ago", d / 3600),
        d if d < 86400 * 30 => crate::i18n::t1("{} 天前", "{} d ago", d / 86400),
        _ => fmt_date(ts),
    }
}

pub fn fmt_date(ts: i64) -> String {
    // Civil date from unix seconds (UTC) — good enough for "checked at" labels.
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[allow(dead_code)]
pub fn fmt_eta(remaining: u64, bps: f64) -> String {
    if bps <= 1.0 {
        return "—".into();
    }
    let s = (remaining as f64 / bps).round() as u64;
    if s < 60 {
        format!("{} 秒", s)
    } else if s < 3600 {
        format!("{} 分 {} 秒", s / 60, s % 60)
    } else {
        format!("{} 时 {} 分", s / 3600, (s % 3600) / 60)
    }
}

/// URL-safe-ish slug for ids. Keeps CJK (filesystem/JSON friendly).
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else if c.is_whitespace() || c == '.' || c == '/' {
            if !out.ends_with('-') {
                out.push('-');
            }
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.chars().count() > 40 {
        out.chars().take(40).collect()
    } else if out.is_empty() {
        "item".into()
    } else {
        out
    }
}

/// Stable 32-bit FNV-1a hash, rendered as hex — enough to disambiguate ids.
pub fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{:08x}", h)
}

// ---------------------------------------------------------------------------
// Version guessing
// ---------------------------------------------------------------------------

fn win_build_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)(\d{2})\s*H\s*(\d)[^\d]{0,24}(\d{5}\.\d{3,6})").unwrap()
    })
}

fn version_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"(?i)(?:^|[^0-9A-Za-z._])(?:v(?:ersion)?\s*)?(\d+(?:\.\d+){1,3})(?:\s*[-_ ]?\s*(alpha|beta|rc|preview|dev|nightly)\s*\.?\s*(\d+)?)?",
        )
        .unwrap()
    })
}

/// Detect the most plausible version string in a title (fallback: page body).
///
/// Windows image titles get the `26H2 · 26300.9550` treatment because both halves
/// matter when you are picking an ISO.
pub fn guess_version(title: &str, body: &str) -> String {
    if let Some(c) = win_build_re().captures(title) {
        return format!("{}H{} · {}", &c[1], &c[2], &c[3]);
    }
    if let Some(v) = best_version(title) {
        return v;
    }
    if let Some(v) = best_version(&body.chars().take(600).collect::<String>()) {
        return v;
    }
    if let Some(c) = win_build_re().captures(body) {
        return format!("{}H{} · {}", &c[1], &c[2], &c[3]);
    }
    "未知".into()
}

fn best_version(text: &str) -> Option<String> {
    let mut best: Option<(i32, String)> = None;
    for c in version_re().captures_iter(text) {
        let core = c.get(1).map(|m| m.as_str()).unwrap_or("");
        // `font-size: 1.1em` is CSS, not a version. Inline styles leak into titles
        // when a page ships highlight markup as text, and without this guard the
        // guesser happily reports "1.1" for a page that is about "8.53".
        if let Some(m) = c.get(0) {
            if followed_by_css_unit(&text[m.end()..]) {
                continue;
            }
        }
        let parts: Vec<&str> = core.split('.').collect();
        // Reject dates such as 2026.10 or IP fragments such as 192.168.1.1.
        let nums: Vec<u64> = parts.iter().filter_map(|p| p.parse().ok()).collect();
        if nums.len() < 2 {
            continue;
        }
        if parts.len() == 2 && nums[0] >= 1990 && nums[0] <= 2100 && nums[1] <= 12 {
            continue; // year.month
        }
        if parts.len() == 4 && nums.iter().all(|n| *n <= 255) && nums[0] >= 127 {
            continue; // IP address
        }
        let start = c.get(0).map(|m| m.start()).unwrap_or(9999);
        let has_v = c
            .get(0)
            .map(|m| m.as_str().to_ascii_lowercase().contains('v'))
            .unwrap_or(false);
        let suffix = c.get(2).map(|m| m.as_str().to_ascii_lowercase());
        let mut score = (parts.len() as i32) * 10 + if has_v { 12 } else { 0 };
        score += if start < 40 { 6 } else if start < 90 { 3 } else { 0 };
        // Bigger leading number wins when everything else is equal (26.04 > 19.00).
        score += (nums[0].min(99) / 10) as i32;
        if suffix.is_some() {
            score -= 4;
        }
        // Penalise things like "x64", "19045.7807" is fine (5 digits) — keep.
        let mut s = core.to_string();
        if let Some(suf) = suffix {
            s.push_str("-");
            s.push_str(&suf);
            if let Some(n) = c.get(3) {
                s.push('.');
                s.push_str(n.as_str());
            }
        }
        if best.as_ref().map(|(b, _)| score > *b).unwrap_or(true) {
            best = Some((score, s));
        }
    }
    best.map(|(_, v)| v)
}

/// True when `rest` continues with a CSS length unit (`em`, `px`, `%` …), i.e. the
/// number it follows is a style value rather than a version.
fn followed_by_css_unit(rest: &str) -> bool {
    const UNITS: [&str; 8] = ["em", "rem", "px", "pt", "vh", "vw", "ex", "%"];
    let rest = rest.trim_start();
    let bytes = rest.as_bytes();
    UNITS.iter().any(|u| {
        let unit = u.as_bytes();
        // Compare on bytes: slicing a str at a fixed offset panics inside CJK text.
        bytes.len() >= unit.len()
            && bytes[..unit.len()].eq_ignore_ascii_case(unit)
            && rest[unit.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !ch.is_alphanumeric())
    })
}

/// Split a version-ish string into comparable numeric chunks.
#[allow(dead_code)]
pub fn version_key(v: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in v.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(cur.parse().unwrap_or(0));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        out.push(cur.parse().unwrap_or(0));
    }
    out.truncate(8);
    out
}

/// `Some(Ordering)` when both parse into comparable numbers.
#[allow(dead_code)]
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let ka = version_key(a);
    let kb = version_key(b);
    for i in 0..ka.len().max(kb.len()) {
        let x = ka.get(i).copied().unwrap_or(0);
        let y = kb.get(i).copied().unwrap_or(0);
        if x != y {
            return x.cmp(&y);
        }
    }
    a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Fuzzy name matching for site search results
// ---------------------------------------------------------------------------

pub fn norm(s: &str) -> String {
    // Small synonym table so "微信电脑版" still matches "微信PC版(WeChat)".
    let s = s
        .replace("电脑版", "pc版")
        .replace("电脑", "pc")
        .replace("客户端", "client")
        .replace("苹果版", "mac版");
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// ASCII words (len >= 2) plus CJK bigrams. `Windows11 26H2` → ["windows11","26h2"].
pub fn tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();

    let flush_ascii = |buf: &mut String, out: &mut Vec<String>| {
        if buf.len() >= 2 {
            out.push(buf.to_lowercase());
        }
        buf.clear();
    };
    let flush_cjk = |buf: &mut Vec<char>, out: &mut Vec<String>| {
        if buf.len() == 1 {
            out.push(buf[0].to_string());
        } else {
            for w in buf.windows(2) {
                out.push(w.iter().collect());
            }
        }
        buf.clear();
    };

    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            if !cjk.is_empty() {
                flush_cjk(&mut cjk, &mut out);
            }
            ascii.push(c);
        } else if c.is_alphanumeric() {
            if !ascii.is_empty() {
                flush_ascii(&mut ascii, &mut out);
            }
            cjk.push(c);
        } else {
            if !ascii.is_empty() {
                flush_ascii(&mut ascii, &mut out);
            }
            if !cjk.is_empty() {
                flush_cjk(&mut cjk, &mut out);
            }
        }
    }
    if !ascii.is_empty() {
        flush_ascii(&mut ascii, &mut out);
    }
    if !cjk.is_empty() {
        flush_cjk(&mut cjk, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

/// 0.0 – 1.0 relevance of a page title for a user query.
pub fn score_title(title: &str, query: &str) -> f64 {
    let nt = norm(title);
    let nq = norm(query);
    if nq.is_empty() || nt.is_empty() {
        return 0.0;
    }
    if nt == nq {
        return 1.0;
    }
    let mut score: f64 = 0.0;
    if nt.contains(&nq) {
        score += 0.72;
        // Prefer titles that start with the product name over long suffix-packed ones.
        if nt.starts_with(&nq) {
            score += 0.08;
        }
    } else {
        let qt = tokens(query);
        if qt.is_empty() {
            return 0.0;
        }
        let tt = tokens(title);
        let hit = qt.iter().filter(|q| tt.iter().any(|t| t == *q)).count();
        score += 0.62 * (hit as f64 / qt.len() as f64);
        let nq_chars: Vec<char> = nq.chars().collect();
        let nt_chars: Vec<char> = nt.chars().collect();
        let common = nq_chars.iter().filter(|c| nt_chars.contains(c)).count();
        score += 0.18 * (common as f64 / nq_chars.len() as f64);
    }
    // Penalties for pre-release / legacy builds unless the user asked for them.
    let lower = title.to_lowercase();
    let qlower = query.to_lowercase();
    for (bad, penalty) in [
        ("beta", 0.10),
        ("测试版", 0.10),
        ("预览", 0.06),
        ("旧版", 0.18),
        ("怀旧", 0.15),
        ("考古", 0.15),
        // Companion patches / plugins share the product name but are not the product.
        ("补丁", 0.16),
        ("插件", 0.12),
        ("扩展", 0.10),
        ("注册机", 0.18),
        ("激活工具", 0.12),
        ("patch", 0.12),
    ] {
        if lower.contains(bad) && !qlower.contains(bad) {
            score -= penalty;
        }
    }
    score.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// base64 (423down wraps mirror links as /go.php?url=<base64>)
// ---------------------------------------------------------------------------

pub fn decode_base64(s: &str) -> Option<String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'=')
        .collect();
    if cleaned.len() < 4 {
        return None;
    }
    let mut out = Vec::with_capacity(cleaned.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u8;
    for c in cleaned {
        let v = val(c)?;
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    String::from_utf8(out).ok()
}

/// Strip query/fragment, normalise trailing slash.
pub fn normalize_url(u: &str) -> String {
    let u = u.trim();
    let u = u.trim_end_matches(['.', ',', ';', ')', '）', '"', '\'']);
    if u.starts_with("http://") || u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{}", u.trim_start_matches('/'))
    }
}

/// Case-insensitive substring test that stays allocation-free when `hay` has no
/// ASCII uppercase letters — true for CJK names and for URLs, i.e. almost every
/// call the sidebar makes while the user types a filter.
pub fn icontains(hay: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if !hay.bytes().any(|b| b.is_ascii_uppercase()) {
        return hay.contains(needle_lower);
    }
    hay.to_lowercase().contains(needle_lower)
}

pub fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_lowercase()
}

pub fn is_github_host(host: &str) -> bool {
    host == "github.com" || host == "www.github.com"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(best_version("v0.2.113").as_deref(), Some("0.2.113"));
        assert_eq!(
            best_version("7-Zip解压软件 7-Zip 26.04 Final 中文优化版").as_deref(),
            Some("26.04")
        );
        assert_eq!(
            best_version("微信PC版(WeChat) v4.1.15.53 多开防撤回版").as_deref(),
            Some("4.1.15.53")
        );
        assert_eq!(
            best_version("v2.0.0-preview.7").as_deref(),
            Some("2.0.0-preview.7")
        );
        assert_eq!(
            guess_version("【不忘初心游戏版】Windows11 26H2（26300.9550）X64 无更新[精简版]", ""),
            "26H2 · 26300.9550"
        );
        assert!(best_version("Windows11 26H2 不忘初心游戏版").is_none());
    }

    #[test]
    fn ignores_css_units() {
        // Verbatim shape of a masuit.org search card: the highlight markup is stored
        // escaped, so it comes back as real HTML in the title.
        let dirty = "硬件检测工具——<span style='color:red;background-color:yellow;font-size: 1.1em;font-weight:700;'>HWiNFO</span> 8.53中文版";
        assert_eq!(best_version(dirty).as_deref(), Some("8.53"));
        assert!(best_version("font-size: 1.1em").is_none());
    }

    #[test]
    fn matching() {
        let s = score_title("7-Zip解压软件 7-Zip 26.04 Final 中文优化版", "7-Zip");
        let s2 = score_title("Cinema 4D v2027.0.0 C4D2027 中文激活版", "7-Zip");
        assert!(s > 0.7, "score {s}");
        assert!(s2 < 0.2, "score {s2}");
        let g = score_title("【不忘初心游戏版】Windows11 26H2（26300.9550）X64 无更新", "Windows11 26H2 不忘初心游戏版");
        assert!(g > 0.5, "score {g}");
    }

    #[test]
    fn base64() {
        assert_eq!(
            decode_base64("aHR0cHM6Ly9wYW4ucXVhcmsuY24vcy85YzM2MWZlMmIwY2M=").as_deref(),
            Some("https://pan.quark.cn/s/9c361fe2b0cc")
        );
    }

    #[test]
    fn cmp() {
        assert_eq!(
            compare_versions("26.04", "24.04"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("0.2.113", "0.2.112"),
            std::cmp::Ordering::Greater
        );
    }

    #[test]
    fn icontains_ignores_case() {
        assert!(icontains("HWiNFO", "hwinfo"));
        assert!(icontains("7-Zip 26.04", "7-zip"));
        assert!(icontains("微信电脑版", "微信"));
        assert!(icontains("anything", ""));
        assert!(!icontains("7-Zip", "hwinfo"));
    }
}
