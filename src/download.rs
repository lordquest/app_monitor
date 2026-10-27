//! Resumable HTTP downloader.
//!
//! Design notes:
//! * bytes go to `<name>.part`; the file is renamed only after it is complete and
//!   (when GitHub published a digest) SHA-256 verified;
//! * a `.part` left behind by a cancelled/paused task is resumed with `Range`
//!   on the next run, which is also how "暂停 → 继续" works;
//! * progress is pushed to the UI at most ~7 times per second.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::http::{self, Http};

#[derive(Clone, Debug)]
pub struct DlSpec {
    pub id: String,
    #[allow(dead_code)]
    pub track_id: String,
    pub app: String,
    pub version: String,
    pub file: String,
    pub url: String,
    pub dir: PathBuf,
    pub sha256: Option<String>,
    /// Page the link was found on. Download sites routinely reject a request
    /// without a matching `Referer`, so it travels with the spec.
    pub referer: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    pub speed: f64,
}

#[derive(Debug)]
pub enum Outcome {
    Done {
        path: String,
        size: u64,
        sha_ok: Option<bool>,
        sha_actual: Option<String>,
    },
    Canceled {
        paused: bool,
        downloaded: u64,
        total: u64,
    },
    Failed {
        error: String,
        downloaded: u64,
        total: u64,
    },
}

