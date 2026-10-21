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

#[derive(Debug)]
pub struct Fetched {
    pub final_url: String,
    #[allow(dead_code)]
    pub status: u16,
    pub body: String,
}

/// 要不要跟这个跳转。
///
/// 抽成独立函数有两个原因：跳转策略是个闭包，单独没法测；而这两条规则的边界
/// （纯 http 站点、http → https 升级）恰恰是最容易写错、又最容易让真实下载失效的地方。
///
/// 1. 链长封顶 12 跳，挡死循环跳转。
/// 2. **拒绝 https → http 降级**：HTTPS 的意义就是端到端不可篡改，一旦跳回明文，
///    链路上的任意一跳（公共 WiFi、运营商注入、透明代理）都能把安装包换成别的，
///    而这个应用下载完是准备让人双击运行的。GitHub 之外的附件基本没有 SHA-256
///    可比，事后也发现不了。
///
/// 注意它**不**拦「本来就是 http 的链接」——那是站点的既有事实，直接拦掉会让
/// 大批只提供 http 的站点整个失效；那种情况由下载列表里的「明文 http」红标提示。
fn redirect_verdict(next: &reqwest::Url, previous: &[reqwest::Url]) -> Result<(), &'static str> {
    if previous.len() >= 12 {
        return Err("重定向次数过多");
    }
    if next.scheme() == "http" && previous.iter().any(|u| u.scheme() == "https") {
        return Err("拒绝 https → http 降级跳转");
    }
    Ok(())
}

impl Http {
    pub fn new(proxy: &str, token: Option<String>) -> Result<Self, String> {        let mut b = Client::builder()
            .user_agent(UA)
            // Some fronted sites (Cloudflare with a slow edge) take 40s+ to
            // first byte; the request-level timeout below is the real guard.
            .timeout(Duration::from_secs(55))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                match redirect_verdict(attempt.url(), attempt.previous()) {
                    Ok(()) => attempt.follow(),
                    Err(reason) => attempt.error(reason),
                }
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
        // reqwest 的 `Kind::Redirect` 是个无字段的变体，自定义原因（`attempt.error("...")`）
        // 只存在 `source()` 里，Display 永远只有一句 "error following redirect"。
        // 不往下挖这一层的话，"拒绝 https → http 降级跳转" 会原样显示成"重定向次数过多"，
        // 用户会以为在重试就行，其实再试一百次也会被拒。
        let reason = std::error::Error::source(&e)
            .map(|s| s.to_string())
            .unwrap_or_default();
        if reason.is_empty() || reason.contains("too many redirects") {
            "重定向次数过多".into()
        } else {
            reason
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn urls(list: &[&str]) -> Vec<reqwest::Url> {
        list.iter().map(|u| reqwest::Url::parse(u).unwrap()).collect()
    }

    #[test]
    fn downgrade_to_plain_http_is_refused() {
        let prev = urls(&["https://example.com/dl/app.exe"]);
        let next = reqwest::Url::parse("http://example.com/dl/app.exe").unwrap();
        assert_eq!(redirect_verdict(&next, &prev), Err("拒绝 https → http 降级跳转"));
    }

    #[test]
    fn upgrade_and_same_scheme_are_allowed() {
        // http → https (7-zip.org 的真实行为) 必须放行。
        let prev = urls(&["http://www.7-zip.org/a/7z2604.exe"]);
        let next = reqwest::Url::parse("https://www.7-zip.org/a/7z2604.exe").unwrap();
        assert_eq!(redirect_verdict(&next, &prev), Ok(()));

        // 一直是明文的站点不该被这条规则挡死。
        let prev = urls(&["http://dl.example.com/a.zip"]);
        let next = reqwest::Url::parse("http://cdn.example.com/a.zip").unwrap();
        assert_eq!(redirect_verdict(&next, &prev), Ok(()));

        // https → https 的不同主机（GitHub 资产跳转）必须放行。
        let prev = urls(&["https://github.com/x/y/releases/download/v1/a.exe"]);
        let next = reqwest::Url::parse("https://release-assets.githubusercontent.com/a.exe").unwrap();
        assert_eq!(redirect_verdict(&next, &prev), Ok(()));

        // 首个请求本身就是 http：previous 里的初始 URL 是 http，不算降级。
        let prev = urls(&["http://example.com/a.exe"]);
        let next = reqwest::Url::parse("http://example.com/b.exe").unwrap();
        assert_eq!(redirect_verdict(&next, &prev), Ok(()));
    }

    #[test]
    fn redirect_chain_is_capped() {
        let prev: Vec<reqwest::Url> = (0..12)
            .map(|i| reqwest::Url::parse(&format!("https://example.com/{i}")).unwrap())
            .collect();
        let next = reqwest::Url::parse("https://example.com/next").unwrap();
        assert_eq!(redirect_verdict(&next, &prev), Err("重定向次数过多"));
    }

    /// 端到端：真正的 301 死循环要能浮出"重定向次数过多"，而不是 reqwest 的英文原文。
    #[tokio::test]
    async fn redirect_loop_reports_readable_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 301 Moved Permanently\r\nLocation: http://127.0.0.1:{port}/loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });

        let http = Http::new("", None).unwrap();
        let err = http
            .get_text(&format!("http://127.0.0.1:{port}/loop"))
            .await
            .unwrap_err();
        assert!(err.contains("重定向次数过多"), "实际错误: {err}");
    }
}
