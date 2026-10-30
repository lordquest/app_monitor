//! 只做一件事：把 `assets/app.ico` 编进 Windows exe 的资源段。
//!
//! 为什么必须走资源而不是运行时设置：
//! - 资源管理器 / 任务栏 / Alt-Tab 显示的是 exe 里的 `RT_GROUP_ICON`，
//!   没有它就只能看到系统那个默认空白图标；
//! - GPUI 的 Windows 后端在 `load_icon()` 里固定按资源 ID `1` 取主图标
//!   （`LoadImageW(module, PCWSTR(1 as _), IMAGE_ICON, ..)`），窗口创建时交给
//!   `register_window_class` 的 `hIcon` 就来自这里，所以 ID 必须写 1。
//!
//! 这里**不**塞 manifest：`gpui.lib` 里已经带了一份 RT_MANIFEST（声明
//! PerMonitorV2、Common-Controls），再放一份同 ID 的资源会让 CVTRES 报
//! 「资源重复」(CVT1100) 直接链接失败。
//!
//! 非 Windows 构建直接跳过：Linux / macOS 用不到 PE 资源。

fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");

    #[cfg(windows)]
    windows_icon();
}

#[cfg(windows)]
fn windows_icon() {
    use std::path::PathBuf;

    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let icon = root.join("assets").join("app.ico");

    // rc.exe 按**当前工作目录**解析脚本里的相对路径，而 cargo 的 cwd 不保证是包根，
    // 所以这里写成绝对路径；rc 的字符串常量是 C 风格转义，反斜杠要写两遍。
    let ico_path = icon
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "");
    let rc = format!("1 ICON \"{ico_path}\"\n");
    let rc_path = out_dir.join("app.rc");
    std::fs::write(&rc_path, rc).expect("write app.rc");

    // 找不到 rc.exe（没装 Windows SDK）时只警告，不让整个构建失败：
    // 少个图标是难看，编不出来是编不出来。
    if let Err(why) = embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=嵌入 Windows 图标失败：{why}");
    }
}
