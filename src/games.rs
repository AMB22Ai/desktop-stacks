// Tells game shortcuts apart from other program shortcuts, so they get their own stack.
//
// Windows has no flag that says "this shortcut is a game". So this looks inside the
// shortcut file (.lnk or .url only) for a game store link (steam://...) or a game
// library folder (\steamapps\...). Nothing is parsed: it is a plain byte search over
// at most MAX_READ bytes, read locally. Nothing is stored except the yes/no answer.
// A game installed in an ordinary folder is not recognised and stays in Apps.

/// Shortcuts are a few KB. The cap keeps a huge file with a .lnk name from costing time.
const MAX_READ: u64 = 64 * 1024;

/// Lowercase ASCII markers. Store links first, then library and launcher folders.
const MARKS: [&str; 24] = [
    "steam://",
    "com.epicgames.launcher://",
    "uplay://",
    "origin://",
    "origin2://",
    "link2ea://",
    "battlenet://",
    "goggalaxy://",
    "amazon-games://",
    "\\steamapps\\",
    "\\steam\\steam.exe",
    "\\epic games\\",
    "\\riot games\\",
    "\\xboxgames\\",
    "\\gog games\\",
    "\\gog galaxy\\",
    "\\ubisoft game launcher\\",
    "\\electronic arts\\",
    "\\ea games\\",
    "\\origin games\\",
    "\\rockstar games\\",
    "\\amazon games\\",
    "\\battle.net\\",
    "microsoft.gamingapp",
];

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// True if the raw bytes of a shortcut mention a game store or a game library folder.
/// Shortcut files hold text both as plain bytes and as UTF-16, so both are searched,
/// ignoring ASCII case.
pub fn looks_like_game(bytes: &[u8]) -> bool {
    let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
    MARKS.iter().any(|m| {
        let wide: Vec<u8> = m.bytes().flat_map(|b| [b, 0]).collect();
        contains(&lower, m.as_bytes()) || contains(&lower, &wide)
    })
}

/// Reads the start of a shortcut file and checks it.
/// None = the file could not be read right now (locked, gone): ask again next time.
/// Only an ordinary file is opened. A symbolic link, a folder or a device with a
/// shortcut's name is answered with Some(false) without being read.
pub fn is_game_shortcut(path: &str) -> Option<bool> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return Some(false);
    }
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.file_type().is_file() {
        return Some(false);
    }
    let mut buf = Vec::new();
    file.take(MAX_READ).read_to_end(&mut buf).ok()?;
    Some(looks_like_game(&buf))
}

#[cfg(test)]
mod tests {
    use super::{is_game_shortcut, looks_like_game, MARKS};

