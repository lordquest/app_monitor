//! 极小的本地化层：目前只做中文与英文，一处一句对照写在调用处。
//!
//! 为什么语言与缩放都用进程级静态量，而不是把 `Lang` 一层层当参数传下去：
//! 界面只有一个窗口，语言是全局设置；而 `badge()` / `chip()` / `util::fmt_secs_ago`
//! 这些函数没有 `self`，`Tab::label()` / `SourceKind::label()` 甚至连 App 都碰不到。
//! 传参会让 60 多处调用点全都变形，换来的只是「可以同时跑两个语言的窗口」这种
//! 用不上的自由度。测试里 `set()` 一下就能切语言。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// 认 `zh*` / `en*`（含 `zh-CN`、`en_US` 这类带地区的写法），其余一律英文。
    ///
    /// 「不认识就英文」而不是「不认识就中文」：能走到这里的只有配置里手写的
    /// 怪值和系统语言代码，前者说明用户自己写错了，后者说明这台机器不是中文
    /// 系统 —— 给英文是更合理的兜底，也免得法语用户看到一屏中文。
    pub fn from_code(code: &str) -> Self {
        if code.trim().to_ascii_lowercase().starts_with("zh") {
            Lang::Zh
        } else {
            Lang::En
        }
    }

    /// 配置值 → 实际语言。空串表示「没选过」，此时跟随系统。
    pub fn resolve(code: &str) -> Self {
        if code.trim().is_empty() {
            Self::from_os()
        } else {
            Self::from_code(code)
        }
    }

    /// 操作系统界面语言。只分「中文 / 其它」，两类语言不值得为它引进依赖。
    fn from_os() -> Self {
        os_lang_code()
            .map(|c| Lang::from_code(&c))
            // 连系统语言都问不出来时按中文（本项目的原始用户群）—— 只在
            // Windows 以外的、连 LANG 都没设的容器里会遇到。
            .unwrap_or(Lang::Zh)
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// 设置里那两个语言按钮上显示的字，按各自的语言写（否则英文用户看不懂「英文」）。
    pub fn label(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
        }
    }

    /// 顶栏那颗小切换按钮上的字，要短。
    pub fn short(self) -> &'static str {
        match self {
            Lang::Zh => "中",
            Lang::En => "EN",
        }
    }
}

/// 系统界面语言的简短代码（`zh-CN` / `en-US` 这样即可，只有前缀有意义）。
#[cfg(windows)]
fn os_lang_code() -> Option<String> {
    // kernel32 的 `GetUserDefaultUILanguage` 返回 LANGID：低 10 位是主语言
    // （0x04 = 中文），高 6 位是地区。直接声明比为一个函数引进 windows-sys 划算
    // —— 那个 crate 会在 exe 里再链一份相同的导入表。
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    let langid = unsafe { GetUserDefaultUILanguage() };
    let primary = langid & 0x03ff;
    Some(if primary == 0x04 { "zh" } else { "en" }.to_string())
}

#[cfg(not(windows))]
fn os_lang_code() -> Option<String> {
    // 与 POSIX 的查找顺序一致：LC_ALL 压过 LC_MESSAGES，最后才看 LANG。
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn set(lang: Lang) {
    LANG.store(
        match lang {
            Lang::Zh => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

pub fn current() -> Lang {
    if LANG.load(Ordering::Relaxed) == 1 {
        Lang::En
    } else {
        Lang::Zh
    }
}

pub fn is_en() -> bool {
    current() == Lang::En
}

/// 当前语言下的文案。两行都写在调用处，漏翻的地方在英文界面上一眼就能看出来
/// （还剩一句中文），而不是悄悄回退成一句看不懂的中文。
pub fn t(zh: &'static str, en: &'static str) -> &'static str {
    match current() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}

/// 带参数的文案：`t1("已复制{}", "Copied {}", what)`。中文与英文的语序常常相反，
/// 与其在两边都塞占位符再猜位置，不如让调用方给两个完整模板。
pub fn t1(zh: &str, en: &str, arg: impl std::fmt::Display) -> String {
    format!("{}", Template(if is_en() { en } else { zh }, arg.to_string()))
}

/// 把 `{}` 换成实参。只支持一个占位符 —— 这一层不需要更复杂的东西。
struct Template<'a>(&'a str, String);

impl std::fmt::Display for Template<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.split_once("{}") {
            Some((before, after)) => write!(f, "{before}{}{after}", self.1),
            // 没有占位符时原样输出，再追加上实参只会更难查。
            None => f.write_str(self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 语言是进程级静态量，测试之间会互相影响，所以这几个用例统一走同一把锁。
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set(Lang::Zh);
        g
    }

    #[test]
    fn recognizes_language_codes_with_region_suffixes() {
        let _g = guard();
        assert_eq!(Lang::from_code("zh"), Lang::Zh);
        assert_eq!(Lang::from_code("zh-CN"), Lang::Zh, "带地区的中文仍然是中文");
        assert_eq!(Lang::from_code("EN"), Lang::En);
        assert_eq!(Lang::from_code(" en_US "), Lang::En, "两侧空白与大小写都不计较");
        // 认不出来的代码给英文兜底：写错配置或非中文系统都属这一类。
        assert_eq!(Lang::from_code(""), Lang::En);
        assert_eq!(Lang::from_code("fr-FR"), Lang::En);
    }

    #[test]
    fn empty_config_value_follows_the_system() {
        let _g = guard();
        // 空串表示「没选过」，此时应等于系统语言而不是某个写死的默认值。
        assert_eq!(Lang::resolve(""), Lang::from_os(), "配置留空要跟随系统");
        // 非空值优先，系统语言不再参与。
        assert_eq!(Lang::resolve("zh"), Lang::Zh);
        assert_eq!(Lang::resolve("en"), Lang::En);
    }

    #[test]
    fn switching_language_changes_lookups() {
        let _g = guard();
        assert_eq!(t("下载", "Download"), "下载");
        set(Lang::En);
        assert_eq!(t("下载", "Download"), "Download");
        assert!(is_en());
        assert_eq!(current().code(), "en");
    }

    #[test]
    fn template_puts_the_argument_in_the_right_place() {
        let _g = guard();
        // 中英语序不同：中文「已复制X」，英文「Copied X」，位置都在 {} 上。
        assert_eq!(t1("已复制{}", "Copied {}", "版本号"), "已复制版本号");
        set(Lang::En);
        assert_eq!(t1("已复制{}", "Copied {}", "version"), "Copied version");
        // 没有占位符时不该丢掉正文，也不该把实参拼在后面。
        assert_eq!(t1("保存", "Save", "x"), "Save");
    }
}
