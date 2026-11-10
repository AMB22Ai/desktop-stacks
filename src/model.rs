// File model: scan the desktop folder(s), classify by kind, group into stacks.
// The app only LISTS files. It never moves, renames or edits anything on its own.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{FILETIME, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, FindNextFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_SYSTEM, FILE_FLAGS_AND_ATTRIBUTES, WIN32_FIND_DATAW,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_DISPLAYNAME};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Kind {
    Apps,
    Games,
    Documents,
    Pdf,
    Spreadsheets,
    Presentations,
    Images,
    Screenshots,
    Videos,
    Audio,
    Archives,
    Developer,
    Other,
    /// Only used when the user turns on "Stack folders". Not part of KIND_ORDER.
    Folders,
}

pub const KIND_ORDER: [Kind; 13] = [
    Kind::Apps,
    Kind::Games,
    Kind::Documents,
    Kind::Pdf,
    Kind::Spreadsheets,
    Kind::Presentations,
    Kind::Images,
    Kind::Screenshots,
    Kind::Videos,
    Kind::Audio,
    Kind::Archives,
    Kind::Developer,
    Kind::Other,
];

impl Kind {
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Kind::Apps => t("Apps", "\u{062A}\u{0637}\u{0628}\u{064A}\u{0642}\u{0627}\u{062A}"),
            Kind::Games => t("Games", "\u{0623}\u{0644}\u{0639}\u{0627}\u{0628}"),
            Kind::Documents => t("Documents", "\u{0645}\u{0633}\u{062A}\u{0646}\u{062F}\u{0627}\u{062A}"),
            Kind::Pdf => t("PDF", "PDF"),
            Kind::Spreadsheets => t("Spreadsheets", "\u{062C}\u{062F}\u{0627}\u{0648}\u{0644}"),
            Kind::Presentations => t("Presentations", "\u{0639}\u{0631}\u{0648}\u{0636} \u{062A}\u{0642}\u{062F}\u{064A}\u{0645}\u{064A}\u{0629}"),
            Kind::Images => t("Images", "\u{0635}\u{0648}\u{0631}"),
            Kind::Screenshots => t("Screenshots", "\u{0644}\u{0642}\u{0637}\u{0627}\u{062A} \u{0634}\u{0627}\u{0634}\u{0629}"),
            Kind::Videos => t("Videos", "\u{0641}\u{064A}\u{062F}\u{064A}\u{0648}"),
            Kind::Audio => t("Audio", "\u{0635}\u{0648}\u{062A}\u{064A}\u{0627}\u{062A}"),
            Kind::Archives => t("Archives", "\u{0645}\u{0644}\u{0641}\u{0627}\u{062A} \u{0645}\u{0636}\u{063A}\u{0648}\u{0637}\u{0629}"),
            Kind::Developer => t("Developer", "\u{0628}\u{0631}\u{0645}\u{062C}\u{0629}"),
            Kind::Folders => t("Folders", "\u{0645}\u{062C}\u{0644}\u{062F}\u{0627}\u{062A}"),
            Kind::Other => t("Other", "\u{0623}\u{062E}\u{0631}\u{0649}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileItem {
    /// Full path, or a shell parsing name like "::{CLSID}" for system icons.
    pub path: String,
    pub name: String,
    pub mtime: u64,
    pub is_dir: bool,
    pub is_system: bool,
    pub kind: Kind,
}

impl FileItem {
    /// Key used for icon cache and animation identity.
    pub fn key(&self) -> String {
        format!("{}|{}", self.path, self.mtime)
    }
}

pub struct Model {
    pub files: Vec<FileItem>,
    /// Stack groups in display order; each holds indices into `files`, newest first.
    pub groups: Vec<(Kind, Vec<usize>)>,
    /// Indices of folders and system icons (shown after the stacks, not stacked).
    /// With "Stack folders" on, folders are a group instead and only system icons are here.
    pub loose_tail: Vec<usize>,
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i + 1 < name.len() => name[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

pub fn classify(name: &str) -> Kind {
    let ext = ext_of(name);
    let lower = name.to_lowercase();
    match ext.as_str() {
        "lnk" | "url" | "appref-ms" | "exe" | "msi" | "bat" | "cmd" => Kind::Apps,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "heic" | "heif" | "tif" | "tiff" | "svg"
        | "ico" | "avif" | "jxl" | "psd" | "raw" | "cr2" | "cr3" | "nef" | "arw" | "dng" | "exr" => {
            if lower.starts_with("screenshot")
                || lower.starts_with("screen shot")
                || lower.starts_with("\u{0644}\u{0642}\u{0637}\u{0629} \u{0634}\u{0627}\u{0634}\u{0629}")
            {
                Kind::Screenshots
            } else {
                Kind::Images
            }
        }
        "mp4" | "mov" | "mkv" | "avi" | "wmv" | "webm" | "m4v" | "flv" | "mpg" | "mpeg" | "ts"
        | "m2ts" | "3gp" => Kind::Videos,
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "wma" | "opus" | "aif" | "aiff" => {
            Kind::Audio
        }
        "pdf" => Kind::Pdf,
        "doc" | "docx" | "txt" | "rtf" | "md" | "odt" | "pages" | "epub" => Kind::Documents,
        "xls" | "xlsx" | "xlsm" | "csv" | "ods" | "numbers" => Kind::Spreadsheets,
        "ppt" | "pptx" | "key" | "odp" => Kind::Presentations,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" | "zst" => Kind::Archives,
        "py" | "js" | "rs" | "html" | "htm" | "css" | "json" | "xml" | "yaml" | "yml"
        | "toml" | "ps1" | "sh" | "c" | "cpp" | "h" | "cs" | "java" | "go" | "ipynb" | "sql" => {
            Kind::Developer
        }
        _ => Kind::Other,
    }
}

thread_local! {
    /// (path, mtime) -> "is a game shortcut", so a shortcut file is read once, not on every rescan.
    static GAMES: std::cell::RefCell<std::collections::HashMap<(String, u64), bool>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    /// lowercase path -> Apps or Games, chosen by the user from the right-click menu.
    static OVERRIDES: std::cell::RefCell<std::collections::HashMap<String, Kind>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Replaces the user's manual Apps / Games choices (paths are compared ignoring case).
pub fn set_overrides(list: &[(String, Kind)]) {
    OVERRIDES.with(|m| {
        let mut m = m.borrow_mut();
        m.clear();
        for (path, kind) in list {
            if matches!(kind, Kind::Apps | Kind::Games) {
                m.insert(path.to_lowercase(), *kind);
            }
        }
    });
}

/// The stack a file belongs to. The name decides, except that a program shortcut
/// (.lnk or .url) pointing at a game store or game library goes to Games.
/// `local` is false for cloud placeholders and offline files: those are not opened,
/// so listing the desktop never triggers a download.
fn kind_of(path: &str, fname: &str, mtime: u64, local: bool) -> Kind {
    let kind = classify(fname);
    if kind != Kind::Apps {
        return kind;
    }
    // the user's own choice ("Move to Games stack" / "Move to Apps stack") wins
    if let Some(k) = OVERRIDES.with(|m| m.borrow().get(&path.to_lowercase()).copied()) {
        return k;
    }
    if !local || !matches!(ext_of(fname).as_str(), "lnk" | "url") {
        return kind;
    }
    let key = (path.to_string(), mtime);
    let game = match GAMES.with(|m| m.borrow().get(&key).copied()) {
        Some(g) => g,
        None => match crate::games::is_game_shortcut(path) {
            Some(g) => {
                GAMES.with(|m| {
                    let mut m = m.borrow_mut();
                    if m.len() > 20_000 {
                        m.clear();
                    }
                    m.insert(key, g);
                });
                g
            }
            // not readable right now (locked by an installer, for example): not cached
            None => false,
        },
    };
    if game {
        Kind::Games
    } else {
        Kind::Apps
    }
}

fn filetime_u64(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

thread_local! {
    /// (path, mtime) -> display name, so rescans don't ask the shell again for every file.
    static NAMES: std::cell::RefCell<std::collections::HashMap<(String, u64), String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn display_name_cached(path: &str, mtime: u64, fallback: &str) -> String {
    let key = (path.to_string(), mtime);
    if let Some(n) = NAMES.with(|m| m.borrow().get(&key).cloned()) {
        return n;
    }
    let n = clean_label(&display_name(path, fallback));
    NAMES.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 20_000 {
            m.clear();
        }
        m.insert(key, n.clone());
    });
    n
}

/// Removes characters that can disguise a file name on screen: bidi embedding /
/// override / isolate controls (U+202A-U+202E, U+2066-U+2069) and control chars.
/// Ordinary Arabic text and the LRM/RLM marks are kept. Display only: the real
/// file is always opened by its real path.
pub fn clean_label(s: &str) -> String {
    s.chars()
        .filter(|&c| !c.is_control() && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}' | '\u{2060}' | '\u{FEFF}' | '\u{00AD}' | '\u{180E}' | '\u{2028}' | '\u{2029}'))
        .collect()
}

fn display_name(path: &str, fallback: &str) -> String {
    unsafe {
        let mut info = SHFILEINFOW::default();
        let r = SHGetFileInfoW(
            &HSTRING::from(path),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_DISPLAYNAME,
        );
        if r != 0 {
            let len = info.szDisplayName.iter().position(|&c| c == 0).unwrap_or(0);
            if len > 0 {
                return String::from_utf16_lossy(&info.szDisplayName[..len]);
            }
        }
    }
    fallback.to_string()
}

/// FILE_ATTRIBUTE_OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS: content is not on this disk.
const NOT_LOCAL: u32 = 0x0000_1000 | 0x0004_0000 | 0x0040_0000;

/// Upper bound on listed items, so a Desktop flooded with files cannot stall the app.
const MAX_ITEMS: usize = 10_000;

/// Names ending in a dot or a space cannot be addressed reliably through the shell
/// (Windows trims them, so an action could land on a different file): not listed.
fn odd_name(name: &str) -> bool {
    name.ends_with('.') || name.ends_with(' ')
}

/// Lists one folder. Returns false if the folder could not be read completely
/// (so the caller keeps the previous, complete list instead of showing a partial one).
fn scan_dir(dir: &str, out: &mut Vec<FileItem>) -> bool {
    let pattern = format!("{}\\*", dir.trim_end_matches('\\'));
    unsafe {
        let mut data = WIN32_FIND_DATAW::default();
        let h: HANDLE = match FindFirstFileW(&HSTRING::from(pattern), &mut data) {
            Ok(h) => h,
            Err(_) => return false,
        };
        let complete;
        loop {
            let len = data.cFileName.iter().position(|&c| c == 0).unwrap_or(0);
            // A name that is not valid UTF-16 cannot be handed to the shell unchanged
            // (an action could land on a different file): such a file is not listed.
            let fname = String::from_utf16(&data.cFileName[..len]).unwrap_or_default();
            let attrs = data.dwFileAttributes;
            let hidden = attrs & (FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0) != 0;
            if !fname.is_empty() && fname != "." && fname != ".." && !hidden && !odd_name(&fname) && out.len() < MAX_ITEMS {
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
                let path = format!("{}\\{}", dir.trim_end_matches('\\'), fname);
                let mtime = filetime_u64(data.ftLastWriteTime);
                let name = display_name_cached(&path, mtime, &fname);
                out.push(FileItem {
                    kind: if is_dir { Kind::Other } else { kind_of(&path, &fname, mtime, attrs & NOT_LOCAL == 0) },
                    path,
                    name,
                    mtime,
                    is_dir,
                    is_system: false,
                });
            }
            if let Err(e) = FindNextFileW(h, &mut data) {
                // ERROR_NO_MORE_FILES (18) is the normal end; anything else is a failed read
                complete = e.code() == windows::core::HRESULT::from_win32(18);
                break;
            }
        }
        let _ = FindClose(h);
        complete
    }
}

const RECYCLE_BIN: &str = "::{645FF040-5081-101B-9F08-00AA002F954E}";
const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
const USER_FILES: &str = "::{59031a47-3f72-44a7-89c5-5595fe6b30ee}";
const NETWORK: &str = "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}";
const CONTROL_PANEL: &str = "::{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}";

/// Windows' own "Desktop icon settings": 0 = shown, 1 = hidden. Missing value uses the default.
fn system_icon_enabled(clsid: &str, default_on: bool) -> bool {
    unsafe {
        let mut val: u32 = 0;
        let mut size = 4u32;
        let r = RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(
                "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\HideDesktopIcons\\NewStartPanel",
            ),
            &HSTRING::from(clsid.trim_start_matches("::")),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut val as *mut u32 as *mut _),
            Some(&mut size),
        );
        if r.is_ok() {
            val == 0
        } else {
            default_on
        }
    }
}

/// None = a folder could not be read right now (keep showing the previous list).
/// `stack_folders`: folders form a Folders stack instead of staying single icons.
pub fn scan(dirs: &[String], include_system: bool, stack_folders: bool) -> Option<Model> {
    let mut files = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        // the main desktop folder must be readable; Public Desktop is optional
        if !scan_dir(d, &mut files) && i == 0 {
            return None;
        }
    }
    if include_system {
        for (clsid, def, fallback) in [
            (THIS_PC, false, "This PC"),
            (USER_FILES, false, "User Files"),
            (NETWORK, false, "Network"),
            (CONTROL_PANEL, false, "Control Panel"),
            (RECYCLE_BIN, true, "Recycle Bin"),
        ] {
            if system_icon_enabled(clsid, def) {
                let name = display_name_shell(clsid).unwrap_or_else(|| fallback.to_string());
                files.push(FileItem {
                    path: clsid.to_string(),
                    name,
                    mtime: 0,
                    is_dir: true,
                    is_system: true,
                    kind: Kind::Other,
                });
            }
        }
    }

