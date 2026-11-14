use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use handy_sdk::app::updater_thread::{Session, Updater};
use handy_sdk::input::focus::{foreground_is, roblox_ready};
use handy_sdk::input::hooks::KeyPoller;
use handy_sdk::input::key::{BACKSLASH, ENTER, Key, MOUSE_LEFT, SLASH, SPACE, digit_key, key_down};
use handy_sdk::input::send::{press, release, tap};
use handy_sdk::roblox::events::RobloxWatcher;
use handy_sdk::util::timing::{Sustained, precise_wait};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};

use crate::app::config::{Settings, SettingsStore, load_settings};
use crate::app::view::hue;
use crate::game::catch::{Catch, Piece, is_item_popup, kg_text, level_pieces, parse_catch};
use crate::game::reader::{
    CatchLook, FishState, GameReader, PlayerInfo, ROD_IDLE, ROD_LURE, ROD_REELING, World, line_out, reel_state,
};
use crate::r#macro::rarest_model::{self, FishModel};

const MS: fn(u64) -> Duration = Duration::from_millis;

/// 15 ms down, then 15 ms up before the next input
fn tap_key(k: &Key) {
    tap(k, MS(15));
    precise_wait(MS(15));
}

fn percent(f: f32) -> String {
    format!("{}%", (f * 100.0 + 0.5) as i32)
}

/// Reel: hold Space (the bar goes right) while the fish is right of the bar, both looked ahead by their speed (X
/// scale per s) to cover Roblox's ~80 ms input lag and the bar's momentum.
pub fn reel_hold(bar: f32, bar_v: f32, fish: f32, fish_v: f32) -> bool {
    // Leads tuned in a closed-loop sim on bar physics fitted to 28 recorded reels (±1.1/s², ~50 ms lag): the bar lead
    // 0.30 (was 0.35) settles centred after a 0.4-0.55 fish jump in 1.23 s on average (was 1.47 s) with no real
    // overshoot, and keeps the fish slightly closer to centre on the recorded fish. Below ~0.28 it overshoots.
    fish + 0.2 * fish_v > bar + 0.3 * bar_v
}

/// Dreambreaker swaps the reel's controls from 40% progress on: holding Space moves the bar left. Rule taken from the
/// public Hikari macro (rod name contains "dreambreaker", progress >= 40%); not yet seen in our own recordings.
pub fn controls_inverted(rod: &str, progress: f32) -> bool {
    progress >= 0.4 && rod.to_lowercase().contains("dreambreaker")
}

/// The bot's own reel phase, with how fast the progress bar fills and the game's progress speed for this reel:
/// "Reeling 45% · +8.2%/s (+55%)" (a negative rate: the fish is winning). per_second is in bar widths per s;
/// game_speed is Reel::progress_speed ("+55% Progress Speed"), left out when empty.
pub fn reeling_text(progress: f32, per_second: f32, game_speed: &str) -> String {
    let mut out = format!("Reeling {} · {:+.1}%/s", percent(progress), per_second * 100.0);
    // "+55% Progress Speed" -> " (+55%)"
    let pct = game_speed.split(' ').next().unwrap_or("");
    if pct.ends_with('%') {
        out += &format!(" ({pct})");
    }
    out
}

/// What the player is doing, for the dashboard: "Reeling 45%", "Shaking", "Waiting for a bite", "Casting"...
pub fn describe_phase(s: &FishState) -> String {
    if let Some(r) = &s.reel
        && reel_state(s.rod_state)
    {
        return format!("Reeling {}", percent(r.progress));
    }
    if s.shake_button_x.is_some() {
        return "Shaking".into();
    }
    if s.rod.is_empty() {
        return "No rod in hand".into();
    }
    if s.rod_state == Some(ROD_LURE) {
        return "Waiting for a bite".into();
    }
    if line_out(s.rod_state) {
        return "Casting".into();
    }
    (if s.rod_state == Some(ROD_IDLE) {
        "Idle"
    } else {
        "Waiting"
    })
    .into()
}

pub const REEL_GUI_WARNING: &str = "reel GUI not found (Roblox update?)";

/// The line is out and reeling (state 8) yet the reel GUI read found nothing: only a moved GUI explains it.
pub fn reel_gui_missing(s: Option<&FishState>) -> bool {
    s.is_some_and(|s| s.rod_state == Some(ROD_REELING) && s.reel.is_none())
}

/// The game's cast rating from cast_feedback(): true for "PERFECT!", false for any other ("Meh.."), None when the
/// label isn't a cast's (empty, or the catch's "PERFECT CATCH!" still showing).
pub fn cast_perfect(feedback: &str) -> Option<bool> {
    if feedback.is_empty() || feedback.ends_with("CATCH!") {
        return None;
    }
    Some(feedback.starts_with("PERFECT"))
}

/// Whether a finished reel landed the fish, from the last progress read. Lost reels feed stuck_reason.
pub fn reel_won(last_progress: f32) -> bool {
    last_progress > 0.9
}

/// Whether a catch waiting for its "+45xp" popup can be logged, `age` after its catch popup: the XP shows a moment
/// later, so wait 400 ms, then until it's read, or give up at 1.5 s (some catches never show one).
pub fn catch_settled(age: Duration, xp_shown: bool) -> bool {
    age >= MS(1500) || (age >= MS(400) && xp_shown)
}

/// Why the macro should stop, or None.
pub fn stuck_reason(lost_streak: i32) -> Option<String> {
    (lost_streak >= 5).then(|| format!("{lost_streak} fish lost in a row"))
}

/// How long to wait before the next cast after `tries` casts left the rod idle: quick retries first, then longer
/// waits so an AFK session outlasts a popup over the cursor without clicking it often.
pub fn cast_retry_wait(tries: i32) -> Duration {
    if tries < 3 {
        return MS(400); // the rod leaves idle ~0.1 s after release
    }
    Duration::from_secs(match tries {
        3 => 5,
        4 => 30,
        _ => 60,
    })
}

/// One tick of a shake: '\' once (Roblox UI Navigation selects the button, nav remembers it), then Enter. Focus is
/// asked again before each key: they are ~30 ms apart and the focus flag can lag an alt-tab, so a stray Enter could
/// send a message typed in another window.
pub fn shake_keys(nav: &mut bool, focused: &mut dyn FnMut() -> bool, tap: &mut dyn FnMut(&Key)) {
    if !*nav {
        if !focused() {
            return;
        }
        tap(&BACKSLASH);
        *nav = true;
    }
    if focused() {
        tap(&ENTER);
    }
}

/// One reel's recording (state 8), times in s since it started: bar X samples.
#[derive(Clone, Debug, Default)]
pub struct ReelLog {
    pub bar: Vec<(f32, f32)>,
    /// same timestamps as bar
    pub fish: Vec<(f32, f32)>,
    pub width: f32,
}

/// How well one reel (state 8) was played, for reels.csv.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReelStats {
    pub seconds: f32,
    /// fish-to-bar distance
    pub mean_dist: f32,
    /// share of time the fish was inside the bar
    pub inside_frac: f32,
    /// times the bar hit a wall faster than 0.1/s
    pub wall_hits: i32,
}

pub fn reel_stats(log: &ReelLog) -> ReelStats {
    let mut r = ReelStats::default();
    let (f, b) = (&log.fish, &log.bar);
    if f.len() < 2 || f.len() != b.len() {
        return r;
    }
    let half = log.width / 2.0;
    r.seconds = f[f.len() - 1].0 - f[0].0;
    let (mut dist, mut inside) = (0.0f32, 0);
    let at_wall = |x: f32| x <= half + 1e-3 || x >= 1.0 - half - 1e-3;
    for i in 0..f.len() {
        let d = (f[i].1 - b[i].1).abs();
        dist += d;
        inside += i32::from(d <= half);
        if i == 0 || !at_wall(b[i].1) || at_wall(b[i - 1].1) {
            continue;
        }
        let mut j = i; // arrived at a wall: how fast, over the last ~30 ms
        while j > 0 && b[i].0 - b[j].0 < 0.03 {
            j -= 1;
        }
        if b[i].0 > b[j].0 && (b[i].1 - b[j].1).abs() / (b[i].0 - b[j].0) > 0.1 {
            r.wall_hits += 1;
        }
    }
    r.mean_dist = dist / f.len() as f32;
    r.inside_frac = inside as f32 / f.len() as f32;
    r
}

/// The local time's year, month, day, h, m, s.
fn local_time() -> [u16; 6] {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    [t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond]
}

fn stamp() -> String {
    let [y, mo, d, h, mi, s] = local_time();
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// CSV field
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn joined(v: &[String]) -> String {
    v.join("+")
}

const CATCH_HEADER: &str =
    "time,size,mutations,species,kg,odds,rarity,location,value,xp,resilience,progress_speed,popup";

/// A catch saved with its popup text, icon and tint, as it was shown before the restart.
fn restored(text: &str, icon: String, tint: Option<u32>) -> Option<Catch> {
    parse_catch(text).map(|c| Catch { icon, tint, ..c })
}

fn catch_line(text: &str, c: &Option<Catch>) -> String {
    let field = |f: fn(&Catch) -> String| c.as_ref().map(f).unwrap_or_default();
    format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{}",
        stamp(),
        quote(&field(|c| c.size.clone())),
        quote(&field(|c| joined(&c.mutations))),
        quote(&field(|c| c.species.clone())),
        field(|c| if c.kg > 0.0 { kg_text(c.kg) } else { String::new() }),
        field(|c| c.odds.to_string()),
        quote(&field(|c| c.rarity.clone())),
        quote(&field(|c| c.location.clone())),
        field(|c| c.value.map(|v| (v as i64).to_string()).unwrap_or_default()),
        field(|c| c.xp.map(|x| x.to_string()).unwrap_or_default()),
        field(|c| c.resilience.clone()),
        field(|c| c.progress_speed.clone()),
        quote(text)
    )
}