    fn wide(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn store_links_in_url_shortcuts_are_games() {
        assert!(looks_like_game(b"[InternetShortcut]\r\nURL=steam://rungameid/730\r\n"));
        assert!(looks_like_game(b"[InternetShortcut]\r\nURL=com.epicgames.launcher://apps/Fortnite?action=launch\r\n"));
        assert!(looks_like_game(b"URL=uplay://launch/635/0"));
        assert!(looks_like_game(b"URL=battlenet://Pro"));
    }

    #[test]
    fn library_folders_in_lnk_shortcuts_are_games_in_either_text_form() {
        assert!(looks_like_game(b"\x4c\x00\x00\x00junkD:\\SteamLibrary\\steamapps\\common\\Game\\game.exe\x00"));
        assert!(looks_like_game(&wide("C:\\Riot Games\\Riot Client\\RiotClientServices.exe")));
        assert!(looks_like_game(&wide("E:\\XboxGames\\Some Game\\Content\\game.exe")));
        assert!(looks_like_game(&wide("C:\\Program Files (x86)\\Steam\\steam.exe")));
    }

    #[test]
    fn letter_case_does_not_matter() {
        assert!(looks_like_game(b"url=STEAM://RunGameId/1"));
        assert!(looks_like_game(&wide("C:\\PROGRAM FILES\\EPIC GAMES\\Launcher\\x.exe")));
    }

    #[test]
    fn ordinary_shortcuts_are_not_games() {
        assert!(!looks_like_game(b""));
        assert!(!looks_like_game(b"[InternetShortcut]\r\nURL=https://store.steampowered.com/app/730\r\n"));
        assert!(!looks_like_game(&wide("C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe")));
        assert!(!looks_like_game(&wide("C:\\Windows\\System32\\notepad.exe")));
        assert!(!looks_like_game(&wide("C:\\Users\\someone\\Documents\\steam notes.txt")));
    }

    #[test]
    fn marks_are_lowercase_ascii_so_the_search_can_match_them() {
        for m in MARKS {
            assert!(m.is_ascii() && *m == m.to_ascii_lowercase() && m.len() >= 8, "{m}");
        }
    }

    /// Small deterministic generator, so no extra crate is needed.
    fn next(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    #[test]
    fn a_marker_is_found_anywhere_in_random_bytes_in_both_text_forms_and_any_case() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for round in 0..400 {
            let mark = MARKS[(next(&mut seed) % MARKS.len() as u64) as usize];
            let mixed: String = mark
                .chars()
                .map(|c| if next(&mut seed) & 1 == 0 { c.to_ascii_uppercase() } else { c })
                .collect();
            let needle = if round & 1 == 0 { mixed.into_bytes() } else { wide(&mixed) };
            let before = (next(&mut seed) % 300) as usize;
            let after = (next(&mut seed) % 300) as usize;
            let mut data: Vec<u8> = (0..before).map(|_| next(&mut seed) as u8).collect();
            data.extend_from_slice(&needle);
            data.extend((0..after).map(|_| next(&mut seed) as u8));
            assert!(looks_like_game(&data), "round {round} mark {mark}");
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut seed = 42u64;
        for _ in 0..300 {
            let len = (next(&mut seed) % 2000) as usize;
            let data: Vec<u8> = (0..len).map(|_| next(&mut seed) as u8).collect();
            let _ = looks_like_game(&data);
        }
    }

    #[test]
    fn a_marker_at_an_odd_offset_is_still_found_in_wide_text() {
        let mut data = vec![7u8];
        data.extend(wide("D:\\Games\\steamapps\\common\\x.exe"));
        assert!(looks_like_game(&data));
    }

    #[test]
    fn an_unreadable_file_gives_no_answer_so_it_is_asked_again() {
        assert_eq!(is_game_shortcut("/this/path/does/not/exist.lnk"), None);
    }

    #[test]
    fn a_folder_or_link_with_a_shortcut_name_is_never_read() {
        let dir = std::env::temp_dir().join(format!("ds-games-kind-{}", std::process::id()));
        let folder = dir.join("folder.lnk");
        std::fs::create_dir_all(&folder).unwrap();
        assert_eq!(is_game_shortcut(folder.to_str().unwrap()), Some(false));
        #[cfg(unix)]
        {
            let target = dir.join("real.url");
            std::fs::write(&target, b"URL=steam://rungameid/10").unwrap();
            let link = dir.join("link.url");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(is_game_shortcut(target.to_str().unwrap()), Some(true));
            assert_eq!(is_game_shortcut(link.to_str().unwrap()), Some(false));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_start_of_a_large_file_is_read() {
        let dir = std::env::temp_dir().join(format!("ds-games-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let near = dir.join("near.url");
        let far = dir.join("far.lnk");
        std::fs::write(&near, b"[InternetShortcut]\r\nURL=steam://rungameid/10\r\n").unwrap();
        let mut big = vec![b'x'; 70 * 1024];
        big.extend_from_slice(b"steam://rungameid/10");
        std::fs::write(&far, &big).unwrap();
        assert_eq!(is_game_shortcut(near.to_str().unwrap()), Some(true));
        assert_eq!(is_game_shortcut(far.to_str().unwrap()), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
