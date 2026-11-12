use std::cell::{Cell, RefCell};
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
use handy_sdk::ui::dashboard::{Dashboard, Span, Spec, check, page, rich_value, section, value};
use handy_sdk::ui::overlay::StatusOverlay;
use handy_sdk::util::paths::app_data_dir;
use windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD;

use super::config::SettingsStore;
use super::fish_pictures::FishPictures;
use super::view::{
    bestiary_color, catch_spans, coin_spans, earned_spans, gps_text, hms, lifetime_spans, overlay_text, per_hour,
    save_warning, spans, styled, xp_spans,
};
use crate::game::catch::{Catch, LABEL_STROKE, mutation_tint, rod_pieces, rod_stats};
use crate::r#macro::bot::{Bot, release_inputs};
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
const ID_PHASE: i32 = 3;
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
            section("Auto fish"),
            check(ID_ON, "Auto fish"),
            check(ID_PERFECT, "Perfect cast\thold until the bar is full"),
            value(ID_LAST_CAST, "Last cast"),
            value(ID_PHASE, "Phase"),
            section("This session"),
            value(ID_TIME, "Time fishing"),
            value(ID_CATCH, "Catches").lines(2),
            // RichValue rows: styled like the game's own label for the same thing (view.rs)
            rich_value(ID_EARNED, "Earned"),
            rich_value(ID_XP, "XP gained"),
            section("Fish"), // kept across restarts
            // name, then kg · odds · value, in the game's colors and font; the fish's picture
            rich_value(ID_LAST, "Last catch").lines(2).picture(80),
            rich_value(ID_RAREST, "Rarest").lines(2).picture(80),
            page("Player"),
            section("Gear"),
            rich_value(ID_ROD, "Rod").lines(11), // name + up to 10 stat lines (Rod Of The Depths has 9)
            rich_value(ID_BAIT, "Bait"),
            section("Progress"),
            rich_value(ID_COINS, "Coins"),
            rich_value(ID_LEVEL, "Level"),
            rich_value(ID_BESTIARY, "Bestiary"),
            rich_value(ID_LIFETIME, "All time"),
            section("Location"),
            rich_value(ID_LOCATION, "Zone").lines(2),
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
        let status = deps.updater.status();
        let v = bot.view();
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
            let mut detail = deps.updater.detail();
            if !deps.hooks_ok {
                detail = format!("{detail} · hotkeys unavailable"); // the '/' chat hold-back is off
            }
            if !v.warning.is_empty() {
                detail = if detail.is_empty() {
                    v.warning.clone()
                } else {
                    format!("{detail} · {}", v.warning)
                };
            }
            ui.set_status(status_tone(status), status_name(status), &detail);
        }
        ui.set_text(ID_ON, &format!("Auto fish\tpress {} in Roblox", vk_name(st.toggle_vk)));
        let pl = &v.player;
        let rod = if pl.rod_label.is_empty() { &v.rod } else { &pl.rod_label }; // "[Momentum] Carrot Rod"
        if rod.is_empty() {
            ui.set_text(ID_ROD, "none equipped");
        } else {
            // the game's own stat lines; the wiki's base stats until the equipment panel has the rod
            let wiki = rod_stats(if pl.rod.is_empty() { &v.rod } else { &pl.rod });
            let mut stats = pl.rod_stats.clone();
            if stats.is_empty() && !wiki.is_empty() {
                stats = vec![wiki];
            }
            ui.set_spans(ID_ROD, &spans(&rod_pieces(rod, !v.rod.is_empty(), &stats)));
        }
        // the shop's bold name
        ui.set_spans(
            ID_BAIT,
            &if pl.bait.is_empty() {
                styled("–", "", false, "")
            } else {
                styled(&pl.bait, "", true, "")
            },
        );
        let zone = if pl.zone.is_empty() { "?" } else { &pl.zone };
        let world = if pl.world.is_empty() {
            String::new()
        } else {
            format!(" · {}", pl.world)
        };
        let mut place = vec![Span::plain(&format!("{zone}{world}"))];
        if let Some(pos) = &pl.pos {
            // bold white, as the GPS shows it
            place.push(Span {
                text: format!("\n{}", gps_text(pos, v.speed)),
                bold: true,
                ..Default::default()
            });
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
                s.extend(styled(" overall", "b3b3b3", false, LABEL_STROKE));
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
        ui.set_text(ID_PHASE, &v.phase);
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
        // the game's own icon when the catch popup had one and it has downloaded, else the wiki's picture
        let picture = |f: &Option<Catch>| {
            let Some(f) = f else { return PathBuf::new() };
            let game = if f.icon.is_empty() {
                PathBuf::new()
            } else {
                pictures.get(&f.icon)
            };
            if game.as_os_str().is_empty() {
                pictures.get(&f.species)
            } else {
                game
            }
        };
        // the game's own recolor when the popup was read, else the mutation's wiki color (a catch restored at startup)
        let tint = |f: &Option<Catch>| {
            f.as_ref()
                .and_then(|f| if f.looked { f.tint } else { mutation_tint(f) })
        };
        ui.set_picture(ID_LAST, &picture(&v.last_fish), tint(&v.last_fish));
        ui.set_picture(ID_RAREST, &picture(&v.rarest_fish), tint(&v.rarest_fish));
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
                per_hour(bot.macro_value.load(SeqCst), ms),
                per_hour(bot.macro_xp.load(SeqCst), ms),
            );
        }
        ui.set_spans(ID_EARNED, &earned_spans(bot.caught_value.load(SeqCst), &r.1));
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
                "{} caught · {}\n{} lost · streak {streak}{best}",
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