/// Appends lines to a CSV log; a file with another header (an older column layout) is first kept under a timestamped
/// name. False, writing nothing, when the file can't be written (open in Excel, say): try the lines again later.
pub fn append_csv(log: &Path, header: &str, lines: &[String]) -> bool {
    if log.exists() {
        let mut first = Vec::new();
        if let Ok(f) = std::fs::File::open(log) {
            let _ = BufReader::new(f).read_until(b'\n', &mut first);
        }
        if first.last() == Some(&b'\n') {
            first.pop();
        }
        if first != header.as_bytes() {
            let [y, mo, d, h, mi, s] = local_time();
            let stem = log
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kept = log.with_file_name(format!("{stem}-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}.csv"));
            if std::fs::rename(log, kept).is_err() {
                return false; // locked: appending would mix two column layouts
            }
        }
    }
    let fresh = !log.exists();
    let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(log) else {
        return false;
    };
    let mut out = String::new();
    if fresh {
        out += header;
        out.push('\n');
    }
    for line in lines {
        out += line;
        out.push('\n');
    }
    f.write_all(out.as_bytes()).and_then(|_| f.flush()).is_ok()
}

/// The bot loop's input for SafetyGuard::tick.
pub struct Tick<'a> {
    pub now: Instant,
    /// auto fish
    pub on: bool,
    /// Roblox is the foreground window
    pub focused: bool,
    pub quit_requested: bool,
    /// the hook held back a '/'
    pub slash_held: bool,
    /// None: not in game
    pub s: Option<&'a FishState>,
}

#[derive(Clone, Debug, Default)]
pub struct Actions {
    /// auto fish after this tick
    pub on: bool,
    /// turned on since the last tick
    pub just_on: bool,
    /// why this tick turned it off (empty for a quit)
    pub stop_reason: String,
    /// tap the rod's hotbar key now: it reels the line in
    pub unequip: bool,
    /// the hook holds back '/' (the line is out)
    pub hold_slash: bool,
    /// send the held-back '/' now (if Roblox is focused) and stop holding it
    pub slash_due: bool,
    /// run the fishing step this tick
    pub fish: bool,
}

/// The bot loop's rules for chat, closing and turning off with the line out, apart from the keys they send so they can
/// be tested. One tick() per loop, after the session and popup checks.
#[derive(Default)]
pub struct SafetyGuard {
    was_on: bool,
    unequip_pending: bool,
    rod_char: usize, // character the rod was last seen on while on (0: not since it turned on)
    rod_gone_at: Option<Instant>, // when the rod left that character
    slash_at: Option<Instant>, // when to send the held-back '/'
    unequip_at: Option<Instant>, // turned off mid-reel: when to (re)try the unequip once the reel is over
    unequip_tries: u8,
}

impl SafetyGuard {
    pub fn tick(&mut self, t: &Tick) -> Actions {
        let s = t.s;
        let out = s.is_some_and(|s| line_out(s.rod_state));
        let typing = s.is_some_and(|s| s.typing);
        let mut a = Actions {
            on: t.on,
            just_on: t.on && !self.was_on, // F1, dashboard
            ..Default::default()
        };
        // Turned off: no more input for this fish. With the line out the rod's key brings the line in now; mid-reel the
        // reel is let go (the key does nothing then) and the rod is unequipped once the reel is over. Back on: as usual.
        let reeling = s.is_some_and(|s| reel_state(s.rod_state));
        if !t.on && self.was_on && out {
            self.unequip_pending = true;
        }
        if !t.on && self.was_on && reeling {
            self.unequip_at = Some(t.now);
            self.unequip_tries = 0;
        }
        if t.on {
            self.unequip_at = None;
        }
        self.was_on = t.on;
        if t.quit_requested && a.on && !s.is_some_and(|s| reel_state(s.rod_state)) {
            // never cast again; a reel in play finishes first
            a.on = false;
            self.was_on = false; // don't queue the unequip twice
            self.unequip_pending = out;
        }
        // You unequipped the rod or switched to another tool: off. Gone for 300 ms, so a misread can't trigger it; a
        // new character (death, respawn) isn't you unequipping, so that re-equips as before.
        if let Some(s) = s.filter(|s| a.on && !s.rod.is_empty()) {
            self.rod_char = s.character;
            self.rod_gone_at = None;
        } else if a.on && self.rod_char != 0 && s.is_some_and(|s| self.rod_char == s.character) {
            match self.rod_gone_at {
                None => self.rod_gone_at = Some(t.now),
                Some(at) if t.now - at >= MS(300) => {
                    a.on = false;
                    self.was_on = false;
                    a.stop_reason = "rod unequipped".into();
                }
                Some(_) => {}
            }
        }
        if !a.on {
            self.rod_char = 0;
            self.rod_gone_at = None;
        }
        // Chat with the line out: unequip first. '/' (held back by the hook) is sent once the unequip has gone through;
        // a chat opened by clicking the bar can't be got ahead of, so that unequips as soon as chat closes. Either way
        // auto fish turns off (was_on: the unequip is handled here, not queued again by the off transition).
        let line_out_now = a.on && t.focused && out;
        a.hold_slash = line_out_now && !typing;
        if t.slash_held && self.slash_at.is_none() {
            if line_out_now && !typing {
                a.unequip = true;
                a.on = false;
                self.was_on = false;
                a.stop_reason = "chat opened".into();
            }
            self.slash_at = Some(t.now + MS(50));
        }
        if self.slash_at.is_some_and(|at| t.now >= at) {
            a.slash_due = true;
            self.slash_at = None;
        }
        if line_out_now && typing {
            a.on = false;
            self.was_on = false;
            self.unequip_pending = true;
            a.stop_reason = "chat opened".into();
        }
        // rod_gone_at: don't re-equip
        a.fish = a.on
            && t.focused
            && s.is_some()
            && !typing
            && self.slash_at.is_none()
            && !a.unequip
            && self.rod_gone_at.is_none();
        if !a.fish {
            if a.on || !out {
                self.unequip_pending = false; // back on, or the line is no longer out
            }
            if let Some(at) = self.unequip_at {
                if s.is_some_and(|s| s.rod.is_empty()) {
                    self.unequip_at = None; // gone: done
                } else if t.now >= at && t.focused && !typing && !out && !reeling && s.is_some_and(|s| s.reel.is_none())
                {
                    // The game ignores the rod's key until the reel bar is gone, ~0.6 s after the rod turns idle (seen
                    // live). Still in hand after a press: press again, but only after the last one had time to show (a
                    // rod goes within ~0.2 s of an accepted press), or a second press would equip it again.
                    a.unequip = true;
                    self.unequip_tries += 1;
                    self.unequip_at = (self.unequip_tries < 3).then(|| t.now + MS(600));
                }
            }
            if self.unequip_pending && t.focused && !typing {
                // waits for Roblox focus: the dashboard checkbox takes it away
                a.unequip = true;
                self.unequip_pending = false;
            }
        }
        a
    }

    /// turned off with the line out, waiting for focus
    pub fn unequip_pending(&self) -> bool {
        self.unequip_pending
    }
}

/// Every key the bot sends, up unconditionally (a key-up for a key that isn't down does nothing).
/// For crash/exit paths; a hard kill (Task Manager) runs no code, so it can't be covered.
pub fn release_inputs() {
    for k in [SPACE, MOUSE_LEFT, BACKSLASH, ENTER, SLASH] {
        release(&k);
    }
    for d in 0..10 {
        release(&digit_key(d)); // hotbar
    }
}

#[derive(Clone, Debug, Default)]
pub struct View {
    pub rod: String,
    pub phase: String,
    /// non-empty when a read that should work keeps failing (a Roblox update moved something)
    pub warning: String,
    /// the game's popup text
    pub last_catch: String,
    pub rarest: String,
    pub coins: String,
    /// level_pieces; empty until read
    pub level: Vec<Piece>,
    /// the same popups parsed; None when they don't
    pub last_fish: Option<Catch>,
    /// last_fish's 3D model from its popup, when it could be read; saved (last_model.bin) like the rarest's
    pub last_model: Option<Arc<FishModel>>,
    pub rarest_fish: Option<Catch>,
    /// rarest_fish's 3D model; saved next to the catch log (rarest_model.bin) so it survives a restart
    pub rarest_model: Option<Arc<FishModel>>,
    pub player: PlayerInfo,
    /// studs/s between the last two position reads (the GPS's "s/s")
    pub speed: Option<f32>,
    /// why the bot last turned auto fish off itself; empty once it's turned on again
    pub stopped: String,
    /// the bot's last cast: power bar when it let go, then the game's rating ("97% · PERFECT!")
    pub last_cast: String,
    /// this session's catches per rarity, in first-caught order (unknown fish left out)
    pub rarities: Vec<(String, i32)>,
    /// this session's catches with any mutation besides Shiny / Sparkling, and with those
    pub mutated: i32,
    pub shiny: i32,
    pub sparkling: i32,
    /// ReplicatedStorage/world, read about once a second
    pub world: World,
}

