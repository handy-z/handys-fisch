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
    /// flash the taskbar for catches at 1/alert_odds or rarer...
    pub alert_odds: i32,
    /// ...or with any of these comma-separated mutations/sizes
    pub alert_words: String,
    /// Kept across restarts: catch popup text of the last and the rarest catch.
    pub last_catch: String,
    pub rarest: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            toggle_vk: u32::from(VK_F1.0),
            rod: String::new(),
            perfect_cast: true,
            alert_odds: 1000,
            alert_words: "Shiny,Sparkling".into(),
            last_catch: String::new(),
            rarest: String::new(),
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
        alert_odds: ini.get_int("alertOdds", d.alert_odds).max(1),
        alert_words: ini.get_str("alertWords", &d.alert_words),
        last_catch: ini.get_str("lastCatch", &d.last_catch),
        rarest: ini.get_str("rarest", &d.rarest),
    }
}

pub fn save_settings(file: &Path, s: &Settings) {
    let ini = Ini::new(file);
    ini.put_int("toggleVk", s.toggle_vk as i32);
    ini.put_str("rod", &s.rod);
    ini.put_bool("perfectCast", s.perfect_cast);
    ini.put_int("alertOdds", s.alert_odds);
    ini.put_str("alertWords", &s.alert_words);
    ini.put_str("lastCatch", &s.last_catch);
    ini.put_str("rarest", &s.rarest);
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
        assert_eq!(s.alert_odds, 1000);
        assert_eq!(s.alert_words, "Shiny,Sparkling");
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
            alert_odds: 250,
            alert_words: "Hades' Curse, Shiny,été".into(), // UTF-8
        };
        save_settings(&ini, &s);
        assert!(load_settings(&ini) == s);
        assert!(load_settings(&dir.join("missing.ini")) == Settings::default());
        set_app_data_dir_for_tests(None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn config_clamps_fields() {
        let dir = std::env::temp_dir().join("fisch-cfg-clamp");
        std::fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("config.ini");
        save_settings(&ini, &Settings::default());
        Ini::new(&ini).put_str("alertOdds", "-3");
        let s = load_settings(&ini);
        assert_eq!(s.alert_odds, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn updater_settings_store_updates_visible_across_threads() {
        let st = SettingsStore::default();
        assert_eq!(st.get().alert_odds, 1000);
        std::thread::scope(|sc| {
            sc.spawn(|| st.update(|s| s.alert_odds = 90));
        });
        assert_eq!(st.get().alert_odds, 90);
    }
}
