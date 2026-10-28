#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod download;
mod engine;
mod html;
mod http;
mod model;
mod parse;
mod provider;
mod store;
mod ui;
mod util;

fn main() {
    install_panic_dialog();
    if std::env::var("AM_DEBUG").is_ok() || std::env::var("AM_LOG").is_ok() {
        install_logger();
    }
    if std::env::args().any(|a| a == "--check") {
        headless_check();
        return;
    }
    if std::env::args().any(|a| a == "--download-test") {
        download_cli(None, None);
        return;
    }
    // `--download <名称> [--asset <子串>]`：真下载某个追踪项的一个附件到 ./.selftest。
    // 大文件下载出问题时（超时 / 续传 / 校验）这是唯一不依赖界面的复现手段。
    if let Some(pos) = std::env::args().position(|a| a == "--download") {
        let name = std::env::args()
            .nth(pos + 1)
            .filter(|v| !v.starts_with("--"));
        let asset = std::env::args()
            .position(|a| a == "--asset")
            .and_then(|i| std::env::args().nth(i + 1));
        download_cli(name.as_deref(), asset.as_deref());
        return;
    }
    if let Some(pos) = std::env::args().position(|a| a == "--fetch") {
        let url = std::env::args().nth(pos + 1).unwrap_or_default();
        let save = std::env::args()
            .position(|a| a == "--save")
            .and_then(|i| std::env::args().nth(i + 1));
        fetch_probe(&url, save.as_deref());
        return;
    }

    // `--select <名称> --cand <n>`: open a specific item (and candidate tab) directly.
    // Useful for screenshots and for reproducing a report about one row.
    if let Some(i) = std::env::args().position(|a| a == "--select") {
        let args: Vec<String> = std::env::args().collect();
        let name = args.get(i + 1).cloned();
        let cand = args
            .iter()
            .position(|a| a == "--cand")
            .and_then(|j| args.get(j + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        ui::set_start_hint(name, cand);
    }

    ui::run();
}

/// GUI 子系统没有控制台。Windows 上 `panic = "abort"` 意味着一次启动失败就是
/// 「双击没反应」，所以装一个 hook：先弹窗把原因说清楚（同时写进
/// `app_monitor_data/last-crash.txt`），再让进程按原方式退出。
fn install_panic_dialog() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info.to_string();
        let dir = crate::store::data_dir().clone();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("last-crash.txt"), &msg);
        eprintln!("app-monitor 崩溃: {msg}");
        let _ = rfd::MessageDialog::new()
            .set_title("App Monitor 意外退出")
            .set_description(format!(
                "{msg}\n\n详细信息已写入 {}。\
                 界面需要 Windows 10 及以上（Direct3D 11 + DirectComposition）。",
                dir.join("last-crash.txt").display()
            ))
            .set_level(rfd::MessageLevel::Error)
            .show();
        default_hook(info);
    }));
}

/// `--fetch <url> [--save out.html]`: fetch a page through the app's own HTTP
/// stack (same UA / TLS / redirect handling as the providers) and print a short
/// diagnosis. Handy when a site starts blocking or returns an unexpected page.
fn fetch_probe(url: &str, save: Option<&str>) {
    if url.is_empty() {
        println!("用法: app-monitor --fetch <url> [--save out.html]");
        return;
    }
    let store = store::Store::load();
    let http = match http::Http::new(&store.config.proxy, None) {
        Ok(h) => h,
        Err(e) => {
            println!("HTTP 客户端创建失败: {e}");
            return;
        }
    };
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    match rt.block_on(http.get_text(url)) {
        Ok(f) => {
            println!("status {}  len {}  final {}", f.status, f.body.len(), f.final_url);
            println!(
                "title  {}",
                html::page_title(&f.body).unwrap_or_else(|| "(无)".into())
            );
            if let Some(p) = save {
                match std::fs::write(p, &f.body) {
                    Ok(_) => println!("saved  {p}"),
                    Err(e) => println!("写文件失败: {e}"),
                }
            }
            if f.body.contains("Just a moment") || f.body.contains("cf-mitigated") {
                println!("警告: 疑似 Cloudflare 人机验证页，程序无法通过");
            }
        }
        Err(e) => println!("失败: {e}"),
    }
}

