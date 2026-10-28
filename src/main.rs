#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod download;
mod engine;
#[cfg(feature = "gpui-ui")]
mod gpui_ui;
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
    // 实验性 GPUI 前端（需 `cargo build --features gpui-ui`）。默认仍走 egui：
    // GPUI 的 Windows 后端要求 Win10+，而且功能还没对齐。
    #[cfg(feature = "gpui-ui")]
    if std::env::args().any(|a| a == "--gpui") {
        gpui_ui::run();
        return Ok(());
    }
    if std::env::args().any(|a| a == "--download-test") {
        download_cli(None, None);
        return Ok(());
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

    let choice = RendererChoice::from_env();
    if let Err(err) = launch(choice) {
        // DX12 起不来（Win7/8、驱动异常、适配器不可用）就退 OpenGL；
        // 两条路都失败才弹窗，避免"双击毫无反应"。
        if matches!(choice.renderer, eframe::Renderer::Wgpu) {
            log::warn!("DX12 后端启动失败: {err}，改用 glow(OpenGL) 重试");
            if launch(RendererChoice::glow()).is_ok() {
                return Ok(());
            }
        }
        report_fatal(&format!("{err}"));
        return Err(err);
    }
    Ok(())
}

/// 渲染后端：Windows 优先 DX12，失败退回 glow。
///
/// 兼容性覆盖顺序：
/// 1. **DX12 硬件适配器** —— D3D12 FL11_0 自 Kepler / HD 5000 一代起就有，新老独显/集显都能跑；
/// 2. **DX12 WARP** —— 没有可用显卡时（未装驱动的虚拟机、纯 RDP 会话、Basic Display Adapter），
///    Windows 自带的软件光栅化顶上。egui-wgpu 请求适配器时 `force_fallback_adapter: false`，
///    所以有硬件就优先硬件，没有才落到 WARP，正是我们要的行为；
/// 3. **glow(OpenGL 3.3)** —— Win7/8 这类没有 DX12 的老系统，或 DX12 初始化失败；
/// 4. 都失败 → 弹窗提示更新显卡驱动。
///
/// 关键点：wgpu 只编入 `dx12` 后端，**绝不枚举 Vulkan** —— 老 NVIDIA 驱动（实测 425.31 /
/// Kepler 移动显卡）在 Vulkan 首帧 present 时会崩在驱动内部（`nvoglv64.dll`），
/// `panic = "abort"` + `windows_subsystem = "windows"` 意味着接不住也打不出日志。
///
/// 非 Windows 平台直接用 glow，不启用 metal/vulkan 特性，避免半配置的 wgpu 后端。
/// `AM_GL=1` 可强制走 OpenGL（排障用）。
#[derive(Clone, Copy, Debug)]
struct RendererChoice {
    renderer: eframe::Renderer,
    backends: wgpu::Backends,
}

impl RendererChoice {
    fn glow() -> Self {
        Self {
            renderer: eframe::Renderer::Glow,
            backends: wgpu::Backends::empty(),
        }
    }

    fn dx12() -> Self {
        Self {
            renderer: eframe::Renderer::Wgpu,
            backends: wgpu::Backends::DX12,
        }
    }

    fn from_env() -> Self {
        if std::env::var("AM_GL").is_ok() {
            log::info!("AM_GL 已设置，强制使用 glow(OpenGL)");
            return Self::glow();
        }
        if cfg!(target_os = "windows") {
            Self::dx12()
        } else {
            Self::glow()
        }
    }
}

fn launch(choice: RendererChoice) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([980.0, 640.0])
            .with_title("App Monitor · 软件追踪下载器"),
        renderer: choice.renderer,
        // 限定后端：只允许 DX12，避免 wgpu 自己挑到会崩的组合。
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            supported_backends: choice.backends,
            ..Default::default()
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

/// GUI 子系统没有控制台，出错必须弹窗，否则就是"双击没反应"。
fn report_fatal(msg: &str) {
    eprintln!("app-monitor 启动失败: {msg}");
    let _ = rfd::MessageDialog::new()
        .set_title("App Monitor 启动失败")
        .set_description(format!(
            "{msg}\n\n界面需要 DirectX 12（含 WARP 软件渲染）或 OpenGL 3.3 或更高版本。\
             若本机没有可用的显卡驱动（例如未安装驱动的虚拟机、或纯远程桌面会话），\
             请更新显卡驱动后重试。"
        ))
        .set_level(rfd::MessageLevel::Error)
        .show();
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
