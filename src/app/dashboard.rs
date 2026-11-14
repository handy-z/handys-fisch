use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant};

use handy_sdk::app::self_update::{SelfUpdate, UpdateState};
use handy_sdk::app::updater_thread::{AppStatus, Updater, status_name, status_tone};
use handy_sdk::input::focus::{ForegroundWatcher, activate_process_window};
use handy_sdk::input::key::vk_name;
use handy_sdk::ui::Tone;
use handy_sdk::ui::d2d::add_font_file;
use handy_sdk::ui::d2d::load_texture;
use handy_sdk::ui::dashboard::{Dashboard, Span, Spec, check, page, rich_value, section, tile, value};
use handy_sdk::ui::overlay::StatusOverlay;
use handy_sdk::ui::raster::{Scene, Texture, Tri, aura, render};
use handy_sdk::util::paths::app_data_dir;
use windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD;

use super::config::SettingsStore;
use super::fish_pictures::FishPictures;
use super::view::{
    bestiary_color, boost_spans, catch_spans, coin_spans, earned_spans, gps_text, hms, hue_shift, lifetime_spans,
    overlay_text, per_hour, rarity_rgb, save_warning, spans, styled, tally_spans, time_text, weather_spans, xp_spans,
};
use crate::game::catch::{Catch, LABEL_STROKE, bait_ability, bait_stats, rod_pieces, rod_stat_pieces, rod_stats};
use crate::r#macro::bot::{Bot, earned, release_inputs};
use handy_sdk::roblox::http::win_http_get_bytes;

pub const DASHBOARD_CLASS: &str = "handys-fisch-dashboard";

pub struct DashboardDeps<'a> {
    pub updater: &'a Updater,
    pub bot: &'a Bot,
    /// written here (ForegroundWatcher + every tick), read by the bot and the hook
    pub roblox_foreground: Arc<AtomicBool>,
    pub settings: &'a SettingsStore,
    /// LowLevelHooks::start() installed every hook
    pub hooks_ok: bool,
    pub update: Arc<SelfUpdate>,
}

const ID_ROD: i32 = 2;
const ID_CATCH: i32 = 4;
const ID_ON: i32 = 5;
const ID_LAST: i32 = 6;
const ID_RAREST: i32 = 7;
const ID_COINS: i32 = 8;
const ID_XP: i32 = 9;
const ID_EARNED: i32 = 10;
const ID_TIME: i32 = 11;
const ID_LEVEL: i32 = 12;
const ID_BAIT: i32 = 13;
const ID_LOCATION: i32 = 14;
const ID_BESTIARY: i32 = 15;
const ID_LIFETIME: i32 = 16;
const ID_PERFECT: i32 = 17;
const ID_LAST_CAST: i32 = 18;
const ID_ROD_CARD: i32 = 19;
const ID_WEATHER: i32 = 20;
const ID_DAYTIME: i32 = 21;
const ID_BOOSTS: i32 = 22;
const ID_TALLY: i32 = 23;
const ID_EARNED_FILTER: i32 = 24;
/// What a fish card last drew: its model, size, textures loaded and shimmer frame.
type DrawKey = (usize, u32, u32, usize, u128);
/// A card's model's textures in its order, for (model, textures loaded).
type CardTextures = ((usize, usize), Vec<Texture>);
const SHIMMER_FPS: u128 = 10; // a rippling ForceField's redraws per second: smooth enough, ~10 renders a second at most