pub async fn run<F>(
    http: &Http,
    spec: &DlSpec,
    cancel: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    mut on_progress: F,
) -> Outcome
where
    F: FnMut(Progress),
{
    if let Err(e) = tokio::fs::create_dir_all(&spec.dir).await {
        return Outcome::Failed {
            error: format!("无法创建下载目录: {e}"),
            downloaded: 0,
            total: 0,
        };
    }
    let target = match claim_target(&spec.dir, &spec.file).await {
        Ok(t) => t,
        Err(e) => {
            return Outcome::Failed {
                error: format!("无法创建下载文件: {e}"),
                downloaded: 0,
                total: 0,
            }
        }
    };
    // Keeps the `.part` reserved for this task until `run` returns; without it two
    // tasks resolving to the same filename truncated each other's file.
    let Target {
        final_path,
        part_path,
        fresh,
        guard: _guard,
    } = target;
    let mut fresh = fresh;
    // `fresh` means this task created the `.part` itself, so it starts empty.
    let mut have = if fresh.is_some() {
        0
    } else {
        tokio::fs::metadata(&part_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
    };

    let mut restarted = false;
    let mut attempts = 0u32;
    // 连续「一点点进展都没有」的次数。有进展就清零——否则用无限重试把真问题
    // （比如链接每次都 302 到错误页）藏起来。
    let mut stalls = 0u32;
    // 本次任务已到达的最大偏移，用来判断一轮重试到底有没有前进。
    let mut best = have;
    // 单个任务内的自动续传上限：超过就把控制权交回用户（可点「继续」）。
    const MAX_ATTEMPTS: u32 = 20;
    const MAX_STALLS: u32 = 3;
    loop {
        attempts += 1;
        let mut req = http
            .download_client()
            .get(&spec.url)
            .header(reqwest::header::ACCEPT, "*/*")
            // Byte-exact transfer: compression would make Content-Length and Range
            // bookkeeping ambiguous, and this is a binary file, not a page.
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .header(reqwest::header::USER_AGENT, http::UA);
        if let Some(referer) = &spec.referer {
            req = req.header(reqwest::header::REFERER, referer);
        }
        if let Some(cookie) = http.cookies_for(&spec.url) {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        if have > 0 {
            req = req.header(reqwest::header::RANGE, http::range_header(have));
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                let reason = http::describe_reqwest(e);
                // 连接层面的抖动（DNS 慢、对端 RST、代理握手失败）值得重试几次；
                // 连续几次都失败才会浮到界面上。
                if attempts < 4 {
                    let wait = (800 * attempts as u64).min(2_000);
                    log::warn!("下载请求失败（{reason}），{wait}ms 后重试");
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                    if cancel.load(Ordering::Relaxed) {
                        return Outcome::Canceled {
                            paused: paused.load(Ordering::Relaxed),
                            downloaded: have,
                            total: 0,
                        };
                    }
                    continue;
                }
                return Outcome::Failed {
                    error: reason,
                    downloaded: have,
                    total: 0,
                };
            }
        };
        let status = resp.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE && !restarted {
            // Stale/oversized .part — start over once.
            let _ = tokio::fs::remove_file(&part_path).await;
            have = 0;
            restarted = true;
            continue;
        }
        if !status.is_success() {
            return Outcome::Failed {
                error: match status.as_u16() {
                    403 => "下载被拒绝（403）：链接可能已过期、需要登录，或被代理/防火墙拦截".to_string(),
                    404 => "文件不存在（404）：该版本可能已被删除".to_string(),
                    429 => "站点限流（429），请稍后再试".to_string(),
                    s => format!("HTTP {s}"),
                },
                downloaded: have,
                total: 0,
            };
        }

        let (total, mut downloaded) = if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let total = resp
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(have + resp.content_length().unwrap_or(0));
            (total, have)
        } else {
            // Server ignored Range (or this is a fresh download) — restart the file.
            (resp.content_length().unwrap_or(0), 0u64)
        };

        // A `.part` this task created is already open and empty; a resumed one has to
        // be opened in append mode (or truncated when the server ignored `Range`).
        let mut file = match fresh.take() {
            Some(f) => f,
            None => {
                let mut opts = tokio::fs::OpenOptions::new();
                opts.create(true).write(true);
                if status == reqwest::StatusCode::PARTIAL_CONTENT {
                    opts.append(true);
                } else {
                    opts.truncate(true);
                }
                match opts.open(&part_path).await {
                    Ok(f) => f,
                    Err(e) => {
                        return Outcome::Failed {
                            error: format!("无法写入文件: {e}"),
                            downloaded,
                            total,
                        }
                    }
                }
            }
        };

        let mut stream = resp.bytes_stream();
        let mut last_emit = Instant::now();
        let mut win_start = Instant::now();
        let mut win_bytes = 0u64;
        let mut speed = 0f64;
        // 流中断（读超时 / 对端提前关闭）不再直接判定失败：记下原因，写盘之后
        // 带着 `Range` 自动接着下。174MB 的包在 2MB/s 下要 80 秒，中途抖一下
        // 就要求用户手点「继续」是不合理的。
        let mut stream_err: Option<String> = None;
        on_progress(Progress {
            downloaded,
            total,
            speed,
        });

        while let Some(chunk) = stream.next().await {
            if cancel.load(Ordering::Relaxed) {
                let _ = file.flush().await;
                return Outcome::Canceled {
                    paused: paused.load(Ordering::Relaxed),
                    downloaded,
                    total,
                };
            }
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => {
                    stream_err = Some(format!("传输中断: {}", http::describe_reqwest(e)));
                    break;
                }
            };
            if let Err(e) = file.write_all(&bytes).await {
                return Outcome::Failed {
                    error: format!("写入失败: {e}"),
                    downloaded,
                    total,
                };
            }
            downloaded += bytes.len() as u64;
            win_bytes += bytes.len() as u64;

            if win_start.elapsed() >= Duration::from_millis(500) {
                speed = win_bytes as f64 / win_start.elapsed().as_secs_f64();
                win_start = Instant::now();
                win_bytes = 0;
            }
            if last_emit.elapsed() >= Duration::from_millis(140) {
                last_emit = Instant::now();
                on_progress(Progress {
                    downloaded,
                    total,
                    speed,
                });
            }
        }

        // `flush` only hands the bytes to the OS; a crash or power loss right after
        // the rename below could leave a correctly-sized but empty file under the
        // final name, with no `.part` left to resume from.
        if let Err(e) = file.sync_all().await {
            return Outcome::Failed {
                error: format!("写入磁盘失败: {e}"),
                downloaded,
                total,
            };
        }
        drop(file);

        // ---- 要不要自动续传 ----
        let short = total > 0 && downloaded < total;
        let interrupted = stream_err.is_some() || short;
        if interrupted {
            let reason = match stream_err {
                Some(e) => e,
                None => format!(
                    "连接提前结束（{}/{}）",
                    crate::util::fmt_bytes(downloaded),
                    crate::util::fmt_bytes(total)
                ),
            };
            if downloaded > best {
                best = downloaded;
                stalls = 0;
            } else {
                stalls += 1;
            }
            if stalls < MAX_STALLS && attempts < MAX_ATTEMPTS {
                log::warn!(
                    "{reason}，自动续传（第 {attempts} 次，已到 {}）",
                    crate::util::fmt_bytes(downloaded)
                );
                // 下一次请求改成 `Range: bytes=<downloaded>-`。
                have = downloaded;
                // 短暂退避，别在站点刚 RST 时立刻又撞上去（封顶 2s，取消要能立刻生效）。
                tokio::time::sleep(Duration::from_millis(
                    (400 * attempts as u64).min(2_000),
                ))
                .await;
                if cancel.load(Ordering::Relaxed) {
                    return Outcome::Canceled {
                        paused: paused.load(Ordering::Relaxed),
                        downloaded,
                        total,
                    };
                }
                continue;
            }
            return Outcome::Failed {
                error: format!(
                    "{reason}；已下载 {}，可点击「继续」断点续传",
                    crate::util::fmt_bytes(downloaded)
                ),
                downloaded,
                total,
            };
        }

        on_progress(Progress {
            downloaded,
            total,
            speed,
        });

        // ---- verify ----
        let mut sha_ok = None;
        let mut sha_actual = None;
        if let Some(expected) = spec.sha256.as_ref().filter(|s| s.len() == 64) {
            let p = part_path.clone();
            // A digest we could not compute must never look like "verified": the old
            // code collapsed both into `None`, so the UI said "下载完成" either way.
            let actual = match tokio::task::spawn_blocking(move || sha256_file(&p)).await {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => {
                    return Outcome::Failed {
                        error: format!("SHA-256 校验失败：无法读取下载文件（{e}）"),
                        downloaded,
                        total,
                    }
                }
                Err(e) => {
                    return Outcome::Failed {
                        error: format!("SHA-256 校验任务异常：{e}"),
                        downloaded,
                        total,
                    }
                }
            };
            let ok = actual.eq_ignore_ascii_case(expected);
            sha_ok = Some(ok);
            sha_actual = Some(actual);
            if !ok {
                let _ = tokio::fs::remove_file(&part_path).await;
                return Outcome::Failed {
                    error: "SHA-256 校验失败，文件已删除（可能被截断或篡改）".into(),
                    downloaded,
                    total,
                };
            }
        }

        // ---- publish ----
        let final_path = match tokio::fs::rename(&part_path, &final_path).await {
            Ok(_) => final_path,
            Err(e) => {
                return Outcome::Failed {
                    error: format!("重命名失败: {e}"),
                    downloaded,
                    total,
                }
            }
        };
        return Outcome::Done {
            path: final_path.to_string_lossy().to_string(),
            size: downloaded,
            sha_ok,
            sha_actual,
        };
    }
}