    let mut groups: Vec<(Kind, Vec<usize>)> = Vec::new();
    for k in KIND_ORDER {
        let mut idx: Vec<usize> = files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.is_dir && f.kind == k)
            .map(|(i, _)| i)
            .collect();
        if !idx.is_empty() {
            idx.sort_by(|a, b| files[*b].mtime.cmp(&files[*a].mtime));
            groups.push((k, idx));
        }
    }
    let mut folders: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.is_dir && !f.is_system)
        .map(|(i, _)| i)
        .collect();
    folders.sort_by_key(|i| files[*i].name.to_lowercase());
    let sys: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, f)| f.is_system)
        .map(|(i, _)| i)
        .collect();
    let loose_tail = if stack_folders {
        if !folders.is_empty() {
            groups.push((Kind::Folders, folders));
        }
        sys
    } else {
        folders.extend(sys);
        folders
    };
    Some(Model { files, groups, loose_tail })
}

fn display_name_shell(parse_name: &str) -> Option<String> {
    use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY};
    use windows::Win32::System::Com::CoTaskMemFree;
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(parse_name), None).ok()?;
        let p = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

#[allow(dead_code)]
pub fn pcwstr(s: &HSTRING) -> PCWSTR {
    PCWSTR(s.as_ptr())
}