/// Shows the dashboard and the "auto fish: on/off" overlay until the window is closed. The first close lets the bot
/// finish the fish (up to 3 min); a second close is immediate. UI thread only, one at a time.
pub fn run_dashboard(deps: &DashboardDeps, show: SHOW_WINDOW_CMD) -> i32 {
    // Hooks read the result from their LL callback, so it must be a plain atomic, never a live query.
    let mut foreground = ForegroundWatcher::new(deps.roblox_foreground.clone(), None);
    let overlay = StatusOverlay::new();
    overlay.create();
    let pictures = FishPictures::new(app_data_dir().join("fish"), Arc::new(win_http_get_bytes));
    let font_tried_for = RefCell::new(String::new()); // Roblox exe whose font we loaded (or tried to)
    let bot = deps.bot;
    let roblox_pid = Cell::new(0u32);
    // what each fish card last drew (model, size, textures loaded): the picture is still, so it's drawn once
    let drawn: RefCell<HashMap<i32, DrawKey>> = RefCell::new(HashMap::new());
    // each card's model's textures in its order, kept while the model and the textures loaded stay the same
    let card_textures: RefCell<HashMap<i32, CardTextures>> = RefCell::new(HashMap::new());
    let started = Instant::now(); // the shimmer's clock
    let textures: RefCell<HashMap<String, Texture>> = RefCell::new(HashMap::new()); // by asset, once downloaded
    let session_due = Cell::new(Instant::now()); // when the time fishing and the /h rates refresh next
    let rates = RefCell::new((String::new(), String::new(), String::new())); // catches, value, xp
    let quit_by: Cell<Option<Instant>> = Cell::new(None); // set by the first close: exit when the bot is ready, or then
    // started before the dashboard takes its borrows
    foreground.start();
    let foreground = foreground;

    let d = Dashboard::new(Spec {
        title: "handy's fisch".into(),
        badge: deps.update.badge().text,
        window_class: DASHBOARD_CLASS.into(),
        rows: vec![
            page("Fishing"),
            section("Controls"),
            check(ID_ON, "Auto fish"),
            check(ID_PERFECT, "Perfect cast\thold until the bar is full"),
            value(ID_LAST_CAST, "Last cast"),
            section("This session"),
            // two per line, Catches | Earned over XP | Time; money and xp styled like the game's own labels (view.rs)
            tile(ID_CATCH, "Catches"),
            tile(ID_EARNED, "Earned"),
            tile(ID_XP, "XP gained"),
            tile(ID_TIME, "Time fishing"),
            rich_value(ID_TALLY, "Rarities").lines(3), // rarities may wrap once; the counts line under them
            section("Fish"),                           // kept across restarts
            // Last catch | Rarest: the fish's picture over name · rarity, kg · odds, value · xp in the game's colors;
            // the picture is about 2:1 across the ~220 DIP tile, as long fish are
            tile(ID_LAST, "Last catch").picture(90).lines(3),
            tile(ID_RAREST, "Rarest").picture(90).lines(3),
            page("Player"),
            section("Gear"),
            // the wiki's pictures over the names, like the Fish cards; tall, as rod pictures are (192×362)
            tile(ID_ROD_CARD, "Rod").picture(130).lines(3), // enchants, name, equipped
            tile(ID_BAIT, "Bait").picture(130).lines(7),    // name, up to 4 wiki stats, ability (may wrap)
            // two to a line (Rod Of The Depths' 9 take 6 lines); room for a long one more
            rich_value(ID_ROD, "Stats").lines(7),
            section("Progress"),
            tile(ID_COINS, "Coins"),
            tile(ID_LEVEL, "Level"),
            tile(ID_BESTIARY, "Bestiary"),
            tile(ID_LIFETIME, "All time"),
            // its own page: on Fishing and Player it made the window taller than the screen
            page("World"),
            section("Conditions"), // the server's, from ReplicatedStorage/world
            rich_value(ID_WEATHER, "Weather").lines(2),
            value(ID_DAYTIME, "Time"),
            rich_value(ID_BOOSTS, "Boosts"),
            section("Location"),
            rich_value(ID_LOCATION, "Zone").lines(2),
            page("Settings"),
            section("Earned"),
            check(ID_EARNED_FILTER, "Match game's sell filters\toff: count every catch"),
        ],
        label_width: 100,
        value_width: 350,
        ..Default::default()
    });
    d.on_tick(|ui| {
        let b = deps.update.badge();
        ui.set_badge(&b.text, b.tone, &b.tip);
        if quit_by
            .get()
            .is_some_and(|by| bot.ready_to_quit.load(SeqCst) || Instant::now() >= by)
        {
            return ui.close();
        }
        let sess = deps.updater.session();
        roblox_pid.set(sess.as_ref().map_or(0, |s| s.proc.pid()));
        foreground.set_pid(roblox_pid.get());
        foreground.refresh();

        let st = deps.settings.get();
        let on = bot.on.load(SeqCst);
        ui.set_check(ID_ON, on);
        ui.set_check(ID_PERFECT, st.perfect_cast);
        ui.set_check(ID_EARNED_FILTER, st.earned_game_filter);
        // Earned from this session's catches, so switching the filter applies to all of them
        let filter = if st.earned_game_filter {
            bot.game_filter.lock().unwrap().clone()
        } else {
            None
        };
        let (earned_value, earned_on, kept) = earned(&bot.sold.lock().unwrap(), filter.as_ref());
        let status = deps.updater.status();
        let v = bot.view();
        let version = if v.world.version.is_empty() {
            String::new()
        } else {
            format!(
                "
Fisch {}",
                v.world.version
            ) // the game's version, to tell an update that moved things
        };
        ui.set_status_tip(&format!("{}{version}", deps.updater.detail()));
        ui.set_spans(ID_WEATHER, &weather_spans(&v.world));
        ui.set_text(ID_DAYTIME, &time_text(&v.world));
        ui.set_spans(ID_BOOSTS, &boost_spans(&v.world));
        ui.set_spans(
            ID_TALLY,
            &tally_spans(
                &v.rarities,
                v.mutated,
                v.shiny,
                v.sparkling,
                (bot.perfect_catches.load(SeqCst), bot.won_reels.load(SeqCst)),
            ),
        );
        let unsaved = save_warning(bot.unsaved_rows.load(SeqCst), bot.settings_unsaved.load(SeqCst));
        if quit_by.get().is_some() {
            // the closing status stays
        } else if !unsaved.is_empty() {
            // first: a crash loses these, while the pill still says why it stopped
            ui.set_status(Tone::Bad, "Not saving", &unsaved);
        } else if status == AppStatus::Ready && !on && !v.stopped.is_empty() {
            // else it just looks switched off
            let detail = format!("{} · press {} in Roblox", v.stopped, vk_name(st.toggle_vk));
            ui.set_status(Tone::Bad, "Auto fish stopped", &detail);
        } else {
            // Ready: the header is the bot's state (the phase), so no Phase row repeats it
            let ready = status == AppStatus::Ready;
            let title = match (ready, on) {
                (false, _) => status_name(status),
                (true, true) => "Fishing",
                (true, false) => "Off",
            };
            let phase = v.phase.strip_prefix("Off · ").unwrap_or(&v.phase);
            let hooks = if deps.hooks_ok { "" } else { "hotkeys unavailable" }; // the '/' chat hold-back is off
            let parts = [if ready { phase } else { "" }, hooks, v.warning.as_str()];
            let detail = parts
                .iter()
                .filter(|p| !p.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · ");
            ui.set_status(status_tone(status), title, &detail);
        }
        ui.set_text(ID_ON, &format!("Auto fish\tpress [{}]", vk_name(st.toggle_vk)));
        let pl = &v.player;
        let rod = if pl.rod_label.is_empty() { &v.rod } else { &pl.rod_label }; // "[Momentum] Carrot Rod"
        let rod_name = if pl.rod.is_empty() { &v.rod } else { &pl.rod }; // no enchant: the wiki page's title
        if rod.is_empty() {
            ui.set_text(ID_ROD_CARD, "none");
            ui.set_text(ID_ROD, "–");
        } else {
            ui.set_spans(ID_ROD_CARD, &spans(&rod_pieces(rod, !v.rod.is_empty())));
            // the game's own stat lines; the wiki's base stats until the equipment panel has the rod
            let stats = if pl.rod_stats.is_empty() {
                rod_stats(rod_name)
            } else {
                pl.rod_stats.clone()
            };
            ui.set_spans(ID_ROD, &spans(&rod_stat_pieces(&stats, 24)));
        }
        // the shop's bold name; the game says "None" with no bait on
        let bait = if pl.bait == "None" { "" } else { pl.bait.as_str() };
        ui.set_spans(
            ID_BAIT,
            &if bait.is_empty() {
                styled("none", "9a9a9a", false, "")
            } else {
                let mut s = styled(bait, "", true, "");
                let stats = bait_stats(bait);
                if !stats.is_empty() {
                    s.push(Span::plain(
                        "
",
                    ));
                    s.extend(spans(&rod_stat_pieces(&stats, 0))); // a half-width card: one per line
                }
                let ability = bait_ability(bait);
                if !ability.is_empty() {
                    s.extend(styled(
                        &format!(
                            "
{ability}"
                        ),
                        "9a9a9a",
                        false,
                        "",
                    ));
                }
                s
            },
        );
        // the wiki's pictures, as for fish; nothing (an empty path) while none or not downloaded yet
        for (id, name) in [(ID_ROD_CARD, rod_name.as_str()), (ID_BAIT, bait)] {
            let file = if name.is_empty() {
                PathBuf::new()
            } else {
                pictures.get(name)
            };
            ui.set_picture(id, &file, None);
        }
        let zone = if pl.zone.is_empty() { "?" } else { &pl.zone };
        let world = if pl.world.is_empty() {
            String::new()
        } else {
            format!(" · {}", pl.world)
        };
        let mut place = vec![Span::plain(&format!("{zone}{world}"))];
        if let Some(pos) = &pl.pos {
            // muted: the zone is the headline, the GPS readout a detail under it
            place.extend(styled(&format!("\n{}", gps_text(pos, v.speed)), "9a9a9a", false, ""));
        }
        ui.set_spans(
            ID_LOCATION,
            &if pl.zone.is_empty() && pl.pos.is_none() {
                styled("–", "", false, "")
            } else {
                place
            },
        );
        // the percent in the player list's color for it, then a grey "overall"
        let bestiary = match bestiary_color(&pl.bestiary) {
            Some(color) => {
                let mut s = styled(&pl.bestiary, &color, true, LABEL_STROKE);
                s.extend(styled("\noverall", "b3b3b3", false, LABEL_STROKE));
                s
            }
            None if pl.bestiary.is_empty() => styled("–", "", false, ""),
            None => styled(&pl.bestiary, "b3b3b3", false, LABEL_STROKE),
        };
        ui.set_spans(ID_BESTIARY, &bestiary);
        ui.set_spans(
            ID_LIFETIME,
            &lifetime_spans(
                pl.fish_caught as i64,
                pl.shiny as i64,
                pl.sparkling as i64,
                (pl.seconds_played / 3600.0) as i64,
            ),
        );
        let casts = bot.casts.load(SeqCst);
        let rated = if casts != 0 {
            format!(" · {}/{casts} perfect", bot.perfect_casts.load(SeqCst))
        } else {
            String::new()
        };
        ui.set_text(
            ID_LAST_CAST,
            &format!("{}{rated}", if v.last_cast.is_empty() { "–" } else { &v.last_cast }),
        );
        // The game's HUD font is Source Sans Pro (read off its TextLabels), which Roblox ships next to its exe: the
        // whole window uses it, in each face the game does.
        if let Some(s) = &sess
            && s.proc.exe_path() != *font_tried_for.borrow()
        {
            s.proc.exe_path().clone_into(&mut font_tried_for.borrow_mut());
            let fonts = Path::new(s.proc.exe_path())
                .parent()
                .unwrap_or(Path::new(""))
                .join("content\\fonts");
            // for GDI (the controls) and DirectWrite (the drawn text) alike
            for face in [
                "SourceSansPro-Bold.ttf",
                "SourceSansPro-It.ttf",
                "SourceSansPro-Semibold.ttf",
            ] {
                add_font_file(&fonts.join(face)); // else Windows fakes them from Regular
            }
            if add_font_file(&fonts.join("SourceSansPro-Regular.ttf")) {
                ui.set_font_family("Source Sans Pro");
            }
        }
        ui.set_spans(ID_LAST, &catch_spans(&v.last_fish, &v.last_catch));
        ui.set_spans(ID_RAREST, &catch_spans(&v.rarest_fish, &v.rarest));
        // the game's own picture: the 3D model its catch popup showed, as its camera saw it, else the popup's 2D icon and its
        // recolor; nothing when neither was read
        let picture = |f: &Option<Catch>| f.as_ref().map_or_else(PathBuf::new, |f| pictures.get(&f.icon));
        let tint = |f: &Option<Catch>| f.as_ref().and_then(|f| f.tint);
        for (id, model, fish) in [
            (ID_LAST, &v.last_model, &v.last_fish),
            (ID_RAREST, &v.rarest_model, &v.rarest_fish),
        ] {
            if !ui.picture_visible(id) {
                continue; // hidden: rendering would only burn CPU; the next visible tick catches up
            }
            match (model, ui.picture_size(id)) {
                (Some(model), Some((w, h))) if w > 0 && h > 0 => {
                    // textures download like icons: each one that arrives redraws the picture once
                    let mut cache = textures.borrow_mut();
                    for t in &model.textures {
                        if !cache.contains_key(t) {
                            let file = pictures.get(t);
                            if !file.as_os_str().is_empty() {
                                cache.insert(t.clone(), load_texture(&file).unwrap_or_default());
                            }
                        }
                    }
                    let loaded = model.textures.iter().filter(|t| cache.contains_key(*t)).count();
                    // a ForceField ripples: redraw it SHIMMER_FPS times a second, a still model once
                    // and a rainbow catch's colors turn, as the game turns them
                    let shimmer = model.tris.iter().any(|t| t.shimmer) || model.hue_rate != 0.0;
                    let elapsed = started.elapsed();
                    let frame = if shimmer {
                        elapsed.as_millis() * SHIMMER_FPS / 1000
                    } else {
                        0
                    };
                    let key = (Arc::as_ptr(model) as usize, w, h, loaded, frame);
                    if drawn.borrow().get(&id) != Some(&key) {
                        let mut maps = card_textures.borrow_mut();
                        let made = maps.get(&id).is_some_and(|(k, _)| *k == (key.0, loaded));
                        if !made {
                            let tex = model.textures.iter().map(|t| cache.get(t).cloned().unwrap_or_default());
                            maps.insert(id, ((key.0, loaded), tex.collect()));
                        }
                        let scene = Scene {
                            time: elapsed.as_secs_f32(),
                            ..model.scene
                        };
                        let turned: Vec<_>;
                        let tris = if model.hue_rate == 0.0 {
                            &model.tris
                        } else {
                            let by = model.hue_rate * scene.time;
                            turned = model
                                .tris
                                .iter()
                                .map(|t| Tri {
                                    rgb: hue_shift(t.rgb, by),
                                    ..*t
                                })
                                .collect();
                            &turned
                        };
                        let mut px = render(tris, &maps[&id].1, &scene, w, h);
                        // the popup's rays behind the fish are in its rarity's color
                        if let Some(rgb) = fish.as_ref().and_then(|f| rarity_rgb(&f.rarity)) {
                            aura(&mut px, w, h, rgb);
                        }
                        ui.set_picture_pixels(id, w, h, &px);
                        drawn.borrow_mut().insert(id, key);
                    }
                }
                _ => {
                    drawn.borrow_mut().remove(&id);
                    ui.set_picture(id, &picture(fish), tint(fish));
                }
            }
        }
        ui.set_spans(ID_COINS, &coin_spans(&v.coins));
        ui.set_spans(
            ID_LEVEL,
            &if v.level.is_empty() {
                styled("–", "", false, "")
            } else {
                spans(&v.level)
            },
        );
        let mut r = rates.borrow_mut();
        let now = Instant::now();
        if now >= session_due.get() {
            // they move with time: once a second
            session_due.set(now + Duration::from_secs(1));
            // Rates use only what was gained while auto fish was on, over the time it was fishing.
            let ms = bot.active_ms.load(SeqCst);
            ui.set_text(ID_TIME, &hms(ms));
            *r = (
                per_hour(i64::from(bot.macro_caught.load(SeqCst)), ms),
                per_hour(earned_on, ms),
                per_hour(bot.macro_xp.load(SeqCst), ms),
            );
        }
        let kept = filter.is_some().then_some(kept);
        ui.set_spans(ID_EARNED, &earned_spans(earned_value, &r.1, kept));
        ui.set_spans(ID_XP, &xp_spans(bot.xp_gained.load(SeqCst), &r.2));
        let (streak, best) = (bot.streak.load(SeqCst), bot.best_streak.load(SeqCst));
        let best = if best > streak {
            format!(" · best {best}")
        } else {
            String::new()
        }; // best only once broken
        ui.set_text(
            ID_CATCH,
            &format!(
                "{} caught\n{} · {} lost · streak {streak}{best}",
                bot.caught.load(SeqCst),
                r.0,
                bot.lost.load(SeqCst)
            ),
        );
        overlay.set(&overlay_text(on, &v.phase, &v.stopped), on);
    });
    d.on_badge_click(|ui| {
        if matches!(deps.update.state(), UpdateState::Ready { .. }) && deps.bot.on.load(SeqCst) {
            return ui.flash(); // not while the app is working
        }
        if deps.update.request_restart() {
            ui.close();
        } else {
            handy_sdk::ui::open_url(&deps.update.release_url());
        }
    });
    d.on_check(|_, id, checked| {
        if id == ID_PERFECT {
            return bot.persist(|s| s.perfect_cast = checked);
        }
        if id == ID_EARNED_FILTER {
            return bot.persist(|s| s.earned_game_filter = checked);
        }
        bot.on.store(checked, SeqCst);
    });
    // × or a plain taskkill: land the fish / unequip first (up to 3 min); a second close is immediate.
    d.on_close(|ui| {
        if quit_by.get().is_some() {
            return true;
        }
        quit_by.set(Some(Instant::now() + Duration::from_secs(180)));
        bot.quit_requested.store(true, SeqCst);
        ui.set_title("handy's fisch (closing after this fish)");
        ui.set_status(Tone::Busy, "Closing after this fish", "Close again to quit now");
        if bot.on.load(SeqCst) && roblox_pid.get() != 0 {
            activate_process_window(roblox_pid.get()); // clicking × took focus from Roblox: the reel plays on
        }
        false
    });
    d.on_end_session(release_inputs); // logoff/shutdown can end us without WM_DESTROY
    d.run(show)
}
