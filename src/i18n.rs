// Interface language: English or Arabic. All translated text goes through `t`,
// so adding or fixing a translation never touches the program logic.
// Source files are ASCII only: Arabic text is written as \u{...} escapes.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Globalization::GetUserDefaultUILanguage;

/// The user's choice in the tray menu (saved in config.txt as auto / ar / en).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Lang {
    #[default]
    Auto,
    Ar,
    En,
}

impl Lang {
    pub fn from_config(s: &str) -> Lang {
        match s {
            "ar" => Lang::Ar,
            "en" => Lang::En,
            _ => Lang::Auto,
        }
    }
    pub fn as_config(self) -> &'static str {
        match self {
            Lang::Auto => "auto",
            Lang::Ar => "ar",
            Lang::En => "en",
        }
    }
}

static ARABIC: AtomicBool = AtomicBool::new(false);

/// Applies the choice. Auto follows the Windows display language:
/// Arabic when Windows is in Arabic, English otherwise.
pub fn set(lang: Lang) {
    let arabic = match lang {
        Lang::Ar => true,
        Lang::En => false,
        // low 10 bits of a language id = primary language; 0x01 is Arabic
        Lang::Auto => (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == 0x01,
    };
    ARABIC.store(arabic, Ordering::Relaxed);
}

pub fn is_arabic() -> bool {
    ARABIC.load(Ordering::Relaxed)
}

/// Picks the English or the Arabic text.
pub fn t(en: &'static str, ar: &'static str) -> &'static str {
    if is_arabic() {
        ar
    } else {
        en
    }
}
