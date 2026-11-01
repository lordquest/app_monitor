//! 极小的本地化层：中文 / 英文 / 日文 / 韩文，一处一句对照写在调用处。
//!
//! 为什么语言与缩放都用进程级静态量，而不是把 `Lang` 一层层当参数传下去：
//! 界面只有一个窗口，语言是全局设置；而 `badge()` / `chip()` / `util::fmt_secs_ago`
//! 这些函数没有 `self`，`Tab::label()` / `SourceKind::label()` 甚至连 App 都碰不到。
//! 传参会让 140 多处调用点全都变形，换来的只是「可以同时跑两个语言的窗口」这种
//! 用不上的自由度。测试里 `set()` 一下就能切语言。
//!
//! 中文与英文直接写在调用处（`t("下载", "Download")`），因为它们是最主要的两种
//! 语言、也是文案改动时最先动到的两行。日文与韩文放在文件末尾的 `TR` 表里，以
//! **英文串为索引**：这样调用点不必为了两种额外语言变成 4 行，改中文/英文时也
//! 不会碰到日韩。代价是表里查不到时会静默回退英文 —— `translations_cover_sources`
//! 这条测试就是拿来堵这个口子的：它扫源码，任何一处英文文案不在表里都会失败。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
    Ja,
    Ko,
}

impl Lang {
    /// 设置面板与顶栏按这个顺序排：中文 → 英文 → 日文 → 韩文。
    pub const ALL: [Lang; 4] = [Lang::Zh, Lang::En, Lang::Ja, Lang::Ko];

    /// 认 `zh*` / `en*` / `ja*` / `ko*`（含 `zh-CN`、`ja_JP.UTF-8` 这类带地区的写法），
    /// 其余一律英文。
    ///
    /// 「不认识就英文」而不是「不认识就中文」：能走到这里的只有配置里手写的
    /// 怪值和系统语言代码，前者说明用户自己写错了，后者说明这台机器既不是中文、
    /// 也不是日韩系统 —— 给英文是更合理的兜底，也免得法语用户看到一屏中文。
    pub fn from_code(code: &str) -> Self {
        let c = code.trim().to_ascii_lowercase();
        if c.starts_with("zh") {
            Lang::Zh
        } else if c.starts_with("ja") {
            Lang::Ja
        } else if c.starts_with("ko") {
            Lang::Ko
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

    /// 操作系统界面语言。中日韩各认一个前缀，其余英文 —— 四类语言不值得为它引进
    /// 一个 i18n 依赖。
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
            Lang::Ja => "ja",
            Lang::Ko => "ko",
        }
    }

    /// 语言下拉按钮与菜单项上显示的字，按各自的语言写（否则英文用户看不懂「英文」）。
    /// 按钮上写的是**当前**语言的名字：切到哪种界面都还认得出自己现在在哪。
    pub fn label(self) -> &'static str {
        match self {
            Lang::Zh => "中文",
            Lang::En => "English",
            Lang::Ja => "日本語",
            Lang::Ko => "한국어",
        }
    }
}

/// 系统界面语言的简短代码（`zh-CN` / `ja-JP` 这样即可，只有前缀有意义）。
#[cfg(windows)]
fn os_lang_code() -> Option<String> {
    // kernel32 的 `GetUserDefaultUILanguage` 返回 LANGID：低 10 位是主语言
    // （0x04 = 中文、0x11 = 日文、0x12 = 韩文），高 6 位是地区。直接声明比为一个
    // 函数引进 windows-sys 划算 —— 那个 crate 会在 exe 里再链一份相同的导入表。
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    let langid = unsafe { GetUserDefaultUILanguage() };
    Some(
        match langid & 0x03ff {
            0x04 => "zh",
            0x11 => "ja",
            0x12 => "ko",
            _ => "en",
        }
        .to_string(),
    )
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
            Lang::Ja => 2,
            Lang::Ko => 3,
        },
        Ordering::Relaxed,
    );
}

pub fn current() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::En,
        2 => Lang::Ja,
        3 => Lang::Ko,
        _ => Lang::Zh,
    }
}

pub fn is_en() -> bool {
    current() == Lang::En
}