impl View {
    fn tally(&mut self, c: &Catch) {
        if !c.rarity.is_empty() {
            match self.rarities.iter_mut().find(|r| r.0 == c.rarity) {
                Some(r) => r.1 += 1,
                None => self.rarities.push((c.rarity.clone(), 1)),
            }
        }
        let has = |m: &str| c.mutations.iter().any(|x| x == m);
        self.shiny += i32::from(has("Shiny"));
        self.sparkling += i32::from(has("Sparkling"));
        self.mutated += i32::from(c.mutations.iter().any(|m| m != "Shiny" && m != "Sparkling"));
    }
}

/// A catch's popup still to read: its text, when it showed, its address, and for a second look the first look's time
/// and a part's color then.
type PendingModel = (String, Instant, usize, Option<(Instant, u32)>);

/// What still to read of a catch's popup after its first look: its model while it isn't there yet, else a second look
/// for a turning hue (when a part has a hue to tell); None when neither.
fn model_followup(
    text: &str,
    at: Instant,
    popup: usize,
    look: Option<&CatchLook>,
    now: Instant,
) -> Option<PendingModel> {
    let look = look?;
    if look.model.is_empty() {
        return Some((text.into(), at, popup, None));
    }
    hue_reference(&look.model).map(|c| (text.into(), at, popup, Some((now, c))))
}

/// The first part color with a telling hue: the same part in every look at one popup (they read the model alike).
fn hue_reference(tris: &[handy_sdk::ui::raster::Tri]) -> Option<u32> {
    tris.iter().map(|t| t.rgb).find(|c| hue(*c).is_some())
}

/// Degrees a second the hue turned from `a` to `b` in `secs` (the short way round); 0 below a few degrees a second,
/// as a still color's rounding gives.
fn hue_rate(a: u32, b: u32, secs: f32) -> f32 {
    let (Some(ha), Some(hb)) = (hue(a), hue(b)) else {
        return 0.0;
    };
    let turned = (hb - ha + 540.0).rem_euclid(360.0) - 180.0;
    let rate = turned / secs.max(0.1);
    if rate.abs() < 3.0 { 0.0 } else { rate }
}

/// A session catch for Earned: its rarity, estimated value, and whether auto fish was on (the /h rates count those).
#[derive(Clone, Debug, PartialEq)]
pub struct SoldCatch {
    pub rarity: String,
    pub value: i64,
    pub while_on: bool,
}

/// The rarities the game's Sell All has a switch for (Settings/willautosell_<lowercase>); the rest are always sold.
pub const RARITY_SWITCHES: [&str; 11] = [
    "Legendary",
    "Mythical",
    "Exotic",
    "Secret",
    "Relic",
    "Gemstone",
    "Extinct",
    "Special",
    "Event",
    "Apex",
    "Cataclysmic",
];

/// Earned over `catches` under `filter` (None: every catch): (value, its part caught while auto fish was on, catches
/// kept).
pub fn earned(catches: &[SoldCatch], filter: Option<&HashMap<String, bool>>) -> (i64, i64, i32) {
    let mut out = (0, 0, 0);
    for c in catches {
        if filter.is_some_and(|f| !sells(f, &c.rarity)) {
            out.2 += 1;
        } else {
            out.0 += c.value;
            out.1 += if c.while_on { c.value } else { 0 };
        }
    }
    out
}

/// The user's own filter (Settings::sold_rarities) in the game's form: every switchable rarity, sold when listed.
pub fn my_filter(sold: &[String]) -> HashMap<String, bool> {
    RARITY_SWITCHES
        .iter()
        .map(|r| {
            let key = r.to_lowercase();
            let on = sold.contains(&key);
            (key, on)
        })
        .collect()
}

/// Whether the game's Sell All sells a catch of `rarity` under `filter` (sell_filter): the rarity's own switch, else
/// sold (the lower rarities have none).
// shortcut: Divine Secret and Limited have no switch of their own; they follow Secret and Event, a guess from the names
fn sells(filter: &HashMap<String, bool>, rarity: &str) -> bool {
    let key: String = rarity
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    let key = if key.contains("secret") {
        "secret"
    } else if key.starts_with("limited") {
        "event"
    } else {
        key.as_str()
    };
    filter.get(key).copied().unwrap_or(true)
}

fn rarest_model_file(catch_log: &Path) -> PathBuf {
    catch_log.with_file_name("rarest_model.bin")
}

fn last_model_file(catch_log: &Path) -> PathBuf {
    catch_log.with_file_name("last_model.bin")
}

/// Gives the last and the rarest catch the model when `text` is that catch; whether each took it (to save then).
fn set_models(v: &mut View, text: &str, model: &Arc<FishModel>) -> (bool, bool) {
    let last = v.last_catch == text;
    if last {
        v.last_model = Some(model.clone());
    }
    let rarest = v.rarest == text;
    if rarest {
        v.rarest_model = Some(model.clone());
    }
    (last, rarest)
}

/// Cast -> shake -> reel loop. Input is sent only while Roblox is the foreground window.
/// The cursor is never moved.
///   cast:  left mouse hold where the cursor is (only over Roblox; a press over any HUD element is dropped)
///   shake: UI Navigation ('\') + Enter
///   reel:  Space hold (a mouse hold loses every 2nd quick press to WM_LBUTTONDBLCLK)
pub struct Bot {
    updater: Arc<Updater>,
    settings: Arc<SettingsStore>,
    roblox_foreground: Arc<AtomicBool>,
    watcher: RobloxWatcher,
    config_ini: PathBuf,
    catch_log: PathBuf,
    /// toggled by the hotkey (while Roblox is focused) or the dashboard
    pub on: AtomicBool,
    pub caught: AtomicI32,
    pub lost: AtomicI32,
    /// catches in a row without a lost fish (best: this session)
    pub streak: AtomicI32,
    pub best_streak: AtomicI32,
    /// catches made while auto fish was on: the /h rate uses these
    pub macro_caught: AtomicI32,
    /// this session's catches as Earned counts them (earned()), and the game's Sell All filters once read
    pub sold: Mutex<Vec<SoldCatch>>,
    pub game_filter: Mutex<Option<HashMap<String, bool>>>,
    /// this session's XP, and the part gained while auto fish was on (for the rate)
    pub xp_gained: AtomicI64,
    pub macro_xp: AtomicI64,
    /// time spent fishing (on, focused, in game, not typing)
    pub active_ms: AtomicI64,
    /// this session's casts the game rated, and its "PERFECT!"s
    pub casts: AtomicI32,
    pub perfect_casts: AtomicI32,
    /// reels the bot played to a catch, and those that never lost progress (the game's perfect catch: +50% XP)
    pub won_reels: AtomicI32,
    pub perfect_catches: AtomicI32,
    /// CSV lines a locked file refused, still in memory
    pub unsaved_rows: AtomicI32,
    /// config.ini didn't read back what was just saved
    pub settings_unsaved: AtomicBool,
    /// Opening chat with the line out: the keyboard hook holds '/' back while hold_slash, and sets slash_held; the bot
    /// unequips (a hotbar key pressed in chat would be typed), then sends '/' itself.
    pub hold_slash: AtomicBool,
    pub slash_held: AtomicBool,
    /// Graceful close: finish the reel being played, unequip if the line is out, then set ready_to_quit.
    pub quit_requested: AtomicBool,
    pub ready_to_quit: AtomicBool,
    view: Mutex<View>,
}

struct Unwritten {
    header: String,
    lines: Vec<String>,
}

struct Sample {
    at: Instant,
    bar: f32,
    fish: f32,
    progress: f32,
}

/// Bot::run's own state, on the bot thread only.
struct Engine<'a> {
    bot: &'a Bot,
    toggle: KeyPoller,
    rarest_odds: i32,
    held: bool,
    nav: bool,
    reeling: bool,
    guard: SafetyGuard,
    unwritten: BTreeMap<PathBuf, Unwritten>, // CSV lines a locked file refused, per file
    cast_tries: i32,                         // casts sent while the rod stayed idle
    lost_streak: i32,
    stop_reason: String, // why the watchdog turned the macro off
    reel_log: ReelLog,
    trace: String, // this reel's rows for reels\<time>.csv
    reel_t0: Instant,
    reel_rod: String, // rod used for the reel being recorded
    last_progress: f32,
    reel_dropped: bool, // progress went down this reel: not a perfect catch
    prev: Option<Sample>,
    bar_v: f32, // per s, smoothed
    fish_v: f32,
    progress_v: f32, // the reel bar's fill
    cast_after: Instant,
    equip_after: Instant,
    rating_by: Option<Instant>, // after a cast: look for the game's rating until then
}

impl Bot {
    /// watcher: polled by run() (this thread only) for chat focus.
    /// config_ini: where learned settings (rod) are saved as soon as they change.
    /// catch_log: CSV each catch is appended to (columns: CATCH_HEADER).
    pub fn new(
        updater: Arc<Updater>,
        settings: Arc<SettingsStore>,
        roblox_foreground: Arc<AtomicBool>,
        watcher: RobloxWatcher,
        config_ini: PathBuf,
        catch_log: PathBuf,
    ) -> Self {
        Self {
            updater,
            settings,
            roblox_foreground,
            watcher,
            config_ini,
            catch_log,
            on: AtomicBool::new(false),
            caught: AtomicI32::new(0),
            lost: AtomicI32::new(0),
            streak: AtomicI32::new(0),
            best_streak: AtomicI32::new(0),
            macro_caught: AtomicI32::new(0),
            sold: Mutex::new(Vec::new()),
            game_filter: Mutex::new(None),
            xp_gained: AtomicI64::new(0),
            macro_xp: AtomicI64::new(0),
            active_ms: AtomicI64::new(0),
            casts: AtomicI32::new(0),
            perfect_casts: AtomicI32::new(0),
            won_reels: AtomicI32::new(0),
            perfect_catches: AtomicI32::new(0),
            unsaved_rows: AtomicI32::new(0),
            settings_unsaved: AtomicBool::new(false),
            hold_slash: AtomicBool::new(false),
            slash_held: AtomicBool::new(false),
            quit_requested: AtomicBool::new(false),
            ready_to_quit: AtomicBool::new(false),
            view: Mutex::new(View::default()),
        }
    }