/// End-to-end download smoke test: resolves the first GitHub item, downloads its
/// Windows asset into `./.selftest` and verifies the published SHA-256.
fn download_cli(track_filter: Option<&str>, asset_filter: Option<&str>) {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let store = store::Store::load();
    let http = Arc::new(http::Http::new(&store.config.proxy, None).expect("http client"));
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let track = match track_filter {
        Some(f) => {
            let needle = f.to_lowercase();
            match store
                .tracks
                .iter()
                .find(|t| t.name.to_lowercase().contains(&needle))
            {
                Some(t) => t,
                None => {
                    println!("没有匹配「{f}」的追踪项");
                    return;
                }
            }
        }
        None => {
            let Some(t) = store.tracks.iter().find(|t| t.kind == model::SourceKind::GitHub) else {
                println!("没有 GitHub 追踪项");
                return;
            };
            t
        }
    };
    println!("追踪项 {}  ({})", track.name, track.url);
    let release = match rt.block_on(provider::resolve(&http, track)) {
        Ok(r) => r,
        Err(e) => {
            println!("解析失败: {e}");
            return;
        }
    };
    let pick = match asset_filter {
        Some(f) => {
            let needle = f.to_lowercase();
            release
                .assets
                .iter()
                .find(|a| a.kind == model::AssetKind::Direct && a.name.to_lowercase().contains(&needle))
        }
        None => release.assets.iter().find(|a| {
            a.kind == model::AssetKind::Direct
                && (a.name.contains("windows")
                    || a.name.ends_with(".exe")
                    || a.name.ends_with(".msi"))
        }),
    };
    let Some(asset) = pick else {
        println!("没有匹配的可下载附件");
        for a in &release.assets {
            println!("  [{}] {}", a.kind.label(), a.name);
        }
        return;
    };
    println!(
        "下载 {} ({}) → ./.selftest",
        asset.name,
        asset.size.map(util::fmt_bytes).unwrap_or_else(|| "?".into())
    );
    let spec = download::DlSpec {
        id: "selftest".into(),
        track_id: track.id.clone(),
        app: track.name.clone(),
        version: release.version.clone(),
        file: asset.name.clone(),
        url: asset.url.clone(),
        dir: std::path::PathBuf::from("./.selftest"),
        sha256: asset.sha256.clone(),
        referer: None,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(false));
    let last = std::sync::Mutex::new(std::time::Instant::now());
    let outcome = rt.block_on(download::run(&http, &spec, cancel, paused, |p| {
        if let Ok(mut l) = last.lock() {
            if l.elapsed() > std::time::Duration::from_millis(600) {
                println!(
                    "  {} / {}  {}",
                    util::fmt_bytes(p.downloaded),
                    util::fmt_bytes(p.total),
                    util::fmt_speed(p.speed)
                );
                *l = std::time::Instant::now();
            }
        }
    }));
    println!("结果: {outcome:?}");
}

/// Headless mode (`app-monitor --check`): resolve every tracked item and print a
/// summary — handy for debugging providers or wiring this into a scheduled task.
fn headless_check() {
    let store = store::Store::load();
    for issue in &store.issues {
        println!("注意: {} {}，本次使用{}", issue.file, issue.what, issue.fallback);
    }
    let http = std::sync::Arc::new(
        http::Http::new(&store.config.proxy, None).expect("http client"),
    );
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    // Optional filter: `--check HWiNFO` only resolves matching items.
    let filter = std::env::args()
        .skip_while(|a| a != "--check")
        .nth(1)
        .filter(|a| !a.starts_with("--"));
    let tracks: Vec<_> = store
        .tracks
        .iter()
        .filter(|t| match &filter {
            Some(f) => t.name.to_lowercase().contains(&f.to_lowercase()),
            None => true,
        })
        .collect();
    println!("共 {} 个追踪项\n", tracks.len());
    for t in tracks {
        println!("=== {} [{}] {}", t.name, t.group, t.url);
        match rt.block_on(provider::resolve(&http, t)) {
            Ok(r) => {
                println!("    版本: {}   发布: {:?}   来源: {}", r.version, r.published, r.provider);
                println!("    标题: {}", r.title);
                println!("    更新摘要: {}", r.summary.lines().next().unwrap_or(""));
                if std::env::args().any(|a| a == "--verbose") {
                    println!(
                        "    正文开头: {}",
                        r.notes.chars().take(180).collect::<String>().replace('\n', " / ")
                    );
                    println!(
                        "    正文长度 {}，更新记录@{:?} 新增变化@{:?}",
                        r.notes.chars().count(),
                        r.notes.find("更新记录"),
                        r.notes.find("新版变化")
                    );
                }
                println!("    附件 {} 个，历史 {} 条", r.assets.len(), r.history.len());
                for a in r.assets.iter().take(6) {
                    println!(
                        "      [{}] {}  {}  {}",
                        a.kind.label(),
                        a.name,
                        a.size.map(crate::util::fmt_bytes).unwrap_or_default(),
                        a.url
                    );
                }
                if r.candidates.len() > 1 {
                    println!("    版本候选 {} 个（详情页右下角以标签切换）:", r.candidates.len());
                    for c in &r.candidates {
                        println!(
                            "      · {}  发布 {}  附件 {} 个  {}",
                            c.tab_label(),
                            c.date.clone().unwrap_or_else(|| "未知".into()),
                            c.assets.len(),
                            c.page_url
                        );
                    }
                }
                for w in &r.warnings {
                    println!("    注意: {w}");
                }
            }
            Err(e) => println!("    失败: {e}"),
        }
        println!();
    }
}

/// Diagnostic logger (only active with `AM_DEBUG` / `AM_LOG`).
fn install_logger() {
    struct Stderr;
    impl log::Log for Stderr {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            eprintln!("[{:?}] {}: {}", record.level(), record.target(), record.args());
        }
        fn flush(&self) {}
    }
    static LOGGER: Stderr = Stderr;
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Debug);
}