/// 当前语言下的文案。中英两行写在调用处，日韩从 `TR` 表按英文串查；漏翻的地方
/// 会回退成英文（而不是悄悄回退成一句看不懂的中文）。
///
/// 日韩表以英文为索引，所以**调用点必须给准英文串** —— 改英文文案时如果忘了改表
/// 里的 key，`translations_cover_sources` 测试会失败。
pub fn t(zh: &'static str, en: &'static str) -> &'static str {
    match current() {
        Lang::Zh => zh,
        Lang::En => en,
        Lang::Ja => lookup(en, 1).unwrap_or(en),
        Lang::Ko => lookup(en, 2).unwrap_or(en),
    }
}

/// 带参数的文案：`t1("已复制{}", "Copied {}", what)`。中文与英文的语序常常相反，
/// 与其在两边都塞占位符再猜位置，不如让调用方给两个完整模板。
pub fn t1(zh: &'static str, en: &'static str, arg: impl std::fmt::Display) -> String {
    fill(t(zh, en), &[arg.to_string()])
}

/// 把模板里的 `{}` 按顺序换成实参。参数比占位符多时多的丢掉，占位符比参数多时
/// 原样留着 —— 两种情况都是写错了文案，留着比静默吞掉更容易在界面上一眼看出来。
pub fn fill(tpl: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut rest = tpl;
    for arg in args {
        match rest.split_once("{}") {
            Some((before, after)) => {
                out.push_str(before);
                out.push_str(arg);
                rest = after;
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// 带变量的整句：`tf!("上次检查 {}", "checked {}", util::fmt_secs_ago(ts))`。
///
/// 句子里有多处变量、或者变量夹在句子中间时，`t1` 只够放一个；这个宏把英文模板
/// 当作查表 key，日韩在 `TR` 里给同样位置的 `{}` 模板。
#[macro_export]
macro_rules! tf {
    ($zh:expr, $en:expr $(, $arg:expr)* $(,)?) => {{
        // 参数可能是 `&str`、`String`、数字或 `Display` 的临时值，统一转成 String
        // 才放得进同一张切片。clippy 展开后看到实参已经是 String 会建议省掉这次
        // `to_string`，但省不掉 —— 得靠 `allow` 按住。
        #[allow(clippy::unnecessary_to_owned)]
        let args: Vec<String> = vec![$($arg.to_string()),*];
        $crate::i18n::fill($crate::i18n::t($zh, $en), &args)
    }};
}

/// 日/韩文案查表。`lang` 只会是 1（日）或 2（韩）。
///
/// 用排序数组 + 二分而不是 `HashMap`：表是静态数据，条目一百多条，二分最多 8 次
/// 字符串比较，比一次性建表的锁或 `OnceLock` 都省事。顺序由生成脚本保证，
/// `translation_table_is_sorted` 这条测试兜底。
///
/// 查不到返回 `None`，由调用方回退英文 —— 这里不能直接返回入参，否则签名里的
/// 生命周期会跟"返回静态串"打架。
fn lookup(en: &str, lang: usize) -> Option<&'static str> {
    let i = TR.binary_search_by(|e| e.0.cmp(en)).ok()?;
    let (_, ja, ko) = TR[i];
    Some(if lang == 1 { ja } else { ko })
}

/// `(英文, 日文, 韩文)`，按英文升序排列。
#[rustfmt::skip]
static TR: &[(&str, &str, &str)] = &[
    (" · SHA-256 mismatch", " · SHA-256 不一致", " · SHA-256 불일치"),
    (" · SHA-256 ok", " · SHA-256 検証済み", " · SHA-256 정상"),
    ("+ Add", "+ 追加", "+ 추가"),
    ("Add", "追加", "추가"),
    ("Add tracked items", "追跡項目を追加", "추적 항목 추가"),
    ("Added to the download queue", "ダウンロードキューに追加しました", "다운로드 대기열에 추가했습니다"),
    ("Added to the queue — no official checksum, so the file can't be verified", "キューに追加しました — 公式チェックサムがないため検証できません", "대기열에 추가했습니다 — 공식 체크섬이 없어 검증할 수 없습니다"),
    ("Added {} item(s), checking versions…", "{} 件追加しました。バージョンを確認しています…", "{}개 항목을 추가했습니다. 버전을 확인하는 중…"),
    ("Added {} item(s), skipped {} duplicate(s)", "{} 件追加しました（重複 {} 件はスキップ）", "{}개 항목을 추가했습니다 (중복 {}개 건너뜀)"),
    ("Alternative", "別バージョン", "대체 버전"),
    ("Applied immediately", "すぐに反映されます", "즉시 적용됩니다"),
    ("Auto-check interval (0 = off)", "自動チェック間隔（0 = オフ）", "자동 확인 간격 (0 = 끔)"),
    ("Browse…", "参照…", "찾아보기…"),
    ("Cancel", "キャンセル", "취소"),
    ("Candidates", "候補", "후보"),
    ("Check all", "すべてチェック", "모두 확인"),
    ("Check on startup", "起動時にチェック", "시작 시 확인"),
    ("Checking every tracked item", "すべての追跡項目をチェックしています", "모든 추적 항목을 확인합니다"),
    ("Checking {}…", "確認中 {}…", "확인 중 {}…"),
    ("Checking…", "チェック中…", "확인 중…"),
    ("Choose the download folder", "ダウンロード先を選択", "다운로드 폴더 선택"),
    ("Clear finished", "終了分を消す", "완료된 항목 지우기"),
    ("Clear history", "履歴を消す", "기록 지우기"),
    ("Clear selection", "選択を解除", "선택 해제"),
    ("Concurrent downloads", "同時ダウンロード数", "동시 다운로드 수"),
    ("Confirm", "確定", "확인"),
    ("Copied {}", "{} をコピーしました", "{} 복사됨"),
    ("Copy", "コピー", "복사"),
    ("Copy all", "全文コピー", "전체 복사"),
    ("Copy link", "リンクをコピー", "링크 복사"),
    ("Copy version", "バージョンをコピー", "버전 복사"),
    ("Cross-platform release tracker", "クロスプラットフォーム版追跡ツール", "크로스 플랫폼 릴리스 추적기"),
    ("Ctrl+V to paste · Esc to close", "Ctrl+V で貼り付け · Esc で閉じる", "Ctrl+V 붙여넣기 · Esc 닫기"),
    ("Delete", "削除", "삭제"),
    ("Deleted \"{}\" and its local record", "「{}」を削除しました（ローカル記録も）", "\"{}\" 및 로컬 기록을 삭제했습니다"),
    ("Dev tools", "開発ツール", "개발 도구"),
    ("Download", "ダウンロード", "다운로드"),
    ("Download folder", "ダウンロード先", "다운로드 폴더"),
    ("Download history", "ダウンロード履歴", "다운로드 기록"),
    ("Download history cleared", "ダウンロード履歴を消しました", "다운로드 기록을 지웠습니다"),
    ("Download sites", "ダウンロードサイト", "다운로드 사이트"),
    ("Downloads", "ダウンロード", "다운로드"),
    ("Drag over some text first, then copy", "先にテキストを範囲選択してからコピーしてください", "먼저 텍스트를 끌어서 선택한 뒤 복사하세요"),
    ("Every tracked item is already being checked", "すべての追跡項目をチェック中です", "모든 추적 항목을 확인하는 중입니다"),
    ("Failed", "失敗", "실패"),
    ("Files", "ファイル", "파일"),
    ("Filter by name or URL", "名前または URL で絞り込み", "이름 또는 URL로 필터"),
    ("Folder", "フォルダー", "폴더"),
    ("GitHub", "GitHub", "GitHub"),
    ("GitHub token (optional — raises the API rate limit)", "GitHub トークン（任意・API 制限を緩和）", "GitHub 토큰 (선택 — API 한도 상향)"),
    ("Got it", "了解", "확인"),
    ("Hide", "閉じる", "숨기기"),
    ("History", "履歴", "기록"),
    ("Language", "言語", "언어"),
    ("Mark all read", "すべて既読にする", "모두 읽음 처리"),
    ("Mark read", "既読にする", "읽음 처리"),
    ("New version", "新しいバージョン", "새 버전"),
    ("No downloads yet", "ダウンロード履歴はありません", "다운로드 기록이 없습니다"),
    ("No matches", "該当なし", "일치 항목 없음"),
    ("No release resolved yet", "まだリリースを取得していません", "아직 릴리스를 확인하지 못했습니다"),
    ("No trackable URL found", "追跡できる URL が見つかりません", "추적할 수 있는 URL을 찾지 못했습니다"),
    ("Nothing new — all {} are already tracked", "新しい項目はありません（{} 件は追跡済み）", "새 항목 없음 — {}개는 이미 추적 중입니다"),
    ("Nothing to copy — no {}", "コピーできる{}がありません", "복사할 {}이 없습니다"),
    ("Nothing tracked yet", "追跡項目がありません", "추적 항목 없음"),
    ("Nothing tracked yet — try “+ Add” at the top right", "まだ何も追跡していません。右上の「+ 追加」から始めてください", "아직 추적하는 항목이 없습니다. 오른쪽 위 \"+ 추가\"를 눌러 시작하세요"),
    ("OS images", "OS イメージ", "OS 이미지"),
    ("One URL per line — GitHub repos and release pages, download-site pages and OS image pages all work. Pasting a paragraph that contains links works too.", "1 行に 1 つの URL。GitHub のリポジトリとリリースページ、ダウンロードサイト、OS イメージのページに対応します。リンクを含む文章を貼り付けても自動で抽出します。", "한 줄에 URL 하나 — GitHub 저장소와 릴리스 페이지, 다운로드 사이트, OS 이미지 페이지를 지원합니다. 링크가 포함된 문단을 붙여넣어도 됩니다."),
    ("Open", "開く", "열기"),
    ("Open data folder", "データフォルダーを開く", "데이터 폴더 열기"),
    ("Open page", "ページを開く", "페이지 열기"),
    ("Opened the net-disk link in your browser", "ネットドライブのリンクをブラウザーで開きました", "네트워크 드라이브 링크를 브라우저에서 열었습니다"),
    ("Opened the page in your browser", "ページをブラウザーで開きました", "페이지를 브라우저에서 열었습니다"),
    ("Opened the source page in your browser", "ソースページをブラウザーで開きました", "원본 페이지를 브라우저에서 열었습니다"),
    ("Overview", "概要", "개요"),
    ("Paste a page, repository or download-site URL to track first", "追跡するページ・リポジトリ・ダウンロードサイトの URL を貼り付けてください", "추적할 페이지, 저장소 또는 다운로드 사이트 URL을 먼저 붙여넣으세요"),
    ("Pause", "一時停止", "일시정지"),
    ("Prefer this OS", "この OS を優先", "이 OS 우선"),
    ("Preview", "プレビュー", "프리뷰"),
    ("Proxy (optional, e.g. http://127.0.0.1:7890)", "プロキシ（任意、例 http://127.0.0.1:7890）", "프록시 (선택, 예: http://127.0.0.1:7890)"),
    ("Proxy or token is invalid: {}", "プロキシまたはトークンが無効です：{}", "프록시 또는 토큰이 유효하지 않습니다: {}"),
    ("Proxy settings are invalid, using a direct connection: {}", "プロキシ設定が無効です。今回は直接接続で続行します：{}", "프록시 설정이 유효하지 않아 직접 연결로 진행합니다: {}"),
    ("Read the full notes ({} chars)", "全文を読む（{} 文字）", "전체 노트 보기 ({}자)"),
    ("Refresh", "再チェック", "새로고침"),
    ("Release notes", "更新内容", "릴리스 노트"),
    ("Resume", "再開", "계속"),
    ("Retry", "再試行", "다시 시도"),
    ("SHA-256 mismatch", "SHA-256 不一致", "SHA-256 불일치"),
    ("SHA-256 verified", "SHA-256 検証済み", "SHA-256 검증됨"),
    ("Save", "保存", "저장"),
    ("Select all", "すべて選択", "모두 선택"),
    ("Settings", "設定", "설정"),
    ("Settings saved", "設定を保存しました", "설정을 저장했습니다"),
    ("Source: {}", "ソース：{}", "출처: {}"),
    ("System", "システム", "시스템"),
    ("TRACKED", "追跡中", "추적 목록"),
    ("That file is already in the download queue", "このファイルはすでにキューにあります", "이 파일은 이미 대기열에 있습니다"),
    ("That is a magnet/ed2k link — use Copy and paste it into a download tool", "マグネット/ed2k リンクです。「コピー」でダウンロードツールに貼り付けてください", "마그넷/ed2k 링크입니다. \"복사\"로 다운로드 도구에 붙여넣으세요"),
    ("The download folder cannot be empty", "ダウンロード先を空にはできません", "다운로드 폴더는 비워 둘 수 없습니다"),
    ("This source lists no downloadable files", "このソースにはダウンロードできるファイルがありません", "이 소스에는 다운로드할 파일이 없습니다"),
    ("UI scale (raise it if the text feels small)", "表示倍率（文字が小さいときに上げる）", "화면 배율 (글자가 작으면 올리기)"),
    ("Versions", "旧バージョン", "이전 버전"),
    ("What's new", "更新内容", "업데이트 내용"),
    ("affects the UI only", "表示だけに影響します", "화면에만 적용됩니다"),
    ("an empty history", "空の履歴", "빈 기록"),
    ("canceled", "キャンセル済み", "취소됨"),
    ("checked {}", "最終確認 {}", "{} 확인함"),
    ("default settings", "既定の設定", "기본 설정"),
    ("direct", "直リンク", "직접 링크"),
    ("done", "完了", "완료"),
    ("download failed", "ダウンロード失敗", "다운로드 실패"),
    ("downloading", "ダウンロード中", "다운로드 중"),
    ("empty = direct connection", "空欄なら直接接続", "비워두면 직접 연결"),
    ("failed", "失敗", "실패"),
    ("failed · {}", "解析失敗 · {}", "분석 실패 · {}"),
    ("ghp_… (empty = anonymous)", "ghp_…（空欄なら匿名）", "ghp_… (비워두면 익명)"),
    ("it could not be read: {}", "読み取れませんでした：{}", "읽을 수 없습니다: {}"),
    ("its content is not valid JSON ({})", "内容が正しい JSON ではありません（{}）", "내용이 올바른 JSON이 아닙니다 ({})"),
    ("just now", "たった今", "방금"),
    ("link to \"{}\"", "「{}」へのリンク", "\"{}\" 링크"),
    ("net disk", "ネットドライブ", "네트워크 드라이브"),
    ("never", "なし", "없음"),
    ("no checksum", "チェックサムなし", "체크섬 없음"),
    ("off", "オフ", "꺼짐"),
    ("page", "ページ", "페이지"),
    ("paused", "一時停止中", "일시중지됨"),
    ("paused · {} / {}", "一時停止 · {} / {}", "일시정지 · {} / {}"),
    ("preview", "プレビュー", "프리뷰"),
    ("queued", "待機中", "대기 중"),
    ("release notes", "更新内容", "릴리스 노트"),
    ("release tracker", "リリース追跡", "릴리스 추적기"),
    ("selected {} character(s)", "選択した {} 文字", "선택한 {}자"),
    ("site search", "サイト検索", "사이트 검색"),
    ("size unknown", "サイズ不明", "크기 알 수 없음"),
    ("source page link", "ソースページのリンク", "원본 페이지 링크"),
    ("stable", "安定版", "안정판"),
    ("the built-in list", "内蔵の一覧", "기본 목록"),
    ("the file was empty, most likely an interrupted write", "ファイルが空です。書き込みが中断された可能性があります", "파일이 비어 있습니다. 쓰기가 중단된 것으로 보입니다"),
    ("the original file was left untouched", "元のファイルはそのまま残しています", "원본 파일은 그대로 두었습니다"),
    ("the original was kept as {}", "元のファイルは {} として保存しました", "원본은 {}로 보관했습니다"),
    ("unknown", "不明", "알 수 없음"),
    ("untagged", "バージョン表記なし", "버전 없음"),
    ("version number", "バージョン番号", "버전 번호"),
    ("v{} · data folder {}", "v{} · データフォルダー {}", "v{} · 데이터 폴더 {}"),
    ("waiting for first check", "初回チェック待ち", "첫 확인 대기 중"),
    ("{} active", "{} 件進行中", "{}개 진행 중"),
    ("{} d ago", "{} 日前", "{}일 전"),
    ("{} data file(s) could not be read — this session fell back to defaults", "{} 個のデータファイルを読み取れず、今回は既定の内容で起動しました", "{}개 데이터 파일을 읽지 못해 이번 세션은 기본값으로 실행됩니다"),
    ("{} downloaded (no official checksum)", "{} ダウンロード完了（公式チェックサムなし）", "{} 다운로드 완료 (공식 체크섬 없음)"),
    ("{} downloaded — SHA-256 verified", "{} ダウンロード完了 — SHA-256 検証済み", "{} 다운로드 완료 — SHA-256 검증됨"),
    ("{} downloaded, but the SHA-256 does not match", "{} ダウンロード完了。ただし SHA-256 が一致しません", "{} 다운로드 완료. 그러나 SHA-256이 일치하지 않습니다"),
    ("{} files", "{} ファイル", "{}개 파일"),
    ("{} finished", "{} 件完了", "{}개 완료됨"),
    ("{} h ago", "{} 時間前", "{}시간 전"),
    ("{} line(s) could not be parsed (ignored):", "{} 行を認識できませんでした（無視）：", "{}개 줄을 인식하지 못했습니다 (무시됨):"),
    ("{} min ago", "{} 分前", "{}분 전"),
    ("{} · published {} · {}", "{} · {} 公開 · ソース {}", "{} · {} 게시 · 출처 {}"),
    ("· {} — {}, fell back to {}. {}", "· {} — {}。今回は{}を使用します。{}", "· {} — {}. {}을(를) 사용합니다. {}"),
    ("↑ new {}", "↑ 新バージョン {}", "↑ 새 버전 {}"),
];

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
        assert_eq!(Lang::from_code("ja"), Lang::Ja);
        assert_eq!(Lang::from_code("ja-JP"), Lang::Ja);
        assert_eq!(Lang::from_code("ko_KR"), Lang::Ko, "下划线写法的地区后缀也认");
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
        assert_eq!(Lang::resolve("ja"), Lang::Ja);
        assert_eq!(Lang::resolve("ko"), Lang::Ko);
    }

    #[test]
    fn switching_language_changes_lookups() {
        let _g = guard();
        assert_eq!(t("下载", "Download"), "下载");
        set(Lang::En);
        assert_eq!(t("下载", "Download"), "Download");
        assert!(is_en());
        set(Lang::Ja);
        assert_eq!(t("下载", "Download"), "ダウンロード");
        set(Lang::Ko);
        assert_eq!(t("下载", "Download"), "다운로드");
        assert_eq!(current().code(), "ko");
        // 表里没有的英文串回退成英文，而不是空白或中文。
        assert_eq!(t("某某", "no such key in the table"), "no such key in the table");
    }

    #[test]
    fn template_puts_the_argument_in_the_right_place() {
        let _g = guard();
        // 中英语序不同：中文「已复制X」，英文「Copied X」，位置都在 {} 上。
        assert_eq!(t1("已复制{}", "Copied {}", "版本号"), "已复制版本号");
        set(Lang::En);
        assert_eq!(t1("已复制{}", "Copied {}", "version"), "Copied version");
        set(Lang::Ja);
        assert_eq!(t1("已复制{}", "Copied {}", "v"), "v をコピーしました");
        // 没有占位符时不该丢掉正文，也不该把实参拼在后面。
        set(Lang::En);
        assert_eq!(t1("保存", "Save", "x"), "Save");
    }

    #[test]
    fn fill_handles_several_arguments_in_order() {
        let _g = guard();
        assert_eq!(
            fill("已添加 {} 项，跳过 {} 个", &["3".into(), "1".into()]),
            "已添加 3 项，跳过 1 个"
        );
        // 参数多于占位符：多的丢掉，别把整句撑变形。
        assert_eq!(fill("共 {}", &["1".into(), "2".into()]), "共 1");
        // 占位符多于参数：留一个空白比吞掉整段好，界面上一眼能看出漏传了。
        assert_eq!(fill("{} 与 {}", &["a".into()]), "a 与 {}");
    }

    #[test]
    fn translation_table_is_sorted_and_unique() {
        // 二分查表的前提，排序写错会让随机几条文案静默回退英文。
        for w in TR.windows(2) {
            assert!(w[0].0 < w[1].0, "表未按英文升序：{:?} 之后是 {:?}", w[0].0, w[1].0);
        }
        // 两条同 key 时后一条永远查不到，等于白写。
        for (i, e) in TR.iter().enumerate() {
            assert!(!e.1.trim().is_empty(), "日文缺失：{}", e.0);
            assert!(!e.2.trim().is_empty(), "韩文缺失：{}", e.0);
            if i > 0 {
                assert_ne!(TR[i - 1].0, e.0, "重复的 key：{}", e.0);
            }
        }
    }

    /// 扫源码，确认每一处 `t(中文, 英文)` / `tf!(中文, 英文, …)` 的英文都在表里。
    ///
    /// 没有这条，漏翻只能靠人把每种语言都点一遍才发现 —— 而这个项目里 140 多处
    /// 调用点，靠点是不现实的。
    #[test]
    fn translations_cover_sources() {
        let sources = [
            ("ui.rs", include_str!("ui.rs")),
            ("model.rs", include_str!("model.rs")),
            ("util.rs", include_str!("util.rs")),
        ];
        let mut missing: Vec<String> = Vec::new();
        let mut total = 0usize;
        for (file, src) in sources {
            for (line, en) in english_keys(src) {
                total += 1;
                if TR.binary_search_by(|e| e.0.cmp(en.as_str())).is_err() {
                    missing.push(format!("{file}:{line}  {en:?}"));
                }
            }
        }
        assert!(total > 100, "扫描到的调用点太少（{total}），解析大概失效了");
        assert!(missing.is_empty(), "这些文案没有日/韩翻译：\n{}", missing.join("\n"));
    }

    /// 从源码里抠出 `t("…", "…")` 与 `tf!("…", "…", …)` 的第二个字符串字面量。
    ///
    /// 手写一个小扫描器而不是引 `regex`：只要认双引号、`\\` 转义和空白，
    /// 几十行就能覆盖这个项目的写法，还省一个只在测试里用的依赖。
    fn english_keys(src: &str) -> Vec<(usize, String)> {
        let b = src.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            // 找 `t(` 或 `tf!(`，前一个字符不能是标识符的一部分（排除 `fmt(`、
            // `shift(`、`assert_eq!(` 里的 `t(`）。
            let is_call = (b[i] == b't' && i + 1 < b.len() && b[i + 1] == b'(')
                || (b[i] == b't'
                    && i + 2 < b.len()
                    && b[i + 1] == b'f'
                    && b[i + 2] == b'!');
            let boundary = i == 0 || !is_ident(b[i - 1]);
            if !(is_call && boundary) {
                i += 1;
                continue;
            }
            let line = src[..i].bytes().filter(|&c| c == b'\n').count() + 1;
            let mut j = src[i..].find('(').map(|k| i + k).unwrap_or(b.len());
            j += 1;
            let Some((_, after_zh)) = literal(src, skip_ws(src, j)) else {
                i += 1;
                continue;
            };
            let after_comma = skip_ws(src, after_zh);
            if !src[after_comma..].starts_with(',') {
                i += 1;
                continue;
            }
            let Some((en, _)) = literal(src, skip_ws(src, after_comma + 1)) else {
                i += 1;
                continue;
            };
            out.push((line, en));
            i += 1;
        }
        out
    }

    fn is_ident(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }

    fn skip_ws(s: &str, mut i: usize) -> usize {
        while s.as_bytes().get(i).is_some_and(|c| c.is_ascii_whitespace()) {
            i += 1;
        }
        i
    }

    /// 读一个字符串字面量，返回（内容, 结束位置）。不是字符串就返回 `None`。
    fn literal(s: &str, i: usize) -> Option<(String, usize)> {
        let b = s.as_bytes();
        if *b.get(i)? != b'"' {
            return None;
        }
        let mut out = String::new();
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => {
                    let n = *b.get(j + 1)?;
                    j += 2;
                    if n == b'\n' {
                        // Rust 的续行转义：反斜杠 + 换行表示「接着写」，连同下一行
                        // 开头的缩进一起去掉。不认这个的话，那条长说明的 key 里会
                        // 多出一串空格和换行，跟表里永远对不上。
                        while b.get(j).is_some_and(|c| c.is_ascii_whitespace()) {
                            j += 1;
                        }
                        continue;
                    }
                    out.push(match n {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        other => other as char,
                    });
                }
                b'"' => return Some((out, j + 1)),
                _ => {
                    // 源码里没有非 ASCII 的转义麻烦：按字节复制能保住 UTF-8。
                    let start = j;
                    let mut k = j;
                    while k < b.len() && b[k] != b'"' && b[k] != b'\\' {
                        k += 1;
                    }
                    out.push_str(&s[start..k]);
                    j = k;
                }
            }
        }
        None
    }
}