    pub fn view(&self) -> View {
        self.view.lock().unwrap().clone()
    }

    /// update + save to config.ini now
    pub fn persist(&self, change: impl FnOnce(&mut Settings)) {
        self.settings.update(change);
        self.settings.save(&self.config_ini);
        // the profile API's writes can fail silently
        self.settings_unsaved
            .store(load_settings(&self.config_ini) != self.settings.get(), SeqCst);
    }

    pub fn run(&self, stop: &AtomicBool) {
        // wakes on time for its 1 ms waits under load
        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
        let now = Instant::now();
        Engine {
            bot: self,
            toggle: KeyPoller::default(),
            rarest_odds: 0,
            held: false,
            nav: false,
            reeling: false,
            guard: SafetyGuard::default(),
            unwritten: BTreeMap::new(),
            cast_tries: 0,
            lost_streak: 0,
            stop_reason: String::new(),
            reel_log: ReelLog::default(),
            trace: String::new(),
            reel_t0: now,
            reel_rod: String::new(),
            last_progress: 0.0,
            reel_dropped: false,
            prev: None,
            bar_v: 0.0,
            fish_v: 0.0,
            progress_v: 0.0,
            cast_after: now,
            equip_after: now,
            rating_by: None,
        }
        .run(stop);
    }
}

