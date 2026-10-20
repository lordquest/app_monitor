#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod download;
mod engine;
mod html;
mod http;
mod model;
mod parse;
mod provider;
mod store;
mod util;
mod views;

use std::path::PathBuf;

use eframe::egui;

fn main() -> eframe::Result<()> {
    if std::env::var("AM_DEBUG").is_ok() || std::env::var("AM_LOG").is_ok() {
        install_logger();
    }
    if std::env::args().any(|a| a == "--check") {
        headless_check();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--download-test") {
        download_test();
        return Ok(());
    }
    if let Some(pos) = std::env::args().position(|a| a == "--fetch") {
        let url = std::env::args().nth(pos + 1).unwrap_or_default();
        let save = std::env::args()
            .position(|a| a == "--save")
            .and_then(|i| std::env::args().nth(i + 1));
        fetch_probe(&url, save.as_deref());
        return Ok(());
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
        app::set_start_hint(name, cand);
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([980.0, 640.0])
            .with_title("App Monitor · 软件追踪下载器"),
        // wgpu → DX12 on Windows. Some drivers fail to present a WGL surface
        // (blank white window), so glow is only used when explicitly asked for.
        renderer: if std::env::var("AM_GL").is_ok() {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        ..Default::default()
    };

    eframe::run_native(
        "app-monitor",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            app::setup_style(&cc.egui_ctx);
            Ok(Box::new(app::App::new()))
        }),
    )
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
fn download_test() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    let store = store::Store::load();
    let http = Arc::new(http::Http::new("", None).expect("http client"));
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let Some(track) = store.tracks.iter().find(|t| t.kind == model::SourceKind::GitHub) else {
        println!("没有 GitHub 追踪项");
        return;
    };
    let release = match rt.block_on(provider::resolve(&http, track)) {
        Ok(r) => r,
        Err(e) => {
            println!("解析失败: {e}");
            return;
        }
    };
    let Some(asset) = release.assets.iter().find(|a| {
        a.kind == model::AssetKind::Direct
            && (a.name.contains("windows") || a.name.ends_with(".exe") || a.name.ends_with(".msi"))
    }) else {
        println!("没有可下载的 Windows 附件");
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

/// Diagnostic logger (only active with `AM_DEBUG` / `AM_LOG`): eframe reports
/// GL / winit problems through `log`, and without a logger they disappear.
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

/// egui ships a Latin-only font; load a system CJK face so Chinese labels render.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in font_candidates() {
        if let Ok(bytes) = std::fs::read(&path) {
            fonts
                .font_data
                .insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
            if let Some(p) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                p.insert(0, "cjk".to_owned());
            }
            if let Some(m) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                m.push("cjk".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn font_candidates() -> Vec<PathBuf> {    let mut v: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let fonts = dir.join("fonts");
            if let Ok(entries) = std::fs::read_dir(&fonts) {
                for e in entries.flatten() {
                    let p = e.path();
                    if matches!(
                        p.extension().and_then(|s| s.to_str()),
                        Some("ttf") | Some("ttc") | Some("otf")
                    ) {
                        v.push(p);
                    }
                }
            }
        }
    }
    if cfg!(target_os = "windows") {
        v.push("C:/Windows/Fonts/msyh.ttc".into());
        v.push("C:/Windows/Fonts/msyh.ttf".into());
        v.push("C:/Windows/Fonts/simhei.ttf".into());
        v.push("C:/Windows/Fonts/simsun.ttc".into());
    } else if cfg!(target_os = "macos") {
        v.push("/System/Library/Fonts/PingFang.ttc".into());
        v.push("/System/Library/Fonts/STHeiti Light.ttc".into());
        v.push("/Library/Fonts/Arial Unicode.ttf".into());
    } else {
        v.push("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc".into());
        v.push("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc".into());
        v.push("/usr/share/fonts/truetype/noto/NotoSansSC-Regular.otf".into());
        v.push("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc".into());
        v.push("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc".into());
        v.push("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.otf".into());
    }
    v
}