/// A reserved download destination.
struct Target {
    final_path: PathBuf,
    part_path: PathBuf,
    /// `Some` when this task created the `.part` itself (empty, cursor at 0);
    /// `None` when an earlier partial download is being resumed.
    fresh: Option<tokio::fs::File>,
    /// Held for the lifetime of the task; drops the path claim on return.
    guard: PartGuard,
}

/// `.part` paths currently owned by a running task.
///
/// Two downloads that resolve to the same filename (the x64 and x86 builds of one
/// app, say) used to pick the same `.part` and truncate each other; the second one
/// now moves on to the next free name.
fn active_parts() -> &'static Mutex<HashSet<PathBuf>> {
    static ACTIVE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

struct PartGuard(PathBuf);

impl PartGuard {
    fn claim(path: &Path) -> Option<Self> {
        let mut set = active_parts().lock().ok()?;
        if set.insert(path.to_path_buf()) {
            Some(Self(path.to_path_buf()))
        } else {
            None
        }
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = active_parts().lock() {
            set.remove(&self.0);
        }
    }
}

/// Pick a free filename and reserve its `.part` atomically.
async fn claim_target(dir: &Path, file: &str) -> std::io::Result<Target> {
    let safe = sanitize(file);
    let (stem, ext) = split_ext(&safe);
    for i in 0..999u32 {
        let name = if i == 0 {
            safe.clone()
        } else {
            format!("{stem} ({i}){ext}")
        };
        let final_path = dir.join(&name);
        if final_path.exists() {
            continue;
        }
        let part_path = dir.join(format!("{name}.part"));
        let Some(guard) = PartGuard::claim(&part_path) else {
            continue;
        };
        match tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part_path)
            .await
        {
            Ok(f) => {
                return Ok(Target {
                    final_path,
                    part_path,
                    fresh: Some(f),
                    guard,
                })
            }
            // Left behind by a cancelled run → resume it.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Ok(Target {
                    final_path,
                    part_path,
                    fresh: None,
                    guard,
                })
            }
            Err(e) => return Err(e),
        }
    }
    // 999 collisions: fall back to a timestamped name.
    let name = format!("{stem}-{}{ext}", crate::util::now_secs());
    let final_path = dir.join(&name);
    let part_path = dir.join(format!("{name}.part"));
    let guard = PartGuard::claim(&part_path).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "同名下载正在进行中")
    })?;
    let fresh = match tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&part_path)
        .await
    {
        Ok(f) => Some(f),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
        Err(e) => return Err(e),
    };
    Ok(Target {
        final_path,
        part_path,
        fresh,
        guard,
    })
}

