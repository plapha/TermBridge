//! Interface language shared by the CLI and the desktop backend.
//!
//! The language is process-global. Resolution order (first hit wins):
//! an explicit `set_lang` call (`--lang`, or the desktop language switch),
//! then `TERMBRIDGE_LANG`, then the locale variables `LC_ALL` / `LC_MESSAGES`
//! / `LANG` (on Windows also the user's default locale), then English.
//!
//! User-facing text is written at the call site with [`tr!`], English first:
//! `tr!("Session ended: {id}", "会话已结束: {id}")`. To add a language, add a
//! [`Lang`] variant and an arm in the macro and in the few helpers below.
//! Wire messages (`Response::Error.message`) stay English; clients map the
//! stable `code` to localized text with [`wire_message`].

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    /// Accepts `en`, `zh`, and locale-style values such as `zh_CN.UTF-8`,
    /// `zh-Hans` or `en_US`. `C` / `POSIX` count as English.
    pub fn parse(value: &str) -> Option<Lang> {
        let tag = value
            .split(['.', '@'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let primary = tag.split(['_', '-']).next().unwrap_or("");
        match primary {
            "en" | "c" | "posix" => Some(Lang::En),
            "zh" => Some(Lang::Zh),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Zh => "zh",
        }
    }
}

const UNSET: u8 = 0;
const EN: u8 = 1;
const ZH: u8 = 2;

static LANG: AtomicU8 = AtomicU8::new(UNSET);

/// Current language; detected from the environment on first use.
pub fn lang() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        EN => Lang::En,
        ZH => Lang::Zh,
        _ => {
            let detected = detect();
            // An explicit `set_lang` that raced with detection wins.
            let _ = LANG.compare_exchange(
                UNSET,
                encode(detected),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            match LANG.load(Ordering::Relaxed) {
                ZH => Lang::Zh,
                _ => Lang::En,
            }
        }
    }
}

pub fn set_lang(lang: Lang) {
    LANG.store(encode(lang), Ordering::Relaxed);
}

fn encode(lang: Lang) -> u8 {
    match lang {
        Lang::En => EN,
        Lang::Zh => ZH,
    }
}

/// Language implied by the environment, defaulting to English.
pub fn detect() -> Lang {
    detect_from(|name| std::env::var(name).ok())
}

fn detect_from(get: impl Fn(&str) -> Option<String>) -> Lang {
    if let Some(value) = get("TERMBRIDGE_LANG").filter(|v| !v.trim().is_empty()) {
        return Lang::parse(value.trim()).unwrap_or(Lang::En);
    }
    for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = get(name).filter(|v| !v.trim().is_empty()) {
            return Lang::parse(value.trim()).unwrap_or(Lang::En);
        }
    }
    system_locale()
        .and_then(|value| Lang::parse(&value))
        .unwrap_or(Lang::En)
}

#[cfg(windows)]
fn system_locale() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
    // LOCALE_NAME_MAX_LENGTH
    let mut buf = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 1 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize - 1]))
}

#[cfg(not(windows))]
fn system_locale() -> Option<String> {
    None
}

/// Picks the text for the current language and formats it.
///
/// `tr!("Saved {name}", "已保存 {name}")` — inline `{name}` captures and
/// positional arguments (`tr!("{}", "{}", value)`) both work.
#[macro_export]
macro_rules! tr {
    ($en:literal, $zh:literal $(, $arg:expr)* $(,)?) => {
        match $crate::i18n::lang() {
            $crate::i18n::Lang::En => format!($en $(, $arg)*),
            $crate::i18n::Lang::Zh => format!($zh $(, $arg)*),
        }
    };
}

/// Localized text for a wire error `code`; falls back to the English
/// `message` the host sent for codes this client does not know.
pub fn wire_message(code: &str, message: &str) -> String {
    match code {
        "host_stopped" => tr!("The host has stopped", "接收端已停止"),
        "not_attached" => tr!("Not attached to this session", "尚未挂接到该会话"),
        "session_not_found" => tr!(
            "The session does not exist or has ended",
            "会话不存在或已结束"
        ),
        "session_not_live" => tr!("The session has ended", "会话已结束"),
        "invalid_input" => tr!("Invalid input length or offset", "输入长度或偏移无效"),
        "invalid_data" => tr!("Input is not valid base64", "输入数据不是有效的 base64"),
        "input_gap" => tr!(
            "Input gap: the host expected an earlier offset",
            "输入缺口：接收端期望更早的偏移"
        ),
        "not_controller" => tr!(
            "Observer mode: keystrokes were not sent",
            "观察模式，按键未发送"
        ),
        "busy" => tr!(
            "The input queue is full; retry later from the acknowledged offset",
            "输入队列已满，请稍后按偏移重发"
        ),
        _ => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The language is process-global; serialize the tests that change it.
    static LANG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn parses_language_tags_and_locales() {
        assert_eq!(Lang::parse("en"), Some(Lang::En));
        assert_eq!(Lang::parse("en_US.UTF-8"), Some(Lang::En));
        assert_eq!(Lang::parse("C"), Some(Lang::En));
        assert_eq!(Lang::parse("POSIX"), Some(Lang::En));
        assert_eq!(Lang::parse("zh"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh_CN.UTF-8"), Some(Lang::Zh));
        assert_eq!(Lang::parse("zh-Hans"), Some(Lang::Zh));
        assert_eq!(Lang::parse("ZH-tw"), Some(Lang::Zh));
        assert_eq!(Lang::parse("fr_FR"), None);
        assert_eq!(Lang::parse(""), None);
    }

    #[test]
    fn detection_precedence() {
        assert_eq!(detect_from(env(&[])), Lang::En);
        assert_eq!(detect_from(env(&[("LANG", "zh_CN.UTF-8")])), Lang::Zh);
        // LC_ALL beats LANG; TERMBRIDGE_LANG beats both.
        assert_eq!(
            detect_from(env(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "zh_CN.UTF-8")])),
            Lang::En
        );
        assert_eq!(
            detect_from(env(&[("TERMBRIDGE_LANG", "zh"), ("LC_ALL", "en_US.UTF-8")])),
            Lang::Zh
        );
        // Empty variables are skipped; unknown locales fall back to English.
        assert_eq!(
            detect_from(env(&[("LC_ALL", ""), ("LANG", "zh_CN")])),
            Lang::Zh
        );
        assert_eq!(detect_from(env(&[("LANG", "de_DE.UTF-8")])), Lang::En);
    }

    #[test]
    fn tr_picks_language_and_formats() {
        let _guard = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = lang();
        set_lang(Lang::En);
        let name = "x";
        assert_eq!(tr!("Saved {name}", "已保存 {name}"), "Saved x");
        assert_eq!(tr!("{} and {}", "{} 与 {}", 1, 2), "1 and 2");
        set_lang(Lang::Zh);
        assert_eq!(tr!("Saved {name}", "已保存 {name}"), "已保存 x");
        assert_eq!(tr!("{} and {}", "{} 与 {}", 1, 2), "1 与 2");
        set_lang(previous);
    }

    #[test]
    fn wire_messages_are_localized_by_code() {
        let _guard = LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = lang();
        set_lang(Lang::En);
        assert_eq!(
            wire_message("session_not_live", "x"),
            "The session has ended"
        );
        assert_eq!(
            wire_message("unknown_code", "raw host text"),
            "raw host text"
        );
        set_lang(Lang::Zh);
        assert_eq!(wire_message("session_not_live", "x"), "会话已结束");
        assert_eq!(
            wire_message("unknown_code", "raw host text"),
            "raw host text"
        );
        set_lang(previous);
    }
}
