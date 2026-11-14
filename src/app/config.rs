use std::path::Path;

use handy_sdk::util::ini::Ini;
use handy_sdk::util::shared_settings::{SaveSettings, SharedSettings};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_F1;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub toggle_vk: u32,
    /// rod auto-equip prefers: the one last equipped (empty: any rod)
    pub rod: String,
    /// hold the cast until the power bar is full (else release at once)
    pub perfect_cast: bool,
    /// Kept across restarts: catch popup text of the last and the rarest catch.
    pub last_catch: String,
    pub rarest: String,
    /// the game's icon and recolor (0xRRGGBB) of each, as read from its catch popup (empty / None: none)
    pub last_icon: String,
    pub last_tint: Option<u32>,
    pub rarest_icon: String,
    pub rarest_tint: Option<u32>,
    /// Earned counts only what the game's Sell All filters would sell (else every catch)
    pub earned_game_filter: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            toggle_vk: u32::from(VK_F1.0),
            rod: String::new(),
            perfect_cast: true,
            last_catch: String::new(),
            rarest: String::new(),
            last_icon: String::new(),
            last_tint: None,
            rarest_icon: String::new(),
            rarest_tint: None,
            earned_game_filter: true,
        }
    }
}

pub fn load_settings(file: &Path) -> Settings {
    let ini = Ini::new(file);
    let d = Settings::default();
    Settings {
        toggle_vk: ini.get_int("toggleVk", d.toggle_vk as i32) as u32,
        rod: ini.get_str("rod", &d.rod),
        perfect_cast: ini.get_bool("perfectCast", d.perfect_cast),
        last_catch: ini.get_str("lastCatch", &d.last_catch),
        rarest: ini.get_str("rarest", &d.rarest),
        last_icon: ini.get_str("lastIcon", ""),
        last_tint: get_tint(&ini, "lastTint"),
        rarest_icon: ini.get_str("rarestIcon", ""),
        rarest_tint: get_tint(&ini, "rarestTint"),
        earned_game_filter: ini.get_bool("earnedGameFilter", d.earned_game_filter),
    }
}

/// A saved tint; -1 (or a missing key, as in a save from before tints were kept) is none.
fn get_tint(ini: &Ini, key: &str) -> Option<u32> {
    u32::try_from(ini.get_int(key, -1)).ok()
}

fn put_tint(ini: &Ini, key: &str, tint: Option<u32>) {
    ini.put_int(key, tint.map_or(-1, |t| t as i32));
}

pub fn save_settings(file: &Path, s: &Settings) {
    let ini = Ini::new(file);
    ini.put_int("toggleVk", s.toggle_vk as i32);
    ini.put_str("rod", &s.rod);
    ini.put_bool("perfectCast", s.perfect_cast);
    ini.put_str("lastCatch", &s.last_catch);
    ini.put_str("rarest", &s.rarest);
    ini.put_str("lastIcon", &s.last_icon);
    put_tint(&ini, "lastTint", s.last_tint);
    ini.put_str("rarestIcon", &s.rarest_icon);
    put_tint(&ini, "rarestTint", s.rarest_tint);
    ini.put_bool("earnedGameFilter", s.earned_game_filter);
}

impl SaveSettings for Settings {
    fn save(&self, ini: &Path) {
        save_settings(ini, self);
    }
}

/// Settings shared between the engine, dashboard and updater threads.
pub type SettingsStore = SharedSettings<Settings>;

#[cfg(test)]
mod tests {
    use super::*;
    use handy_sdk::util::paths::{app_data_dir, set_app_data_dir_for_tests};
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_F2;

    #[test]
    fn config_defaults_match_spec() {
        let s = Settings::default();
        assert_eq!(s.toggle_vk, u32::from(VK_F1.0));
        assert!(s.rod.is_empty());
        assert!(s.perfect_cast);
    }

    #[test]
    fn config_roundtrip() {
        let dir = std::env::temp_dir().join("fisch-cfg-test's");
        set_app_data_dir_for_tests(Some(&dir));
        let ini = app_data_dir().join("settings.ini");
        let s = Settings {
            toggle_vk: u32::from(VK_F2.0),
            rarest: "You just caught a Big Fallen Batfish at 11.6kg! (1/9)".into(),
            last_catch: "You just caught a Stalactite at 10.5kg! (1/4)".into(),
            rod: "Rod Of The Depths".into(),
            perfect_cast: false,
            last_icon: "rbxassetid://123".into(),
            last_tint: Some(0xa60000),
            rarest_icon: String::new(),
            rarest_tint: None,
            earned_game_filter: false,
        };
        save_settings(&ini, &s);
        assert!(load_settings(&ini) == s);
        // a save from before icons were kept still loads, with none
        std::fs::write(
            &ini,
            "lastCatch=You just caught a Stalactite at 10.5kg! (1/4)
",
        )
        .unwrap();
        let old = load_settings(&ini);
        assert!(old.last_icon.is_empty() && old.last_tint.is_none() && old.rarest_tint.is_none());
        assert!(old.earned_game_filter); // from before Earned had a choice: the game's filters
        assert!(load_settings(&dir.join("missing.ini")) == Settings::default());
        set_app_data_dir_for_tests(None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn updater_settings_store_updates_visible_across_threads() {
        let st = SettingsStore::default();
        assert!(st.get().perfect_cast);
        std::thread::scope(|sc| {
            sc.spawn(|| st.update(|s| s.perfect_cast = false));
        });
        assert!(!st.get().perfect_cast);
    }
}
