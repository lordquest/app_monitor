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

pub struct Http {
    pub client: Client,
    cookies: Mutex<HashMap<String, String>>,
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
            .redirect(reqwest::redirect::Policy::limited(12));
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
        self.cookies.lock().ok()?.get(host).cloned()
    }

    /// Cookies gathered for the host of `url` — download requests reuse them so
    /// that links behind an anti-bot cookie challenge keep working.
    pub fn cookies_for(&self, url: &str) -> Option<String> {
        self.cookie_for(&crate::util::host_of(url))
    }

    fn store_cookies(&self, host: &str, resp: &reqwest::Response) {
        let mut jar = match self.cookies.lock() {
            Ok(j) => j,
            Err(_) => return,
        };
        let entry = jar.entry(host.to_string()).or_default();
        for hv in resp.headers().get_all(reqwest::header::SET_COOKIE) {
            if let Ok(s) = hv.to_str() {
                if let Some(pair) = s.split(';').next() {
                    let pair = pair.trim();
                    if pair.contains('=') {
                        if !entry.is_empty() {
                            entry.push_str("; ");
                        }
                        entry.push_str(pair);
                    }
                }
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
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
                .header("Accept-Encoding", "identity");
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
            self.store_cookies(&host, &resp);
            if status == StatusCode::FORBIDDEN && attempt == 1 {
                tokio::time::sleep(Duration::from_millis(350)).await;
                continue;
            }
            let final_url = resp.url().to_string();
            if !status.is_success() {
                let cf_hint = resp
                    .headers()
                    .get("cf-mitigated")
                    .map(|v| v.to_str().unwrap_or("").contains("challenge"))
                    .unwrap_or(false);
                let body = resp.text().await.unwrap_or_default();
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
            let body = resp.text().await.map_err(|e| describe_reqwest(e))?;
            return Ok(Fetched {
                final_url,
                status: status.as_u16(),
                body,
            });
        }
    }

    pub async fn get_json(&self, url: &str) -> Result<String, String> {
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
        resp.text().await.map_err(describe_reqwest)
    }

    /// Query the size of a remote file without downloading it (best effort).
    #[allow(dead_code)]
    pub async fn probe_size(&self, url: &str) -> Option<u64> {
        let req = self.client.head(url).header(ACCEPT, "*/*");
        let resp = req.send().await.ok()?;
        resp.content_length()
    }
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