impl Engine<'_> {
    fn hold(&mut self, down: bool) {
        if down == self.held {
            return;
        }
        if down {
            press(&SPACE);
        } else {
            release(&SPACE);
        }
        self.held = down;
    }

    /// toggle key edge (only while Roblox is focused)
    fn poll_toggle(&mut self, toggle_vk: u32) {
        let down = self.bot.roblox_foreground.load(Relaxed) && key_down(toggle_vk as u16);
        if self.toggle.step(down) {
            self.bot.on.store(!self.bot.on.load(SeqCst), SeqCst);
        }
    }

    fn end_reel(&mut self) {
        self.reeling = false;
        let won = reel_won(self.last_progress);
        if won {
            self.bot.won_reels.fetch_add(1, SeqCst);
            if !self.reel_dropped {
                self.bot.perfect_catches.fetch_add(1, SeqCst);
            }
        } else {
            // catches are counted from the popup (count_catch)
            self.bot.lost.fetch_add(1, SeqCst);
            self.lost_streak += 1;
            self.bot.streak.store(0, SeqCst);
        }
        if self.reel_log.fish.len() >= 50 {
            self.write_reel(&reel_stats(&self.reel_log), won);
            self.write_trace(won);
        }
        self.reel_log = ReelLog::default();
        self.trace.clear();
    }

    /// The whole reel, one row per read, to reels\<time>-<outcome>-w<bar width>.csv: what the reel control is tuned
    /// from. Best effort: a failed write loses only this trace.
    // shortcut: every reel is kept (~130 KB each), delete the folder by hand; cap it if it stays on after tuning
    fn write_trace(&self, won: bool) {
        let dir = self.bot.catch_log.parent().unwrap_or(Path::new("")).join("reels");
        let _ = std::fs::create_dir_all(&dir);
        let name = format!(
            "{}-{}-w{:.2}.csv",
            stamp().replace(':', "").replace(' ', "-"),
            if won { "caught" } else { "lost" },
            self.reel_log.width
        );
        let _ = std::fs::write(
            dir.join(name),
            format!(
                "t,bar,fish,progress,held
{}",
                self.trace
            ),
        );
    }

    fn set_view(&self, rod: &str, phase: &str) {
        let mut v = self.bot.view.lock().unwrap();
        v.rod = rod.into();
        v.phase = phase.into();
        v.stopped = self.stop_reason.clone();
    }

    /// append_csv, keeping the line (and any before it) when the file is locked; they go out with the next line.
    fn append_log(&mut self, csv: &Path, header: &str, line: String) {
        let u = self.unwritten.entry(csv.to_path_buf()).or_insert_with(|| Unwritten {
            header: String::new(),
            lines: Vec::new(),
        });
        header.clone_into(&mut u.header);
        u.lines.push(line);
        if append_csv(csv, &u.header, &u.lines) {
            u.lines.clear();
        }
        let rows: usize = self.unwritten.values().map(|w| w.lines.len()).sum();
        self.bot.unsaved_rows.store(rows as i32, SeqCst);
    }

    /// appends to reels.csv
    fn write_reel(&mut self, r: &ReelStats, won: bool) {
        let line = format!(
            "{},{},{:.2},{},{:.2},{:.4},{:.3},{}",
            stamp(),
            quote(&self.reel_rod),
            self.reel_log.width,
            if won { "caught" } else { "lost" },
            r.seconds,
            r.mean_dist,
            r.inside_frac,
            r.wall_hits
        );
        let csv = self.bot.catch_log.parent().unwrap_or(Path::new("")).join("reels.csv");
        self.append_log(
            &csv,
            "time,rod,bar_width,outcome,seconds,mean_dist,inside,wall_hits",
            line,
        );
    }

    /// on each catch popup: counts, view
    fn count_catch(&mut self, text: &str) {
        let bot = self.bot;
        // The popup is read whether the macro is on or not, so a reel finished after F1 (or by hand) still counts.
        bot.caught.fetch_add(1, SeqCst);
        if bot.on.load(SeqCst) {
            bot.macro_caught.fetch_add(1, SeqCst);
        }
        let streak = bot.streak.fetch_add(1, SeqCst) + 1;
        bot.best_streak.store(bot.best_streak.load(SeqCst).max(streak), SeqCst);
        self.lost_streak = 0;
        let c = parse_catch(text);
        let value = c.as_ref().and_then(|c| c.value).map_or(0, |v| v as i64);
        bot.sold.lock().unwrap().push(SoldCatch {
            rarity: c.as_ref().map(|c| c.rarity.clone()).unwrap_or_default(),
            value,
            while_on: bot.on.load(SeqCst),
        });
        let mut new_rarest = false;
        {
            let mut v = bot.view.lock().unwrap();
            v.last_catch = text.into();
            v.last_fish = c.clone();
            if let Some(c) = &c {
                v.tally(c);
            }
            v.last_model = None;
            if let Some(f) = c.as_ref().filter(|f| f.odds > self.rarest_odds) {
                self.rarest_odds = f.odds;
                v.rarest = text.into();
                v.rarest_fish = c.clone();
                v.rarest_model = None; // the file's text no longer matches, so it is ignored at startup
                new_rarest = true;
            }
        }
        bot.persist(|s| {
            s.last_catch = text.into();
            (s.last_icon, s.last_tint) = (String::new(), None); // until finish_catch has read the popup
            if new_rarest {
                s.rarest = text.into();
                (s.rarest_icon, s.rarest_tint) = (String::new(), None);
            }
        });
    }

    /// The popup's model for the catch `text`: shown on its cards and saved for each (so they survive a restart).
    fn keep_model(&self, text: &str, model: Arc<FishModel>) {
        let (last, rarest) = set_models(&mut self.bot.view.lock().unwrap(), text, &model);
        let log = &self.bot.catch_log;
        for (took, file) in [(last, last_model_file(log)), (rarest, rarest_model_file(log))] {
            if took {
                rarest_model::save(&file, text, &model);
            }
        }
    }

    /// ~400 ms later, with the XP popup: XP, log
    fn finish_catch(&mut self, text: &str, xp: Option<i64>, look: Option<CatchLook>) {
        let mut c = parse_catch(text);
        let mut model = None;
        if let Some(look) = look {
            if !look.model.is_empty() {
                model = Some(Arc::new(FishModel {
                    tris: look.model,
                    scene: look.scene,
                    textures: look.textures,
                    hue_rate: 0.0, // measured by a second look (the model loop)
                }));
            }
            if let Some(c) = &mut c {
                c.icon = look.icon.unwrap_or_default();
                c.tint = look.tint;
            }
        }
        if let Some(c) = &mut c {
            c.xp = xp;
        }
        self.bot.xp_gained.fetch_add(xp.unwrap_or(0), SeqCst);
        if self.bot.on.load(SeqCst) {
            self.bot.macro_xp.fetch_add(xp.unwrap_or(0), SeqCst);
        }
        let (mut is_last, mut is_rarest) = (false, false);
        {
            let mut v = self.bot.view.lock().unwrap();
            if v.last_catch == text {
                v.last_fish.clone_from(&c); // now with its XP and icon; a newer catch already replaced it otherwise
                is_last = true;
            }
            if v.rarest == text {
                v.rarest_fish.clone_from(&c);
                is_rarest = true;
            }
        }
        if let Some(model) = model {
            self.keep_model(text, model);
        }
        if let Some(c) = c.as_ref().filter(|_| is_last || is_rarest) {
            self.bot.persist(|s| {
                if is_last {
                    (s.last_icon, s.last_tint) = (c.icon.clone(), c.tint);
                }
                if is_rarest {
                    (s.rarest_icon, s.rarest_tint) = (c.icon.clone(), c.tint);
                }
            });
        }
        let log = self.bot.catch_log.clone();
        self.append_log(&log, CATCH_HEADER, catch_line(text, &c));
    }

    fn step(&mut self, s: &FishState, cfg: &Settings, pid: u32, reader: &mut GameReader) {
        let bot = self.bot;
        let now = Instant::now();
        if s.shake_button_x.is_none() {
            self.nav = false; // the shake button is gone, and the game's selection with it
        }
        if self.rating_by.is_some_and(|by| now > by) {
            self.rating_by = None;
        }
        if self.rating_by.is_some()
            && let Some(t) = reader.cast_feedback()
            && let Some(perfect) = cast_perfect(&t)
        {
            // ~0.1 s after the release
            bot.casts.fetch_add(1, SeqCst);
            bot.perfect_casts.fetch_add(i32::from(perfect), SeqCst);
            self.rating_by = None;
            bot.view.lock().unwrap().last_cast += &format!(" · {t}");
        }

        if let Some(r) = &s.reel
            && reel_state(s.rod_state)
        {
            if !self.reeling {
                self.prev = None;
                (self.bar_v, self.fish_v, self.progress_v) = (0.0, 0.0, 0.0);
                self.reel_log = ReelLog::default();
                self.trace.clear();
                self.reel_dropped = false;
            } else if r.progress < self.last_progress - 1e-3 {
                self.reel_dropped = true;
            }
            self.reeling = true;
            self.last_progress = r.progress;
            if s.rod_state == Some(ROD_REELING) {
                // record for reels.csv; the bar ignores input before state 8
                if self.reel_log.bar.is_empty() {
                    self.reel_t0 = now;
                    self.reel_log.width = r.bar_width;
                    s.rod.clone_into(&mut self.reel_rod);
                }
                let t = (now - self.reel_t0).as_secs_f32();
                self.reel_log.bar.push((t, r.bar));
                self.reel_log.fish.push((t, r.fish));
                // held: Space as decided on the last read, so the key's state over the time since then
                self.trace += &format!(
                    "{t:.4},{:.5},{:.5},{:.4},{}
",
                    r.bar,
                    r.fish,
                    r.progress,
                    u8::from(self.held)
                );
            }
            if let Some(prev) = &self.prev {
                let dt = (now - prev.at).as_secs_f32();
                if dt > 1e-4 {
                    // smoothed over time, not samples: the bar moves once per frame, we read every ~1 ms
                    let ease = |v: &mut f32, target: f32, tau: f32| *v += (1.0 - (-dt / tau).exp()) * (target - *v);
                    // 10 and 22 ms: the per-sample 0.4 / 0.2 the reel leads were tuned with at 5 ms reads
                    ease(&mut self.bar_v, (r.bar - prev.bar) / dt, 0.010);
                    ease(&mut self.fish_v, (r.fish - prev.fish) / dt, 0.022);
                    // over ~0.5 s: the fill steps once per frame, and only its trend is worth reading
                    self.progress_v +=
                        (1.0 - (-dt / 0.5f32).exp()) * ((r.progress - prev.progress) / dt - self.progress_v);
                }
            }
            self.prev = Some(Sample {
                at: now,
                bar: r.bar,
                fish: r.fish,
                progress: r.progress,
            });
            // Only while the reel owns Space: during the lock and the ~1 s exit slide, Space jumps.
            // Let go just before 100% too, so Space is already up when the state flips to done.
            // Focus read live, as for every key here: the roblox_foreground flag can lag an alt-tab.
            let down = s.rod_state == Some(ROD_REELING)
                && r.progress < 0.98
                && reel_hold(r.bar, self.bar_v, r.fish, self.fish_v) != controls_inverted(&s.rod, r.progress)
                && foreground_is(pid);
            self.hold(down);
            self.set_view(&s.rod, &reeling_text(r.progress, self.progress_v, &r.progress_speed));
            return;
        }
        self.hold(false);
        if self.reeling {
            self.end_reel();
        }

        if s.shake_button_x.is_some() {
            shake_keys(&mut self.nav, &mut || foreground_is(pid), &mut |k| tap_key(k));
            self.set_view(&s.rod, "Shaking");
            return;
        }

        if !s.rod.is_empty() && s.rod != cfg.rod {
            bot.persist(|c| s.rod.clone_into(&mut c.rod)); // the rod to re-equip
        }
        if s.rod.is_empty() {
            if now < self.equip_after {
                return self.set_view("", "Equipping rod");
            }
            self.equip_after = now + Duration::from_secs(1); // the rod shows up in the character a moment after the key press
            let Some(k) = reader.rod_hotkey(&cfg.rod) else {
                return self.set_view("", "Put a fishing rod on the hotbar (keys 1-9)");
            };
            if foreground_is(pid) {
                tap_key(&digit_key(k));
            }
            return self.set_view("", "Equipping rod");
        }
        if s.rod_state != Some(ROD_IDLE) {
            self.cast_tries = 0;
        }
        if s.rod_state == Some(ROD_IDLE) && now >= self.cast_after {
            // A click lands on whatever window is under the cursor, so only cast when that is Roblox.
            if !roblox_ready(pid) {
                return self.set_view(&s.rod, "Cast paused: put the cursor over Roblox");
            }
            self.cast_tries += 1;
            self.set_view(&s.rod, "Casting");
            // Hold until the power bar starts filling (perfect cast: until it is full), then let go. The game ignores
            // the press during its post-catch cooldown, so holding through it casts the moment it ends.
            // 0.95: releases at 96-100% were rated "PERFECT!" (the bar fills in ~0.4 s, so 1 ms polls don't overshoot
            // it).
            let release_above = if cfg.perfect_cast { 0.95 } else { 0.0 };
            press(&MOUSE_LEFT); // button only, never moves the cursor
            let mut give_up = Instant::now() + Duration::from_secs(2);
            let mut filling = false;
            let mut power = 0.0;
            while Instant::now() < give_up && bot.on.load(SeqCst) && bot.roblox_foreground.load(Relaxed) {
                power = reader.cast_power().unwrap_or(0.0);
                if power > release_above {
                    break;
                }
                if power > 0.0 && !filling {
                    filling = true;
                    give_up = Instant::now() + Duration::from_secs(2); // time for the bar to fill
                }
                precise_wait(MS(1));
                self.poll_toggle(cfg.toggle_vk); // F1 during the hold turns the macro off and ends the loop
            }
            release(&MOUSE_LEFT);
            if power > 0.0 {
                self.rating_by = Some(Instant::now() + Duration::from_secs(1));
                bot.view.lock().unwrap().last_cast = percent(power);
            }
            self.cast_after = Instant::now() + cast_retry_wait(self.cast_tries); // retry if the rod is still idle by then
            return;
        }
        // Roblox drops a press over any HUD element, so a popup under the cursor shows up as casts that do nothing.
        self.set_view(
            &s.rod,
            if s.rod_state == Some(ROD_LURE) {
                "Waiting for a bite"
            } else if s.rod_state == Some(ROD_IDLE) && self.cast_tries >= 3 {
                "Cast not starting (cursor over the HUD?), retrying"
            } else {
                "Waiting"
            },
        );
    }

    fn run(&mut self, stop: &AtomicBool) {
        let bot = self.bot;
        let mut sess: Option<Arc<Session>> = None;
        let mut reader: Option<GameReader> = None;
        let mut prev_tick = Instant::now();
        let (mut next_popup_check, mut next_player_info, mut next_world) = (prev_tick, prev_tick, prev_tick);
        let mut pos_at = prev_tick; // when view.player.pos was read
        let mut fishing = Duration::ZERO; // active_ms before rounding
        {
            // the last and the all-time rarest catch, saved by count_catch
            let cfg = bot.settings.get();
            let mut v = bot.view.lock().unwrap();
            v.last_fish = restored(&cfg.last_catch, cfg.last_icon, cfg.last_tint);
            v.last_catch = cfg.last_catch;
            v.rarest_fish = restored(&cfg.rarest, cfg.rarest_icon, cfg.rarest_tint);
            v.rarest = cfg.rarest;
            v.rarest_model = rarest_model::load(&rarest_model_file(&bot.catch_log), &v.rarest).map(Arc::new);
            v.last_model = rarest_model::load(&last_model_file(&bot.catch_log), &v.last_catch).map(Arc::new);
            self.rarest_odds = v.rarest_fish.as_ref().map_or(0, |f| f.odds);
        }
        let mut pending_catch: Option<(String, Instant, usize)> = None; // popup text, when it was seen, the popup
        // A catch whose popup had no 3D model yet when it was read (the game fills it in a moment later, sometimes
        // after the XP shows), or whose model gets a second look for a turning hue.
        let mut pending_model: Option<PendingModel> = None;
        let mut next_model_try = Instant::now();
        let mut reel_missing = Sustained::default();
        let mut warned = false;
        while !stop.load(Relaxed) {
            let cfg = bot.settings.get();
            let fg = bot.roblox_foreground.load(Relaxed);
            self.poll_toggle(cfg.toggle_vk);

            let s_now = bot.updater.session();
            let same = match (&s_now, &sess) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (a, b) => a.is_none() && b.is_none(),
            };
            if !same {
                if s_now.is_none() && bot.on.load(SeqCst) {
                    // the process exited (a refresh swaps, never empties)
                    bot.on.store(false, SeqCst);
                    self.stop_reason = "Roblox closed".into();
                }
                drop(reader.take()); // it references the old session, so it goes first
                sess = s_now;
                reader = sess.clone().map(GameReader::new);
            }
            bot.watcher.poll();
            let mut s = reader.as_mut().and_then(|r| r.sample());
            if let Some(s) = &mut s {
                s.typing = bot.watcher.chat_focused().unwrap_or(true); // unreadable counts as typing: no keys
            }
            let now = Instant::now();
            let warn = reel_missing.update(reel_gui_missing(s.as_ref()), now, Duration::from_secs(2));
            if warn != warned {
                warned = warn;
                bot.view.lock().unwrap().warning = if warn { REEL_GUI_WARNING.into() } else { String::new() };
            }
            if let Some(r) = reader.as_mut()
                && now >= next_popup_check
            {
                // also catches fish you reel in by hand
                next_popup_check = now + MS(50); // the dashboard redraws every 50 ms: reading faster shows nothing sooner
                if let Some(c) = r.coins() {
                    bot.view.lock().unwrap().coins = c;
                }
                if bot.on.load(SeqCst) && r.error_prompt() {
                    bot.on.store(false, SeqCst);
                    self.stop_reason = "disconnected".into(); // don't click over the dialog
                }
                if let Some(lv) = r.level_progress() {
                    bot.view.lock().unwrap().level = level_pieces(lv.level, lv.xp);
                }
                if let Some((text, popup)) = r.take_catch_popup().filter(|(t, _)| !is_item_popup(t)) {
                    if let Some((t, at, prev)) = pending_catch.take() {
                        let xp = r.catch_xp();
                        let look = r.catch_look(prev);
                        pending_model = model_followup(&t, at, prev, look.as_ref(), now);
                        self.finish_catch(&t, xp, look);
                    }
                    self.count_catch(&text);
                    pending_catch = Some((text, now, popup));
                }
            }
            if let Some(r) = reader.as_mut()
                && !self.reeling
                && now >= next_player_info
            {
                // it walks the hotbar: never inside the timed reel loop
                // player_info() takes ~0.3 ms live (live_reader_costs), so every 100 ms costs the cast/shake loop ~0.3%.
                next_player_info = now + MS(100);
                if let Some(p) = r.player_info() {
                    let mut v = bot.view.lock().unwrap();
                    let secs = (now - pos_at).as_secs_f32();
                    v.speed = match (p.pos, v.player.pos) {
                        (Some(a), Some(b)) if secs > 0.0 => {
                            let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
                            Some((dx * dx + dy * dy + dz * dz).sqrt() / secs)
                        }
                        _ => None,
                    };
                    pos_at = now;
                    v.player = p;
                }
                if now >= next_world {
                    next_world = now + MS(1000); // ~40 reads; the weather changes with the day/night cycle
                    if let Some(w) = r.world() {
                        bot.view.lock().unwrap().world = w;
                    }
                    if let Some(f) = r.sell_filter() {
                        *bot.game_filter.lock().unwrap() = Some(f);
                    }
                }
            }
            if let Some((_, at, _)) = &pending_catch {
                let age = now - *at;
                let xp = if age >= MS(400) {
                    reader.as_mut().and_then(|r| r.catch_xp())
                } else {
                    None
                };
                if catch_settled(age, xp.is_some())
                    && let Some((text, at, popup)) = pending_catch.take()
                {
                    let look = reader.as_mut().and_then(|r| r.catch_look(popup));
                    pending_model = model_followup(&text, at, popup, look.as_ref(), now);
                    self.finish_catch(&text, xp, look);
                }
            }
            if let Some((text, at, popup, first)) = pending_model.clone()
                && !self.reeling
                && now >= next_model_try
            {
                next_model_try = now + MS(200); // an empty popup costs ~60 reads; the model's meshes a few hundred once
                let look = reader.as_mut().and_then(|r| r.catch_look(popup));
                match look {
                    Some(l) if !l.model.is_empty() => {
                        let color = hue_reference(&l.model);
                        let hue_rate = match (first, color) {
                            (Some((t0, c0)), Some(c1)) => hue_rate(c0, c1, (now - t0).as_secs_f32()),
                            _ => 0.0,
                        };
                        let model = Arc::new(FishModel {
                            tris: l.model,
                            scene: l.scene,
                            textures: l.textures,
                            hue_rate,
                        });
                        pending_model = match (first, color) {
                            // the model just arrived: look again later for a turning hue
                            (None, Some(c)) => Some((text.clone(), at, popup, Some((now, c)))),
                            _ => None,
                        };
                        if pending_model.is_some() {
                            next_model_try = now + MS(500);
                        }
                        if first.is_none() || hue_rate != 0.0 {
                            self.keep_model(&text, model);
                        }
                    }
                    // popup gone, or no model after 4 s (some popups show only the flat icon)
                    None => pending_model = None,
                    Some(_) if now - at > MS(4000) => pending_model = None,
                    Some(_) => {}
                }
            }
            let dt = now - prev_tick;
            prev_tick = now;
            let was_on = bot.on.load(SeqCst);
            let a = self.guard.tick(&Tick {
                now,
                on: was_on,
                focused: fg,
                quit_requested: bot.quit_requested.load(SeqCst),
                slash_held: bot.slash_held.load(SeqCst),
                s: s.as_ref(),
            });
            if a.just_on {
                self.lost_streak = 0;
                self.cast_tries = 0;
                self.stop_reason.clear();
            }
            if !a.stop_reason.is_empty() {
                self.stop_reason = a.stop_reason.clone();
            }
            if a.on != was_on {
                bot.on.store(a.on, SeqCst); // only on a change: the dashboard may have toggled it meanwhile
            }
            bot.hold_slash.store(a.hold_slash, SeqCst);
            let pid = sess.as_ref().map_or(0, |s| s.proc.pid());
            if a.unequip
                && let (Some(r), Some(s)) = (reader.as_mut(), &s)
                && let Some(k) = r.hotbar_key_of(&s.rod)
                && foreground_is(pid)
            {
                tap_key(&digit_key(k)); // reels the line in
            }
            if a.slash_due {
                if foreground_is(pid) {
                    tap_key(&SLASH); // '/': dropped if Roblox lost focus meanwhile, rather than typed elsewhere
                }
                bot.slash_held.store(false, SeqCst);
            }
            if a.fish
                && let (Some(st), Some(r)) = (&s, reader.as_mut())
            {
                // summed whole: rounding each ~1.5 ms tick to ms lost a third of the time (9:00 shown for 13:16)
                fishing += dt;
                bot.active_ms.store(fishing.as_millis() as i64, SeqCst);
                self.step(st, &cfg, pid, r);
                if let Some(why) = stuck_reason(self.lost_streak) {
                    bot.on.store(false, SeqCst);
                    self.stop_reason = why;
                }
            } else {
                self.hold(false);
                self.reeling = false;
                self.reel_log = ReelLog::default();
                self.trace.clear();
                let phase = match &s {
                    None => "Not in game".to_owned(),
                    Some(s) if !bot.on.load(SeqCst) => format!(
                        "Off · {}{}",
                        describe_phase(s),
                        if self.stop_reason.is_empty() {
                            String::new()
                        } else {
                            format!(" (stopped: {})", self.stop_reason)
                        }
                    ),
                    Some(s) if s.typing => "Paused (typing in chat)".into(),
                    Some(_) => "Paused (Roblox not focused)".into(),
                };
                self.set_view(s.as_ref().map_or("", |s| s.rod.as_str()), &phase);
            }
            // Unfocused, the unequip key can't be sent, so it's skipped rather than waited for.
            bot.ready_to_quit.store(
                bot.quit_requested.load(SeqCst)
                    && !bot.on.load(SeqCst)
                    && pending_catch.is_none()
                    && (!self.guard.unequip_pending() || !fg),
                SeqCst,
            );
            precise_wait(MS(1)); // a new Roblox frame (4-16 ms) is seen within ~1 ms
        }
        self.hold(false);
        // A second close doesn't wait for the XP: log the catch without it rather than lose the row.
        if let Some((text, _, popup)) = pending_catch.take() {
            let xp = reader.as_mut().and_then(|r| r.catch_xp());
            let look = reader.as_mut().and_then(|r| r.catch_look(popup));
            self.finish_catch(&text, xp, look);
        }
        // shortcut: lines a file still refuses at exit (open in Excel all along) are lost; a pending file would keep them.
        for (csv, u) in &self.unwritten {
            if !u.lines.is_empty() {
                append_csv(csv, &u.header, &u.lines);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_sells_by_the_games_filters() {
        // as read live: every rarity switch off (kept) but treasure maps
        let f: HashMap<String, bool> = [
            ("legendary", false),
            ("mythical", false),
            ("secret", false),
            ("event", true),
        ]
        .map(|(k, v)| (k.to_owned(), v))
        .into();
        assert!(!sells(&f, "Legendary") && !sells(&f, "Mythical"));
        assert!(sells(&f, "Common") && sells(&f, "Uncommon") && sells(&f, "")); // no switch: always sold
        assert!(!sells(&f, "Divine Secret") && sells(&f, "Limited+")); // following Secret and Event
    }

    #[test]
    fn bot_earned_follows_the_filter() {
        let c = |rarity: &str, value, while_on| SoldCatch {
            rarity: rarity.into(),
            value,
            while_on,
        };
        let catches = [
            c("Common", 10, true),
            c("Legendary", 500, true),
            c("Mythical", 900, false),
        ];
        assert_eq!(earned(&catches, None), (1410, 510, 0)); // everything
        let game: HashMap<String, bool> = [("legendary", false), ("mythical", true)]
            .map(|(k, v)| (k.into(), v))
            .into();
        assert_eq!(earned(&catches, Some(&game)), (910, 10, 1)); // Legendary kept
        let mine = my_filter(&["legendary".into()]);
        assert_eq!(earned(&catches, Some(&mine)), (510, 510, 1)); // only Legendary of the switchable ones sold
        assert_eq!(mine.len(), RARITY_SWITCHES.len());
    }

    #[test]
    fn bot_measures_a_turning_hue() {
        // the Mythical relic's glyph, read live 1.5 s apart: about +17 degrees a second
        let r = hue_rate(0x3e44ff, 0x8b3eff, 1.5);
        assert!((16.5..18.0).contains(&r), "{r}");
        assert!((hue_rate(0x8b3eff, 0x3e44ff, 1.5) + r).abs() < 0.01); // the other way
        assert_eq!(hue_rate(0x3e44ff, 0x3e44ff, 0.5), 0.0); // still
        assert!(hue_rate(0xff0010, 0xff1000, 0.5) > 0.0); // the short way across red (0 degrees)
        assert_eq!(hue_rate(0x707070, 0x3e44ff, 0.5), 0.0); // grey: no hue to compare
    }

    #[test]
    fn bot_models_go_to_their_catch_cards() {
        let mut v = View {
            last_catch: "B".into(),
            rarest: "A".into(),
            ..View::default()
        };
        let m = Arc::new(FishModel::default());
        assert_eq!(set_models(&mut v, "A", &m), (false, true)); // the rarest only
        assert!(v.rarest_model.is_some() && v.last_model.is_none());
        v.rarest = "B".into();
        assert_eq!(set_models(&mut v, "B", &m), (true, true)); // the last catch is also the rarest
        assert_eq!(set_models(&mut v, "C", &m), (false, false)); // an older popup: neither
    }

    #[test]
    fn view_tallies_rarities_and_mutations() {
        let mut v = View::default();
        for t in [
            "You just caught a Shiny Grey Carp at 5.8kg! (1/50)",
            "You just caught a Grey Carp at 5.8kg! (1/50)",
            "Extra! You just caught a Abyssal Deep-sea Hatchetfish at 3.2kg! (1/5)",
        ] {
            v.tally(&parse_catch(t).unwrap());
        }
        assert_eq!(v.rarities[0], ("Uncommon".to_owned(), 2)); // Grey Carp
        assert_eq!((v.shiny, v.mutated, v.sparkling), (1, 1, 0));
    }
    use crate::game::reader::{ROD_REELING, Reel};
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, OPEN_EXISTING};
    use windows::core::PCWSTR;

    #[test]
    fn bot_restored_catch_keeps_icon_and_tint() {
        let c = restored(
            "You just caught a Stalactite at 10.5kg! (1/4)",
            "rbxassetid://7".into(),
            Some(0x112233),
        )
        .unwrap();
        assert!(c.icon == "rbxassetid://7" && c.tint == Some(0x112233) && c.species == "Stalactite");
        assert!(restored("junk", String::new(), None).is_none());
    }

    #[test]
    fn bot_shake_asks_for_focus_before_each_key() {
        let mut sent: Vec<Key> = Vec::new();
        let mut nav = false;
        shake_keys(&mut nav, &mut || true, &mut |k| sent.push(*k)); // first tick: select the button, then press it
        assert!(nav && sent == vec![BACKSLASH, ENTER]);

        sent.clear();
        shake_keys(&mut nav, &mut || true, &mut |k| sent.push(*k)); // selected already: only Enter
        assert_eq!(sent, vec![ENTER]);

        sent.clear();
        nav = false;
        let mut asked = 0;
        shake_keys(
            &mut nav,
            &mut || {
                asked += 1;
                asked == 1
            },
            &mut |k| sent.push(*k),
        ); // alt-tab right after '\': no Enter in the other window
        assert!(nav && sent == vec![BACKSLASH]);

        sent.clear();
        nav = false;
        shake_keys(&mut nav, &mut || false, &mut |k| sent.push(*k)); // not focused at all: nothing, and '\' is still owed
        assert!(!nav && sent.is_empty());
    }

    #[test]
    fn bot_catch_waits_for_its_xp_popup() {
        assert!(!catch_settled(MS(399), true)); // too soon: the popup up now may be the last catch's
        assert!(catch_settled(MS(400), true));
        assert!(!catch_settled(MS(1499), false)); // not shown yet: keep looking
        assert!(catch_settled(MS(1500), false)); // some catches never show one
    }

    #[test]
    fn bot_reel_won_only_near_full() {
        assert!(reel_won(1.0) && reel_won(0.91)); // the bot lets go at 98%, and the last read can be a frame short of full
        assert!(!reel_won(0.9) && !reel_won(0.2) && !reel_won(0.0)); // the fish got away: these count toward stuck_reason
    }

    #[test]
    fn bot_reel_holds_toward_the_fish_and_brakes() {
        assert!(reel_hold(0.5, 0.0, 0.8, 0.0));
        assert!(!reel_hold(0.5, 0.0, 0.2, 0.0));
        assert!(!reel_hold(0.5, 0.6, 0.6, 0.0)); // racing right toward the fish: let go early to stop on it
        assert!(reel_hold(0.5, -0.6, 0.4, 0.0)); // sliding left toward the fish: hold to brake
        assert!(reel_hold(0.5, 0.0, 0.48, 0.5)); // fish just left but heading right fast: go meet it
    }

    #[test]
    fn bot_dreambreaker_swaps_controls_from_40_percent() {
        assert!(!controls_inverted("Dreambreaker", 0.39));
        assert!(controls_inverted("Dreambreaker", 0.4));
        assert!(controls_inverted("Abyssal dreambreaker", 0.9)); // mutation prefix, any case
        assert!(!controls_inverted("Rod Of The Depths", 0.9));
    }

    #[test]
    fn bot_cast_rating_from_feedback_label() {
        // texts seen live after casts and catches
        assert_eq!(cast_perfect("PERFECT!"), Some(true));
        assert_eq!(cast_perfect("Meh.."), Some(false));
        assert!(cast_perfect("PERFECT CATCH!").is_none()); // the catch's, still up when the next cast starts
        assert!(cast_perfect("").is_none());
    }

    #[test]
    fn bot_watchdog_stops_when_stuck() {
        assert!(stuck_reason(4).is_none());
        assert!(stuck_reason(5).is_some());
    }

    #[test]
    fn bot_cast_retries_back_off() {
        assert!(cast_retry_wait(1) == MS(400) && cast_retry_wait(2) == MS(400));
        assert!(cast_retry_wait(3) == Duration::from_secs(5) && cast_retry_wait(4) == Duration::from_secs(30));
        assert_eq!(cast_retry_wait(50), Duration::from_secs(60)); // never gives up: an AFK session outlasts a popup
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn bot_csv_append_waits_out_a_locked_file() {
        let dir = std::env::temp_dir().join("fisch-csv-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("log.csv");
        assert!(append_csv(&csv, "a,b", &lines(&["1,2"])));
        // Excel keeps an open CSV readable by others, not writable.
        let name = handy_sdk::util::text::wide_path(&csv);
        let excel = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .unwrap();
        assert!(!append_csv(&csv, "a,b", &lines(&["3,4"])));
        assert!(!append_csv(&csv, "a,b,c", &lines(&["3,4,5"]))); // a new layout can't move the locked file aside either
        unsafe { CloseHandle(excel) }.unwrap();
        assert!(append_csv(&csv, "a,b", &lines(&["3,4", "5,6"]))); // the bot retries with the refused line first
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), "a,b\n1,2\n3,4\n5,6\n");
        assert!(append_csv(&csv, "a,b,c", &lines(&["7,8,9"]))); // new layout: the old file is kept under another name
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), "a,b,c\n7,8,9\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rod_out(state: Option<i64>, typing: bool) -> FishState {
        FishState {
            rod: "Carrot Rod".into(),
            rod_state: state,
            typing,
            character: 1,
            ..Default::default()
        }
    }

    fn tk(now: Instant, on: bool, focused: bool, quit: bool, slash: bool, s: &FishState) -> Tick<'_> {
        Tick {
            now,
            on,
            focused,
            quit_requested: quit,
            slash_held: slash,
            s: Some(s),
        }
    }

    #[test]
    fn bot_guard_chat_key_with_line_out_unequips_then_types() {
        let t0 = Instant::now();
        let mut g = SafetyGuard::default();
        let s = rod_out(Some(ROD_LURE), false);
        let a = g.tick(&tk(t0, true, true, false, false, &s));
        assert!(a.on && a.just_on && a.fish && a.hold_slash && !a.unequip);
        let a = g.tick(&tk(t0 + MS(5), true, true, false, true, &s)); // the hook held back '/'
        assert!(!a.on && a.unequip && a.stop_reason == "chat opened" && !a.fish && !a.slash_due);
        let a = g.tick(&tk(t0 + MS(60), false, true, false, true, &s));
        assert!(a.slash_due && !a.unequip); // '/' goes out after the unequip, which isn't sent twice
    }

    #[test]
    fn bot_guard_chat_opened_by_click_unequips_once_it_closes() {
        let t0 = Instant::now();
        let mut g = SafetyGuard::default();
        let (fishing, typing) = (rod_out(Some(ROD_LURE), false), rod_out(Some(ROD_LURE), true));
        assert!(g.tick(&tk(t0, true, true, false, false, &fishing)).fish);
        let a = g.tick(&tk(t0 + MS(5), true, true, false, false, &typing));
        assert!(!a.on && a.stop_reason == "chat opened" && !a.unequip && g.unequip_pending()); // keys would be typed
        assert!(!g.tick(&tk(t0 + MS(10), false, true, false, false, &typing)).unequip);
        let a = g.tick(&tk(t0 + MS(15), false, true, false, false, &fishing));
        assert!(a.unequip && !g.unequip_pending());
    }

    #[test]
    fn bot_guard_turned_off_with_line_out_unequips_when_focused() {
        let t0 = Instant::now();
        let mut g = SafetyGuard::default();
        let s = rod_out(Some(ROD_LURE), false);
        assert!(g.tick(&tk(t0, true, true, false, false, &s)).fish);
        let a = g.tick(&tk(t0 + MS(5), false, false, false, false, &s)); // dashboard click: Roblox lost focus
        assert!(!a.fish && !a.unequip && g.unequip_pending() && a.stop_reason.is_empty());
        assert!(g.tick(&tk(t0 + MS(10), false, true, false, false, &s)).unequip);
        assert!(!g.tick(&tk(t0 + MS(15), false, true, false, false, &s)).unequip);
        let idle = rod_out(Some(ROD_IDLE), false); // turned off with the line in: nothing to do
        let mut g2 = SafetyGuard::default();
        g2.tick(&tk(t0, true, true, false, false, &idle));
        assert!(!g2.tick(&tk(t0 + MS(5), false, true, false, false, &idle)).unequip && !g2.unequip_pending());
    }

    #[test]
    fn bot_guard_turned_off_mid_reel_lets_go_then_unequips() {
        let t0 = Instant::now();
        let mut g = SafetyGuard::default();
        let (reeling, idle) = (rod_out(Some(ROD_REELING), false), rod_out(Some(ROD_IDLE), false));
        assert!(g.tick(&tk(t0, true, true, false, false, &reeling)).fish);
        let a = g.tick(&tk(t0 + MS(5), false, true, false, false, &reeling)); // F1 mid-reel
        assert!(!a.on && !a.fish && !a.unequip); // let go at once; the rod's key does nothing mid-reel
        assert!(!g.tick(&tk(t0 + MS(500), false, true, false, false, &reeling)).fish);
        // the reel bar lingers ~0.6 s after the rod turns idle; the game ignores the key until it goes
        let bar_up = FishState {
            reel: Some(Reel::default()),
            ..idle.clone()
        };
        assert!(!g.tick(&tk(t0 + MS(900), false, true, false, false, &bar_up)).unequip);
        assert!(!g.tick(&tk(t0 + MS(1400), false, false, false, false, &idle)).unequip); // waits for Roblox focus
        assert!(g.tick(&tk(t0 + MS(1410), false, true, false, false, &idle)).unequip); // bar gone, focused: now
        assert!(!g.tick(&tk(t0 + MS(1800), false, true, false, false, &idle)).unequip); // too soon to retry
        assert!(g.tick(&tk(t0 + MS(2020), false, true, false, false, &idle)).unequip); // ignored: pressed again
        let mut gone = idle.clone();
        gone.rod = String::new();
        assert!(!g.tick(&tk(t0 + MS(2100), false, true, false, false, &gone)).unequip);
        assert!(!g.tick(&tk(t0 + MS(9000), false, true, false, false, &idle)).unequip); // done once it went
        // never more than 3 presses
        let mut g = SafetyGuard::default();
        g.tick(&tk(t0, true, true, false, false, &reeling));
        g.tick(&tk(t0 + MS(5), false, true, false, false, &reeling));
        let presses = (1..60)
            .filter(|i| {
                g.tick(&tk(t0 + MS(10 + i * 200), false, true, false, false, &idle))
                    .unequip
            })
            .count();
        assert_eq!(presses, 3);
        // back on mid-reel: fishing as usual, nothing unequipped after
        let mut g = SafetyGuard::default();
        g.tick(&tk(t0, true, true, false, false, &reeling));
        g.tick(&tk(t0 + MS(5), false, true, false, false, &reeling));
        assert!(g.tick(&tk(t0 + MS(10), true, true, false, false, &reeling)).fish);
        let a = g.tick(&tk(t0 + MS(15), true, true, false, false, &idle));
        assert!(a.on && !a.unequip);
    }

    #[test]
    fn bot_guard_quit_lets_the_reel_finish() {
        let t0 = Instant::now();
        let mut g = SafetyGuard::default();
        let (reeling, lure) = (rod_out(Some(ROD_REELING), false), rod_out(Some(ROD_LURE), false));
        let a = g.tick(&tk(t0, true, true, true, false, &reeling));
        assert!(a.on && a.fish);
        let a = g.tick(&tk(t0 + MS(5), true, true, true, false, &lure));
        assert!(!a.on && !a.fish && a.unequip && a.stop_reason.is_empty());
    }

    #[test]
    fn bot_guard_rod_unequipped_stops_but_respawn_reequips() {
        let t0 = Instant::now();
        let mut g = SafetyGuard::default();
        let mut s = rod_out(Some(ROD_IDLE), false);
        assert!(g.tick(&tk(t0, true, true, false, false, &s)).fish);
        s.rod.clear();
        let a = g.tick(&tk(t0 + MS(5), true, true, false, false, &s));
        assert!(a.on && !a.fish); // could be a misread: wait, and don't re-equip meanwhile
        let a = g.tick(&tk(t0 + MS(400), true, true, false, false, &s));
        assert!(!a.on && a.stop_reason == "rod unequipped");

        let mut g2 = SafetyGuard::default();
        let mut r = rod_out(Some(ROD_IDLE), false);
        g2.tick(&tk(t0, true, true, false, false, &r));
        r.rod.clear();
        r.character = 2; // died: a new character without the rod
        let a = g2.tick(&tk(t0 + MS(400), true, true, false, false, &r));
        assert!(a.on && a.fish);
    }

    #[test]
    fn bot_reel_gui_missing_needs_state_8_without_reel() {
        let mut s = FishState::default();
        assert!(!reel_gui_missing(None) && !reel_gui_missing(Some(&s)));
        s.rod_state = Some(ROD_REELING);
        assert!(reel_gui_missing(Some(&s)));
        s.rod_state = Some(ROD_LURE);
        assert!(!reel_gui_missing(Some(&s)));
        s.rod_state = Some(ROD_REELING);
        s.reel = Some(Reel::default());
        assert!(!reel_gui_missing(Some(&s)));
    }

    #[test]
    fn bot_describe_phase() {
        let mut s = FishState::default();
        assert_eq!(describe_phase(&s), "No rod in hand");
        s.rod = "Fallen Rod".into();
        s.rod_state = Some(ROD_IDLE);
        assert_eq!(describe_phase(&s), "Idle");
        s.rod_state = Some(5);
        assert_eq!(describe_phase(&s), "Casting");
        s.rod_state = Some(ROD_LURE);
        assert_eq!(describe_phase(&s), "Waiting for a bite");
        s.shake_button_x = Some(0.4);
        assert_eq!(describe_phase(&s), "Shaking");
        s.shake_button_x = None;
        s.rod_state = Some(ROD_REELING);
        s.reel = Some(Reel {
            bar: 0.5,
            bar_width: 0.3,
            fish: 0.5,
            progress: 0.45,
            progress_speed: String::new(),
        });
        assert_eq!(describe_phase(&s), "Reeling 45%");
        assert!(
            line_out(Some(4))
                && line_out(Some(5))
                && line_out(Some(6))
                && !line_out(Some(3))
                && !line_out(Some(7))
                && !line_out(Some(8))
        );
    }

    #[test]
    fn bot_reeling_text_shows_the_fill_speed() {
        assert_eq!(reeling_text(0.45, 0.082, ""), "Reeling 45% · +8.2%/s");
        assert_eq!(reeling_text(0.3, -0.031, ""), "Reeling 30% · -3.1%/s"); // the fish is winning
        assert_eq!(reeling_text(0.0, 0.0, ""), "Reeling 0% · +0.0%/s");
        // the reel bar's own label, read live
        assert_eq!(
            reeling_text(0.41, 0.182, "+55% Progress Speed"),
            "Reeling 41% · +18.2%/s (+55%)"
        );
        assert_eq!(
            reeling_text(0.41, 0.182, "-10% Progress Speed"),
            "Reeling 41% · +18.2%/s (-10%)"
        );
        assert_eq!(reeling_text(0.41, 0.182, "garbage"), "Reeling 41% · +18.2%/s");
    }

    #[test]
    fn bot_reel_stats_measure_tracking() {
        let mut log = ReelLog {
            width: 0.3,
            ..Default::default()
        };
        for i in 0..=400 {
            // 2 s at 5 ms: bar slides 0.5 -> right wall (0.85) at 0.5/s, fish sits at 0.72
            let t = i as f32 * 0.005;
            log.bar.push((t, 0.85f32.min(0.5 + 0.5 * t)));
            log.fish.push((t, 0.72));
        }
        let r = reel_stats(&log);
        assert!((r.seconds - 2.0).abs() < 0.01);
        assert_eq!(r.wall_hits, 1); // reached the wall once, at 0.5/s
        assert!(r.inside_frac > 0.9 && r.inside_frac < 1.0); // outside only the first ~0.14 s (bar below 0.57)
        assert!(r.mean_dist > 0.1 && r.mean_dist < 0.15);
        assert_eq!(reel_stats(&ReelLog::default()).seconds, 0.0); // empty log: nothing
    }
}