fn split_ext(name: &str) -> (String, String) {
    if let Some(pos) = name.rfind('.') {
        if pos > 0 && name.len() - pos <= 12 {
            return (name[..pos].to_string(), name[pos..].to_string());
        }
    }
    (name.to_string(), String::new())
}

pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        return "download.bin".into();
    }
    let cleaned = if cleaned.chars().count() > 180 {
        cleaned.chars().take(180).collect::<String>()
    } else {
        cleaned
    };
    // Windows device names are still special even with an extension: `NUL.exe`
    // opens the device, not a file. Prefix instead of dropping the name.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return format!("_{cleaned}");
    }
    cleaned
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize("a/b:c?.zip"), "a_b_c_.zip");
        assert_eq!(sanitize("   "), "download.bin");
    }

    #[test]
    fn escapes_windows_device_names() {
        assert_eq!(sanitize("NUL.exe"), "_NUL.exe");
        assert_eq!(sanitize("con"), "_con");
        // A name that merely starts with a device name is fine.
        assert_eq!(sanitize("console.zip"), "console.zip");
    }

    // ---- 大文件：断流自动续传 / 无进展及时放弃 ----

    /// 极简 HTTP 服务：`cut` 表示"这次只发这么多字节就断开"，`honor_range` 决定是否支持 Range。
    fn spawn_server(
        body: Vec<u8>,
        cut: Option<usize>,
        honor_range: bool,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let range = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|l| l.split('=').nth(1))
                    .and_then(|v| v.split('-').next())
                    .and_then(|v| v.trim().parse::<usize>().ok());
                let start = if honor_range { range.unwrap_or(0) } else { 0 };
                let rest = &body[start.min(body.len())..];
                let send = match cut {
                    Some(c) => &rest[..c.min(rest.len())],
                    None => rest,
                };
                let head = if start > 0 {
                    format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        start,
                        body.len().saturating_sub(1),
                        body.len(),
                        rest.len()
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        rest.len()
                    )
                };
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(send);
                let _ = s.flush();
                // 提前关闭连接：模拟对端中途断开 / 读超时。
            }
        });
        (format!("http://{addr}/big.bin"), handle)
    }

    fn spec_for(url: &str, dir: &Path) -> DlSpec {
        DlSpec {
            id: "t".into(),
            track_id: "t".into(),
            app: "big".into(),
            version: "1".into(),
            file: "big.bin".into(),
            url: url.to_string(),
            dir: dir.to_path_buf(),
            sha256: None,
            referer: None,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "app-monitor-{tag}-{}-{}",
            std::process::id(),
            crate::util::now_secs()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 第一次只给 40% 就断线，第二次带着 Range 拿下剩余部分 —— 中间不该报失败。
    #[tokio::test]
    async fn resumes_after_connection_drops() {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let cut_at = 120_000;
        let (url, _h) = spawn_server(body.clone(), Some(cut_at), true);
        let dir = tmpdir("resume");
        let http = Http::new("", None).unwrap();
        let spec = spec_for(&url, &dir);

        let outcome = run(
            &http,
            &spec,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |_p| {},
        )
        .await;

        match outcome {
            Outcome::Done { size, path, .. } => {
                assert_eq!(size, body.len() as u64, "续传后大小不对");
                let got = std::fs::read(&path).unwrap();
                assert_eq!(got, body, "续传拼出来的内容不对");
            }
            other => panic!("应当自动续传成功，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每次都一点不给就断线 → 三轮之后放弃，而不是无限重试。
    #[tokio::test]
    async fn gives_up_when_no_progress() {
        let body = vec![7u8; 200_000];
        let (url, _h) = spawn_server(body.clone(), Some(0), true);
        let dir = tmpdir("stall");
        let http = Http::new("", None).unwrap();
        let spec = spec_for(&url, &dir);

        let outcome = run(
            &http,
            &spec,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            |_p| {},
        )
        .await;

        match outcome {
            Outcome::Failed { error, .. } => {
                assert!(error.contains("可点击「继续」"), "错误缺少续传提示: {error}");
            }
            other => panic!("应当失败，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two downloads that resolve to the same filename must not share a `.part`.
    #[tokio::test]
    async fn concurrent_claims_get_distinct_paths() {
        let dir = std::env::temp_dir().join(format!(
            "app-monitor-claim-{}-{}",
            std::process::id(),
            crate::util::now_secs()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let first = claim_target(&dir, "same.zip").await.unwrap();
        let second = claim_target(&dir, "same.zip").await.unwrap();
        assert_ne!(first.part_path, second.part_path);
        assert_ne!(first.final_path, second.final_path);
        assert!(first.part_path.exists() && second.part_path.exists());

        drop(first);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
