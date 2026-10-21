//! HTTP layer: one client, a tiny per-host cookie jar, HTML/JSON helpers.
//!
//! The cookie jar exists for sites like pc528.net which answer the first request
//! with a `403` + `Set-Cookie` challenge and let you through on the retry. reqwest's
//! `cookie_store` feature would pull another dependency tree for ~40 lines of code,
//! so we keep it local.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, COOKIE};
use reqwest::{Client, StatusCode};

pub const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                      (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Pages larger than this are almost certainly not a download article; refusing
/// them keeps a hostile or broken response from ballooning memory (the parser then
/// makes several more full-size copies of the body).
const MAX_TEXT_BODY: usize = 4 * 1024 * 1024;
/// Diagnostic-only reads (error pages, JSON) need far less.
const MAX_AUX_BODY: usize = 64 * 1024;
const MAX_JSON_BODY: usize = 2 * 1024 * 1024;
/// Back-off ceiling for 429 responses, so a hostile `Retry-After: 86400` cannot
/// stall the resolve queue for a day.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);

pub struct Http {
    pub client: Client,
    /// host → (cookie name, value). Stored per name so a repeated `Set-Cookie`
    /// replaces the old value instead of piling up duplicate pairs.
    cookies: Mutex<HashMap<String, Vec<(String, String)>>>,
    pub token: Option<String>,
}

pub struct Fetched {
    pub final_url: String,
    #[allow(dead_code)]
    pub status: u16,
    pub body: String,
}

impl Http {
    pub fn new(proxy: &str, token: Option<String>) -> Result<Self, String> {
        let mut b = Client::builder()
            .user_agent(UA)
            // Some fronted sites (Cloudflare with a slow edge) take 40s+ to
            // first byte; the request-level timeout below is the real guard.
            .timeout(Duration::from_secs(55))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 12 {
                    return attempt.error("重定向次数过多");
                }
                // Refuse https → http: anyone on the path could then swap the
                // bytes, and non-GitHub downloads carry no digest to catch it.
                if attempt.url().scheme() == "http"
                    && attempt.previous().iter().any(|u| u.scheme() == "https")
                {
                    return attempt.error("拒绝 https → http 降级跳转");
                }
                attempt.follow()
            }));
        if !proxy.trim().is_empty() {
            b = b.proxy(reqwest::Proxy::all(proxy.trim()).map_err(|e| format!("代理地址无效: {e}"))?);
        }
        let client = b.build().map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;
        Ok(Self {
            client,
            cookies: Mutex::new(HashMap::new()),
            token,
        })
    }

    fn cookie_for(&self, host: &str) -> Option<String> {
        let jar = self.cookies.lock().ok()?;
        let pairs = jar.get(host)?;
        if pairs.is_empty() {
            return None;
        }
        Some(
            pairs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Cookies gathered for the host of `url` — download requests reuse them so
    /// that links behind an anti-bot cookie challenge keep working.
    pub fn cookies_for(&self, url: &str) -> Option<String> {
        self.cookie_for(&crate::util::host_of(url))
    }

    /// Store the cookies handed out by `resp` under the host that actually served
    /// it (`resp.url()`), not under the host we asked for: after a cross-host
    /// redirect the two differ, and keying by the request host sent one site's
    /// cookies to another.
    fn store_cookies(&self, resp: &reqwest::Response) {
        let host = crate::util::host_of(resp.url().as_str());
        if host.is_empty() {
            return;
        }
        let mut jar = match self.cookies.lock() {
            Ok(j) => j,
            Err(_) => return,
        };
        let entry = jar.entry(host).or_default();
        for hv in resp.headers().get_all(reqwest::header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            let Some(pair) = s.split(';').next() else {
                continue;
            };
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let (name, value) = (name.trim(), value.trim());
            if name.is_empty() {
                continue;
            }
            match entry.iter_mut().find(|(k, _)| k == name) {
                Some(slot) => slot.1 = value.to_string(),
                None => entry.push((name.to_string(), value.to_string())),
            }
        }
    }

    /// GET a text resource, retrying once when a site answers with an anti-bot 403
    /// (the retry carries whatever cookies the first response handed out).
    pub async fn get_text(&self, url: &str) -> Result<Fetched, String> {
        self.get_text_opts(url, None).await
    }

    pub async fn get_text_opts(&self, url: &str, accept: Option<&str>) -> Result<Fetched, String> {
        let host = crate::util::host_of(url);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self
                .client
                .get(url)
                .header(ACCEPT, accept.unwrap_or("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"))
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8");
            if let Some(c) = self.cookie_for(&host) {
                req = req.header(COOKIE, c);
            }
            if let Some(t) = &self.token {
                // Only the API accepts bearer tokens; github.com's HTML endpoints
                // reject them, so keep the token off normal page fetches.
                if host == "api.github.com" {
                    req = req.header("Authorization", format!("Bearer {t}"));
                }
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    // Cloudflare-fronted sites stall the first connection or two;
                    // a couple of short back-offs clear most of those.
                    if e.is_timeout() && attempt < 2 {
                        let wait = 600u64 * attempt as u64;
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        continue;
                    }
                    return Err(describe_reqwest(e));
                }
            };
            let status = resp.status();
            self.store_cookies(&resp);
            if status == StatusCode::FORBIDDEN && attempt <= 1 {
                tokio::time::sleep(Duration::from_millis(350)).await;
                continue;
            }
            // 429 used to surface straight to the user; honour Retry-After once.
            if status == StatusCode::TOO_MANY_REQUESTS && attempt <= 2 {
                let wait = retry_after(&resp).unwrap_or(Duration::from_secs(2));
                tokio::time::sleep(wait).await;
                continue;
            }
            let final_url = resp.url().to_string();
            if !status.is_success() {
                let cf_hint = resp
                    .headers()
                    .get("cf-mitigated")
                    .map(|v| v.to_str().unwrap_or("").contains("challenge"))
                    .unwrap_or(false);
                let body = read_body(resp, MAX_AUX_BODY).await.unwrap_or_default();
                let challenge = cf_hint
                    || body.contains("Just a moment")
                    || body.contains("challenge-platform")
                    || body.contains("cf-challenge");
                return Err(match status.as_u16() {
                    403 if challenge => {
                        "该站点开启了 Cloudflare 人机验证，程序无法直接抓取；请在浏览器打开后用「添加追踪」粘贴具体文章链接".to_string()
                    }
                    403 => "站点拒绝了请求（403），可能触发了访问保护".to_string(),
                    404 => "页面不存在（404）".to_string(),
                    429 => "请求过于频繁（429），请稍后重试".to_string(),
                    s => format!("HTTP {s}"),
                });
            }
            let body = read_body(resp, MAX_TEXT_BODY).await?;
            return Ok(Fetched {
                final_url,
                status: status.as_u16(),
                body,
            });
        }
    }

    pub async fn get_json(&self, url: &str) -> Result<String, String> {
        // The bearer token is attached unconditionally below; make sure the URL can
        // only ever be the GitHub API so a future caller cannot leak it to a mirror.
        if crate::util::host_of(url) != "api.github.com" {
            return Err("内部错误：拒绝向非 GitHub API 主机发送凭证".into());
        }
        let mut req = self
            .client
            .get(url)
            .header(ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req.send().await.map_err(describe_reqwest)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                403 | 429 => "GitHub API 限流（可在设置中填写 Token 提高额度）".to_string(),
                404 => "GitHub API 404".to_string(),
                s => format!("GitHub API HTTP {s}"),
            });
        }
        read_body(resp, MAX_JSON_BODY).await
    }

    /// Query the size of a remote file without downloading it (best effort).
    #[allow(dead_code)]
    pub async fn probe_size(&self, url: &str) -> Option<u64> {
        let req = self.client.head(url).header(ACCEPT, "*/*");
        let resp = req.send().await.ok()?;
        resp.content_length()
    }
}

/// Read a body with a hard cap.
///
/// `Response::text()` buffers whatever the server sends; a huge or hostile page
/// would then be copied several more times by the parser (lowercasing, entity
/// decoding, char collection), so the cap belongs here rather than at the parser.
async fn read_body(mut resp: reqwest::Response, max: usize) -> Result<String, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > max {
                    return Err(format!(
                        "响应超过 {}，已中止读取",
                        crate::util::fmt_bytes(max as u64)
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(describe_reqwest(e)),
        }
    }
    // Same behaviour as `Response::text()` without the `charset` feature: UTF-8
    // with replacement characters.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// `Retry-After` in seconds, clamped so a server cannot park the resolve queue.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_WAIT))
}

pub fn describe_reqwest(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "请求超时".into()
    } else if e.is_connect() {
        "连接失败（检查网络或代理设置）".into()
    } else if e.is_redirect() {
        "重定向次数过多".into()
    } else if e.is_decode() {
        "响应解码失败".into()
    } else {
        format!("请求失败: {e}")
    }
}

/// Range header helper used by the downloader.
pub fn range_header(from: u64) -> String {
    format!("bytes={}-", from)
}