#[cfg(test)]
mod tests {
    use super::clean_label;

    #[test]
    fn strips_direction_override_used_to_disguise_extensions() {
        assert_eq!(clean_label("invoice\u{202E}fdp.exe"), "invoicefdp.exe");
    }
    #[test]
    fn strips_isolates_controls_and_invisible_characters() {
        assert_eq!(clean_label("a\u{2066}b\u{2069}c\u{0007}d\te"), "abcde");
        assert_eq!(clean_label("a\u{200B}b\u{2060}c\u{FEFF}.pdf"), "abc.pdf");
        assert_eq!(clean_label("a\u{00AD}b\u{180E}c\u{2028}d\u{2029}.txt"), "abcd.txt");
    }
    #[test]
    fn keeps_normal_text_arabic_persian_joiners_and_emoji() {
        assert_eq!(clean_label("report (final).docx"), "report (final).docx");
        assert_eq!(clean_label("\u{0645}\u{0644}\u{0641} \u{200F}1.pdf"), "\u{0645}\u{0644}\u{0641} \u{200F}1.pdf");
        assert_eq!(clean_label("\u{0645}\u{200C}\u{0647} \u{1F468}\u{200D}\u{1F469}"), "\u{0645}\u{200C}\u{0647} \u{1F468}\u{200D}\u{1F469}");
    }
}

