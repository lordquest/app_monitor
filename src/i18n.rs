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
    /// 认 `zh` / `en`，其余（含空串）一律当中文 —— 缺省语言不该是英文。
    pub fn from_code(code: &str) -> Self {
        if code.trim().eq_ignore_ascii_case("en") {
            Lang::En
        } else {
            Lang::Zh
        }
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
    fn defaults_to_chinese_for_unknown_codes() {
        let _g = guard();
        assert_eq!(Lang::from_code(""), Lang::Zh);
        assert_eq!(Lang::from_code("zh-CN"), Lang::Zh, "只认 en，其余都算中文");
        assert_eq!(Lang::from_code("EN"), Lang::En);
        assert_eq!(Lang::from_code(" En "), Lang::En);
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
