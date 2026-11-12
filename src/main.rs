#![windows_subsystem = "windows"]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use handy_sdk::app::args::arg_value;
use handy_sdk::app::crash_guard::on_crash;
use handy_sdk::app::self_update::{SelfUpdate, Startup};
use handy_sdk::app::session_reader::SessionReader;
use handy_sdk::app::single_instance::SingleInstance;
use handy_sdk::app::status_snapshot::{snapshot_allowed, status_line, wait_for};
use handy_sdk::app::updater_thread::{Updater, production_smoke_test};
use handy_sdk::input::hooks::{Handlers, LowLevelHooks};
use handy_sdk::roblox::events::{CHAT_KEYS, RobloxWatcher};
use handy_sdk::roblox::http::win_http_get;
use handy_sdk::util::event_bus::EventBus;
use handy_sdk::util::paths::{app_data_dir, set_app_name};
use handy_sdk::util::thread::StopThread;
use handy_sdk::util::timing::TimerResolution;
use handys_fisch::app::config::{SettingsStore, load_settings};
use handys_fisch::app::dashboard::{DASHBOARD_CLASS, DashboardDeps, run_dashboard};
use handys_fisch::app::offset_keys::required_keys;
use handys_fisch::game::reader::GameReader;
use handys_fisch::r#macro::bot::{Bot, describe_phase, release_inputs};
use handys_fisch::r#macro::slash_hold::hold_back_slash;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT;

const MUTEX: &str = "Local\\handys-fisch";

fn new_updater() -> Arc<Updater> {
    Arc::new(Updater::new(
        Arc::new(win_http_get),
        production_smoke_test,
        app_data_dir(),
        required_keys(),
    ))
}

/// `--status <file>`: read-only snapshot (no hooks, no input). Waits for Ready + one fishing read, or 15 s.
fn status_snapshot(out: &Path) -> i32 {
    if !snapshot_allowed(MUTEX, "handy's fisch", out) {
        return 1;
    }
    let updater = new_updater();
    let u = updater.clone();
    let thread = StopThread::spawn(move |st| u.run(st));
    let mut session_reader = SessionReader::for_updater(&updater, |s| GameReader::new(s.clone()));
    let mut s = None;
    wait_for(
        || {
            s = session_reader.get().and_then(|r| r.sample());
            s.is_some()
        },
        Duration::from_secs(15),
        Duration::from_millis(50),
    );
    let mut f = format!(
        "version: {}\nstatus: {}\n",
        env!("CARGO_PKG_VERSION"),
        status_line(&updater)
    );
    if let Some(s) = &s {
        let rod = if s.rod.is_empty() { "none in hand" } else { &s.rod };
        f += &format!("rod: {rod}\nphase: {}\n", describe_phase(s));
        if let Some(reader) = session_reader.get() {
            if let Some(c) = reader.coins() {
                f += &format!("coins: {c}\n");
            }
            if let Some(l) = reader.level_progress() {
                f += &format!("level: {}\n", l.level);
            }
        }
    } else {
        f += "game: not found (not in Fisch?)\n";
    }
    let _ = std::fs::write(out, f);
    thread.request_stop();
    if s.is_some() { 0 } else { 1 }
}

fn main() {
    set_app_name("handy's fisch");
    if let Some(out) = arg_value("--status") {
        std::process::exit(if out.is_empty() {
            2
        } else {
            status_snapshot(Path::new(&out))
        });
    }
    on_crash(release_inputs); // a crash mid-reel must not leave Space held down in Roblox
    // Only builds made by release.cmd update themselves (dev builds and build.cmd never do). Skipped while another
    // copy runs: its exe is in use and it would be replaced under it.
    let enabled = option_env!("HANDY_RELEASE").is_some() && !SingleInstance::is_running(MUTEX);
    let update = match SelfUpdate::start("handy-z", "handys-fisch", env!("CARGO_PKG_VERSION"), enabled) {
        Startup::Restarting => return, // an update staged earlier is starting; exit before anything else runs
        Startup::Running(u) => u,
    };
    let mut instance = SingleInstance::new(MUTEX, DASHBOARD_CLASS);
    if !instance.acquire() {
        return; // brought the running copy's window up
    }
    let timer = TimerResolution::new(); // 1 ms for the bot's 1 ms loop and precise_wait

    let ini = app_data_dir().join("config.ini");
    let store = Arc::new(SettingsStore::new(load_settings(&ini)));
    store.save(&ini); // first run: write the defaults so config.ini exists
    let roblox_foreground = Arc::new(AtomicBool::new(false));

    let updater = new_updater();
    let bus = EventBus::new();
    // bot thread only; chat is the only group the bot reads
    let watcher = RobloxWatcher::for_updater(&updater, bus.clone(), &CHAT_KEYS);
    let bot = Arc::new(Bot::new(
        updater.clone(),
        store.clone(),
        roblox_foreground.clone(),
        watcher,
        bus.clone(),
        ini.clone(),
        app_data_dir().join("catches.csv"),
    ));
    // Hook callback: O(1), atomics only.
    let (fg, b) = (roblox_foreground.clone(), bot.clone());
    let mut hooks = LowLevelHooks::new(Handlers {
        key: Some(Box::new(move |e| hold_back_slash(e, &fg, &b.hold_slash, &b.slash_held))),
        ..Default::default()
    });
    let u = updater.clone();
    let updater_thread = StopThread::spawn(move |st| u.run(st));
    let b = bot.clone();
    let bot_thread = StopThread::spawn(move |st| b.run(st));
    let hooks_ok = hooks.start();

    // SW_SHOWDEFAULT: the show command the launcher gave
    let deps = DashboardDeps {
        updater: &updater,
        bot: &bot,
        roblox_foreground: roblox_foreground.clone(),
        settings: &store,
        bus: &bus,
        hooks_ok,
        update: update.clone(),
    };
    let rc = run_dashboard(&deps, SW_SHOWDEFAULT);

    hooks.stop();
    drop(bot_thread); // stops and joins; Bot::run releases Space on exit
    store.save(&ini);
    // The Updater join can wait out a WinHTTP timeout (up to 10 s): let a relaunch start meanwhile.
    instance.release();
    drop(updater_thread);
    drop(timer);
    update.finish(); // after the mutex is released: starts the staged update
    std::process::exit(rc);
}