#[cfg(test)]
mod classify_tests {
    use super::{classify, Kind};

    #[test]
    fn extensions_decide_the_stack_whatever_their_case() {
        assert_eq!(classify("Report.PDF"), Kind::Pdf);
        assert_eq!(classify("clip.Mp4"), Kind::Videos);
        assert_eq!(classify("notes.DOCX"), Kind::Documents);
        assert_eq!(classify("setup.ExE"), Kind::Apps);
        assert_eq!(classify("Chrome.lnk"), Kind::Apps);
        assert_eq!(classify("photo.jpeg"), Kind::Images);
        assert_eq!(classify("Screenshot 2026-01-01.png"), Kind::Screenshots);
        assert_eq!(classify("budget.xlsx"), Kind::Spreadsheets);
        assert_eq!(classify("pack.7z"), Kind::Archives);
    }

    #[test]
    fn odd_names_never_panic_and_fall_into_other() {
        assert_eq!(classify("README"), Kind::Other);
        assert_eq!(classify("trailing."), Kind::Other);
        assert_eq!(classify(""), Kind::Other);
        assert_eq!(classify("\u{0645}\u{0644}\u{0641}"), Kind::Other);
        assert_eq!(classify("\u{0645}\u{0644}\u{0641}.pdf"), Kind::Pdf);
        assert_eq!(classify(".gitignore"), Kind::Other);
    }
}
