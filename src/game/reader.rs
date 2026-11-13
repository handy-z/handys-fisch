use std::collections::HashMap;
use std::sync::Arc;

use handy_sdk::app::updater_thread::Session;
use handy_sdk::roblox::instance::{child_by_class, child_by_name, children, data_model, instance_class, instance_name};
use handy_sdk::roblox::memory::Memory;
use handy_sdk::roblox::read::{
    CachedPath, VisibleProbe, character_of, gui_image, gui_layout_order, gui_size, gui_text, int_value, local_player,
    number_value, object_value, parent_of, part_position, player_gui, string_value,
};

use super::catch::{asset_id, model_tint, parse_xp};

/// How a catch popup shows the catch; see GameReader::catch_look.
#[derive(Debug, Default)]
pub struct CatchLook {
    pub icon: Option<String>,
    pub tint: Option<u32>,
}

// Fisch rod values/state (IntValue), recorded by tools/debug/fish_record.py.
pub const ROD_REEL_RESET: i64 = 2;
pub const ROD_IDLE: i64 = 3;
pub const ROD_LURE: i64 = 6;
pub const ROD_REEL_LOCKED: i64 = 7;
pub const ROD_REELING: i64 = 8;
pub const ROD_REEL_DONE: i64 = 9;

/// The reel's own states (7 -> 8 -> 9 -> 2). Its GUI lingers ~0.9 s into idle and the next cast, so check this too.
pub fn reel_state(s: Option<i64>) -> bool {
    matches!(s, Some(ROD_REEL_LOCKED | ROD_REELING | ROD_REEL_DONE | ROD_REEL_RESET))
}

/// Cast (4, 5) or waiting / shaking (6): the line is out but nothing is hooked yet.
pub fn line_out(s: Option<i64>) -> bool {
    matches!(s, Some(4 | 5 | ROD_LURE))
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reel {
    /// X scales in reel/bar; progress 0..1
    pub bar: f32,
    pub bar_width: f32,
    pub fish: f32,
    pub progress: f32,
    /// reel/bar/progressspeed, the game's total for this reel (rod, enchant, bait, the fish's own): "+55% Progress
    /// Speed"; empty while the game hides it
    pub progress_speed: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FishState {
    /// equipped rod name, empty when none
    pub rod: String,
    /// rod values/state
    pub rod_state: Option<i64>,
    /// PlayerGui/shakeui/safezone/<button> X scale while luring
    pub shake_button_x: Option<f32>,
    /// PlayerGui/reel while reeling
    pub reel: Option<Reel>,
    /// not read here: the bot sets it from RobloxWatcher (chat focus)
    pub typing: bool,
    /// the character model: a new one after death / respawn
    pub character: usize,
}

/// (item name, key label) per hotbar slot.
pub type Slots = Vec<(String, String)>;

/// Hotbar key (0-9) of the first rod (in the given priority order) found in a slot. Slots: (item name, key label);
/// item names carry mutation prefixes ("Abyssal Fallen Rod" for the tool "Fallen Rod"), so the exact name wins, then a
/// name ending in the rod's.
pub fn hotbar_key_for(rods: &[String], slots: &[(String, String)]) -> Option<i32> {
    for r in rods {
        // rods in priority order
        let mut prefixed = None;
        for (item, key) in slots {
            let k = key.as_bytes();
            if r.is_empty() || k.len() != 1 || !k[0].is_ascii_digit() {
                continue;
            }
            let digit = i32::from(k[0] - b'0');
            if item == r {
                return Some(digit); // first: "Original No-Life Rod" also ends with "No-Life Rod"
            }
            if prefixed.is_none() && item.ends_with(&format!(" {r}")) {
                prefixed = Some(digit);
            }
        }
        // shortcut: an enchanted No-Life Rod next to an Original No-Life Rod can still pick the wrong one, check against
        // RODS if a player ever owns both.
        if prefixed.is_some() {
            return prefixed;
        }
    }
    None
}

/// "[Momentum] Carrot Rod": the rod with the enchant its hotbar label puts in front ("Momentum Carrot Rod"); just the
/// rod when it has none or isn't on the hotbar.
pub fn enchanted_rod(rod: &str, slots: &[(String, String)]) -> String {
    if slots.iter().any(|(item, _)| item == rod) {
        return rod.into();
    }
    for (item, _) in slots {
        if !rod.is_empty() && item.len() > rod.len() + 1 && item.ends_with(&format!(" {rod}")) {
            return format!("[{}] {rod}", &item[..item.len() - rod.len() - 1]);
        }
    }
    rod.into()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LevelProgress {
    pub level: i32,
    pub xp: f64,
}

/// The player beyond fishing, for the dashboard. Each field stays empty / 0 when it can't be read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlayerInfo {
    /// Stats/rod ("Carrot Rod")
    pub rod: String,
    /// enchanted_rod() of it
    pub rod_label: String,
    /// The rod's live stats from the game's equipment panel, enchant bonuses included, in its order:
    /// "Lure Speed: 85% (+28%)", "Luck: 125%", ... "Caught: 737". Empty when the panel has no such rod.
    pub rod_stats: Vec<String>,
    /// Stats/bait, Cache/CurrentWorld ("Sea 1"), the character's zone
    pub bait: String,
    pub world: String,
    pub zone: String,
    /// HumanoidRootPart, studs
    pub pos: Option<[f32; 3]>,
    /// overall, from the player list ("14%")
    pub bestiary: String,
    /// Stats/tracker_*
    pub fish_caught: f64,
    pub shiny: f64,
    pub sparkling: f64,
    pub seconds_played: f64,
}

/// `c` re-made when `names` differ from last time (they hold the rod's or the player's name).
fn path_to(m: &dyn Memory, sess: &Session, c: &mut Option<CachedPath>, names: &[&str], root: usize) -> usize {
    if c.as_ref().is_none_or(|c| c.names() != names) {
        *c = Some(CachedPath::new(names));
    }
    c.as_mut().map_or(0, |c| c.get(m, &sess.offsets, root))
}

/// Read-only view of the local player's fishing state. Not thread-safe; one per thread. Every offset it reads is in
/// required_keys(), which the Updater enforces, so none can be missing.
pub struct GameReader {
    sess: Arc<Session>,
    players: usize,
    lp: usize,
    gui: usize,
    stats_owner: usize, // cached PlayerStats/<name>/T/<name> + its Stats folder
    stats: usize,
    prompt_gui: usize, // cached CoreGui/RobloxPromptGui + its promptOverlay
    overlay: usize,
    rod: usize, // cached Tool + its values/state
    rod_state: usize,
    reel: usize, // cached reel ScreenGui + parts
    bar: usize,
    fish: usize,
    prog: usize,
    prog_speed: usize,
    seen_popups: Option<Vec<usize>>, // unset until the first take_catch_popup()
    // Children seen by the last character / PlayerGui scan: sample() reads names again only when a list changed.
    scanned_chr: usize, // the character chr_kids is of
    shake: usize,       // the shakeui that scan found
    chr_kids: Vec<usize>,
    gui_kids: Vec<usize>,
    // Paths read every tick or every poll; each checks its cached nodes instead of reading every name on the way.
    coins: CachedPath,
    xpup: CachedPath,
    // always shown / hidden unless opened: tell a wrong dumped Visible offset
    safezone: CachedPath,
    inventory: CachedPath,
    fade: CachedPath,
    vis: VisibleProbe,
    announcements: CachedPath,
    power: CachedPath,
    feedback: CachedPath,
    level: CachedPath, // in stats()
    xp: CachedPath,
    hotbar: CachedPath,
    world: CachedPath,
    zone: CachedPath,
    root: CachedPath,
    stat_paths: HashMap<String, CachedPath>,
    rod_stats: Option<CachedPath>,
    bestiary: Option<CachedPath>,
}

impl GameReader {
    pub fn new(sess: Arc<Session>) -> Self {
        Self {
            sess,
            players: 0,
            lp: 0,
            gui: 0,
            stats_owner: 0,
            stats: 0,
            prompt_gui: 0,
            overlay: 0,
            rod: 0,
            rod_state: 0,
            reel: 0,
            bar: 0,
            fish: 0,
            prog: 0,
            prog_speed: 0,
            seen_popups: None,
            scanned_chr: 0,
            shake: 0,
            chr_kids: Vec::new(),
            gui_kids: Vec::new(),
            coins: CachedPath::new(&["hud", "safezone", "coins"]),
            xpup: CachedPath::new(&["hud", "safezone", "xpup"]),
            safezone: CachedPath::new(&["hud", "safezone"]),
            inventory: CachedPath::new(&["backpack", "inventory"]),
            fade: CachedPath::new(&["death", "fade"]),
            vis: VisibleProbe::default(),
            announcements: CachedPath::new(&["hud", "safezone", "announcements"]),
            power: CachedPath::new(&["HumanoidRootPart", "power", "powerbar", "bar"]),
            feedback: CachedPath::new(&["HumanoidRootPart", "powerfeedback", "title"]),
            level: CachedPath::new(&["level"]),
            xp: CachedPath::new(&["xp"]),
            hotbar: CachedPath::new(&["backpack", "hotbar"]),
            world: CachedPath::new(&["Cache", "CurrentWorld"]),
            zone: CachedPath::new(&["zone"]),
            root: CachedPath::new(&["HumanoidRootPart"]),
            stat_paths: HashMap::new(),
            rod_stats: None,
            bestiary: None,
        }
    }

    /// Hotbar key of a rod in the Backpack, `preferred` first if it's there; slow (walks the Backpack).
    pub fn rod_hotkey(&mut self, preferred: &str) -> Option<i32> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let mut rods = Vec::new();
        let bp = child_by_class(p, o, self.lp, "Backpack");
        if bp != 0 {
            for c in children(p, o, bp) {
                if instance_class(p, o, c).as_deref() == Some("Tool") && child_by_name(p, o, c, "rodClient") != 0 {
                    rods.push(instance_name(p, o, c).unwrap_or_default());
                }
            }
        }
        let (mut first, rest): (Vec<String>, Vec<String>) = rods.into_iter().partition(|r| r == preferred);
        first.extend(rest); // a stable partition
        if first.is_empty() {
            None
        } else {
            hotbar_key_for(&first, &self.hotbar_slots())
        }
    }

    /// e.g. the equipped rod's key, to unequip it
    pub fn hotbar_key_of(&mut self, item: &str) -> Option<i32> {
        if self.resolve_player() {
            hotbar_key_for(&[item.to_owned()], &self.hotbar_slots())
        } else {
            None
        }
    }

    /// (item name, key label) per hotbar slot
    fn hotbar_slots(&mut self) -> Slots {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let bar = self.hotbar.get(p, o, self.gui);
        let text = |slot: usize, label: &str| gui_text(p, o, child_by_name(p, o, slot, label)).unwrap_or_default();
        let mut slots = Vec::new();
        for s in if bar != 0 { children(p, o, bar) } else { Vec::new() } {
            if instance_name(p, o, s).as_deref() == Some("ItemTemplate") {
                slots.push((text(s, "ItemName"), text(s, "TextLabel")));
            }
        }
        slots
    }

    /// The HUD's balance text, e.g. "4,105,298 C$" (hud/safezone/coins)
    pub fn coins(&mut self) -> Option<String> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        gui_text(p, o, self.coins.get(p, o, self.gui))
    }

    /// Roblox's disconnect / kick dialog is up (CoreGui/RobloxPromptGui/promptOverlay/ErrorPrompt)
    pub fn error_prompt(&mut self) -> bool {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        if !self.parent_is(self.overlay, self.prompt_gui) {
            // CoreGui/RobloxPromptGui/promptOverlay
            let dm = data_model(p, o);
            let cg = if dm != 0 {
                child_by_class(p, o, dm, "CoreGui")
            } else {
                0
            };
            self.prompt_gui = if cg != 0 {
                child_by_name(p, o, cg, "RobloxPromptGui")
            } else {
                0
            };
            self.overlay = if self.prompt_gui != 0 {
                child_by_name(p, o, self.prompt_gui, "promptOverlay")
            } else {
                0
            };
        }
        self.overlay != 0 && child_by_name(p, o, self.overlay, "ErrorPrompt") != 0
    }

    /// Workspace/PlayerStats/<name>/T/<name>/Stats, cached; 0 if not there. After resolve_player().
    fn stats(&mut self) -> usize {
        if !self.parent_is(self.stats, self.stats_owner) {
            // replicated by the game
            let sess = self.sess.clone();
            let (p, o) = (&sess.proc, &sess.offsets);
            let me = instance_name(p, o, self.lp).unwrap_or_default();
            let dm = if me.is_empty() { 0 } else { data_model(p, o) };
            let mut i = if dm != 0 {
                child_by_class(p, o, dm, "Workspace")
            } else {
                0
            };
            for n in ["PlayerStats", me.as_str(), "T", me.as_str()] {
                i = if i != 0 { child_by_name(p, o, i, n) } else { 0 };
            }
            self.stats_owner = i;
            self.stats = if i != 0 { child_by_name(p, o, i, "Stats") } else { 0 };
        }
        self.stats
    }

    /// The game's own level + XP into it (Workspace/PlayerStats/...)
    pub fn level_progress(&mut self) -> Option<LevelProgress> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let level = if self.stats() != 0 {
            self.level.get(p, o, self.stats)
        } else {
            0
        };
        let level = number_value(p, o, level);
        // xp: into the current level, +each catch's popup XP
        let xp = if self.stats() != 0 {
            self.xp.get(p, o, self.stats)
        } else {
            0
        };
        let xp = number_value(p, o, xp);
        match (level, xp) {
            (Some(level), Some(xp)) if level >= 1.0 => Some(LevelProgress {
                level: level as i32,
                xp,
            }),
            _ => None,
        }
    }

    /// Stats/<name>, cached per name; 0 if not there
    fn stat(&mut self, name: &str) -> usize {
        if self.stats() == 0 {
            return 0;
        }
        let sess = self.sess.clone();
        self.stat_paths
            .entry(name.to_owned())
            .or_insert_with(|| CachedPath::new(&[name]))
            .get(&sess.proc, &sess.offsets, self.stats)
    }

    /// Slow (walks the hotbar): not while reeling.
    pub fn player_info(&mut self) -> Option<PlayerInfo> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let mut info = PlayerInfo::default();
        let text = |me: &mut Self, n: &str| string_value(p, o, me.stat(n)).unwrap_or_default();
        info.rod = text(self, "rod");
        info.rod_label = enchanted_rod(&info.rod, &self.hotbar_slots());
        // hud/.../ScrollingFrame/<rod>/Stats/<stat>/Label, always loaded
        let stats = if info.rod.is_empty() {
            0
        } else {
            let names = [
                "hud",
                "safezone",
                "equipment",
                "Container",
                "Rods",
                "Main",
                "ScrollingFrame",
                info.rod.as_str(),
                "Stats",
            ];
            path_to(p, &sess, &mut self.rod_stats, &names, self.gui)
        };
        let mut lines: Vec<(i32, String)> = Vec::new(); // LayoutOrder, text
        for s in if stats != 0 { children(p, o, stats) } else { Vec::new() } {
            if !self.vis.read(p, o, s).unwrap_or(false) {
                continue; // hidden: Disturbance...
            }
            if let Some(t) = gui_text(p, o, child_by_name(p, o, s, "Label")).filter(|t| !t.is_empty()) {
                lines.push((gui_layout_order(p, o, s).unwrap_or(0), t));
            }
        }
        lines.sort_by_key(|l| l.0); // stable
        info.rod_stats = lines.into_iter().map(|l| l.1).collect();
        info.bait = text(self, "bait");
        // Cache is Stats' sibling
        info.world = string_value(p, o, self.world.get(p, o, self.stats_owner)).unwrap_or_default();
        let num = |me: &mut Self, n: &str| number_value(p, o, me.stat(n)).unwrap_or(0.0);
        info.fish_caught = num(self, "tracker_fishcaught");
        info.shiny = num(self, "tracker_shinycaught");
        info.sparkling = num(self, "tracker_sparklingcaught");
        info.seconds_played = num(self, "tracker_timeplayed");

        let chr = character_of(p, o, self.lp);
        if chr != 0 {
            let zone = self.zone.get(p, o, chr); // ObjectValue -> the zone Part the game puts you in
            let z = object_value(p, o, zone);
            if z != 0 {
                info.zone = instance_name(p, o, z).unwrap_or_default();
            }
            if let Some(pos) = part_position(p, o, self.root.get(p, o, chr)) {
                info.pos = Some([pos.x, pos.y, pos.z]);
            }
        }

        // the player list shows everyone's overall bestiary %
        let me = instance_name(p, o, self.lp).unwrap_or_default();
        let row = path_to(
            p,
            &sess,
            &mut self.bestiary,
            &["PlayerList", "Panel", "List", me.as_str(), "Bestiary"],
            self.gui,
        );
        info.bestiary = gui_text(p, o, row).unwrap_or_default();
        Some(info)
    }

    /// The "+45xp" popup shown at a catch (hud/safezone/xpup), while it's visible
    pub fn catch_xp(&mut self) -> Option<i64> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let i = self.xpup.get(p, o, self.gui);
        if !self.vis.read(p, o, i).unwrap_or(false) {
            return None; // shown per catch
        }
        gui_text(p, o, child_by_name(p, o, i, "xp")).and_then(|t| parse_xp(&t))
    }

    /// Cast power 0..1 (HumanoidRootPart/power/powerbar/bar height) while charging, None otherwise. Call after
    /// sample().
    pub fn cast_power(&mut self) -> Option<f32> {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        gui_size(p, o, self.power.get(p, o, character_of(p, o, self.lp))).map(|s| s.y_scale)
    }

    /// The game's rating over the character (HumanoidRootPart/powerfeedback/title): "PERFECT!" / "Meh.." just after a
    /// cast, "PERFECT CATCH!" after a catch. Shown ~1.4 s. Call after sample().
    pub fn cast_feedback(&mut self) -> Option<String> {
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        gui_text(p, o, self.feedback.get(p, o, character_of(p, o, self.lp)))
    }

    /// Text of a catch popup (hud/safezone/announcements/catch, one per catch) not returned before; popups already up
    /// on the first call are skipped. The fish is only revealed here: while reeling the GUI shows a generic icon.
    pub fn take_catch_popup(&mut self) -> Option<(String, usize)> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let i = self.announcements.get(p, o, self.gui);
        if i == 0 {
            return None;
        }
        let popups: Vec<usize> = children(p, o, i)
            .into_iter()
            .filter(|&c| instance_name(p, o, c).as_deref() == Some("catch"))
            .collect();
        let first = self.seen_popups.is_none();
        let seen = self.seen_popups.get_or_insert_with(|| popups.clone());
        seen.retain(|c| popups.contains(c));
        if first {
            return None;
        }
        for &c in &popups {
            if seen.contains(&c) {
                continue;
            }
            match gui_text(p, o, child_by_name(p, o, c, "Main")) {
                Some(text) if !text.is_empty() => {
                    seen.push(c);
                    return Some((text, c));
                }
                _ => continue, // just created: filled in a moment later, retry next call
            }
        }
        None
    }

    /// How a catch popup from take_catch_popup shows the catch: its flat icon (catch/fih/icon, "rbxassetid://123"; None
    /// for fish shown as a 3D model, most of them) and the color the game recolored that model to (model_tint; None
    /// unless it marks it recolored with a "<mutation>template" part). None once the popup is gone. Set a moment after
    /// the text: read it with the XP.
    pub fn catch_look(&mut self, popup: usize) -> Option<CatchLook> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let ann = self.announcements.get(p, o, self.gui);
        if !self.parent_is(popup, ann) {
            return None; // freed: its memory may hold anything now
        }
        let fih = child_by_name(p, o, popup, "fih");
        let icon = gui_image(p, o, child_by_name(p, o, fih, "icon")).filter(|s| asset_id(s).is_some());
        let model = child_by_name(p, o, child_by_name(p, o, fih, "vp"), "viewmodel");
        let kids = children(p, o, model);
        let recolored = kids
            .iter()
            .any(|&c| instance_name(p, o, c).is_some_and(|n| n.ends_with("template")));
        let m: &dyn Memory = p;
        let mut parts = Vec::new();
        let mut solids = 0;
        let mut stack = if recolored { kids } else { Vec::new() }; // only a recolored fish needs its parts read
        while let Some(c) = stack.pop() {
            if solids >= 64 {
                break; // plenty to average; a huge model stays cheap
            }
            // every solid part kind: Piranha and Anomalocaris bodies are unions (PartOperation)
            if matches!(
                instance_class(p, o, c).as_deref(),
                Some("Part" | "MeshPart" | "PartOperation" | "UnionOperation" | "WedgePart" | "CornerWedgePart")
            ) {
                let rgb = m.get::<[u8; 4]>(o.at(c, "BasePart.Color3")).unwrap_or_default();
                let prim = m.get::<usize>(o.at(c, "BasePart.Primitive")).unwrap_or(0);
                let rgb = u32::from_be_bytes([0, rgb[0], rgb[1], rgb[2]]);
                let transparency = m.get::<f32>(o.at(c, "BasePart.Transparency")).unwrap_or(1.0);
                let material = if prim == 0 {
                    0
                } else {
                    m.get::<u16>(o.at(prim, "Primitive.Material")).unwrap_or(0)
                };
                solids += 1;
                parts.push((rgb, transparency, material));
            }
            stack.extend(children(p, o, c));
        }
        Some(CatchLook {
            icon,
            tint: model_tint(&parts), // empty unless recolored
        })
    }

    fn parent_is(&self, inst: usize, parent: usize) -> bool {
        inst != 0 && parent != 0 && parent_of(&self.sess.proc, &self.sess.offsets, inst) == parent
    }

    fn scale_x(&self, gui: usize, field: &str) -> f32 {
        let m: &dyn Memory = &self.sess.proc;
        m.get::<f32>(self.sess.offsets.at(gui, field)).unwrap_or(0.0) // UDim2 starts with X.Scale
    }

    fn resolve_player(&mut self) -> bool {
        // A rejoin or teleport frees the old player, whose stale memory still passes parent_is(gui, lp): seen live,
        // every character and PlayerGui read went empty until restart. So check down from the DataModel each time (4
        // reads).
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let dm = data_model(p, o);
        if !self.parent_is(self.players, dm) {
            self.players = if dm != 0 {
                child_by_class(p, o, dm, "Players")
            } else {
                0
            };
        }
        let lp = local_player(p, o, self.players);
        if lp != 0 && lp == self.lp && self.parent_is(self.gui, self.lp) {
            return true;
        }
        if lp != self.lp {
            // the caches below hang off the old player's game, freed just the same
            self.stats_owner = 0;
            self.stats = 0;
            self.prompt_gui = 0;
            self.overlay = 0;
            self.rod = 0;
            self.rod_state = 0;
            self.reel = 0;
            self.scanned_chr = 0;
            self.shake = 0;
            self.seen_popups = None;
            self.chr_kids.clear();
            self.gui_kids.clear();
        }
        self.lp = lp;
        self.gui = player_gui(p, o, self.lp);
        if self.gui != 0 {
            // hotbar alone left 5 offsets fitting (live, version-4ea8e85481e34f46: the real 0x345 and 0x1ed, 0x1f5,
            // 0x22d, 0x235), so the scan kept the wrong dumped 0x72d; safezone is 0 at those four
            let shown = [self.hotbar.get(p, o, self.gui), self.safezone.get(p, o, self.gui)];
            let hidden = [self.inventory.get(p, o, self.gui), self.fade.get(p, o, self.gui)];
            self.vis.calibrate(p, o, &shown, &hidden, &[]);
        }
        self.gui != 0
    }

    /// None while loading / not in game
    pub fn sample(&mut self) -> Option<FishState> {
        if !self.resolve_player() {
            return None;
        }
        let sess = self.sess.clone();
        let (p, o) = (&sess.proc, &sess.offsets);
        let mut s = FishState::default();
        // Equipped tools are parented to the character; Fisch rods carry a rodClient script.
        let chr = character_of(p, o, self.lp);
        s.character = chr;
        if !self.parent_is(self.rod, chr) || self.rod_state == 0 {
            // No rod last time and the same children: nothing to find. A rod without its state yet keeps looking.
            let kids = if chr != 0 { children(p, o, chr) } else { Vec::new() };
            let same = self.rod == 0 && chr == self.scanned_chr && kids == self.chr_kids;
            self.scanned_chr = chr;
            self.chr_kids = kids;
            if !same {
                self.rod = 0;
                self.rod_state = 0;
                for &c in &self.chr_kids {
                    if instance_class(p, o, c).as_deref() == Some("Tool") && child_by_name(p, o, c, "rodClient") != 0 {
                        self.rod = c;
                        let vals = child_by_name(p, o, c, "values");
                        self.rod_state = if vals != 0 {
                            child_by_name(p, o, vals, "state")
                        } else {
                            0
                        };
                        break;
                    }
                }
            }
        }
        if self.rod != 0 {
            s.rod = instance_name(p, o, self.rod).unwrap_or_else(|| "?".into());
            if self.rod_state != 0 {
                s.rod_state = int_value(p, o, self.rod_state);
            }
        }

        if !self.parent_is(self.reel, self.gui) {
            // The reel and shakeui come and go as PlayerGui children: read their names only when that list changed.
            let kids = children(p, o, self.gui);
            let fresh = kids != self.gui_kids;
            if fresh {
                self.gui_kids = kids;
                self.reel = 0;
                self.bar = 0;
                self.fish = 0;
                self.prog = 0;
                self.prog_speed = 0;
                self.shake = 0;
            }
            // nothing new: the shakeui found before is read below
            for &c in if fresh { &self.gui_kids[..] } else { &[] } {
                let n = instance_name(p, o, c);
                if n.as_deref() == Some("reel") {
                    let bar = child_by_name(p, o, c, "bar");
                    let child = |name: &str| if bar != 0 { child_by_name(p, o, bar, name) } else { 0 };
                    let prog = child("progress");
                    self.reel = c;
                    self.bar = child("playerbar");
                    self.fish = child("fish");
                    self.prog = if prog != 0 { child_by_name(p, o, prog, "bar") } else { 0 };
                    self.prog_speed = child("progressspeed");
                } else if n.as_deref() == Some("shakeui") {
                    self.shake = c;
                }
            }
            if self.shake != 0 {
                // every tick while luring: the button moves and is replaced after each click
                let zone = child_by_name(p, o, self.shake, "safezone");
                let mut btn = if zone != 0 {
                    child_by_name(p, o, zone, "button")
                } else {
                    0
                };
                if btn == 0 && zone != 0 {
                    btn = child_by_name(p, o, zone, "default");
                }
                if btn != 0 {
                    s.shake_button_x = Some(self.scale_x(btn, "GuiObject.Position"));
                }
            }
        }
        if self.reel != 0 && self.bar != 0 && self.fish != 0 && self.prog != 0 {
            s.reel = Some(Reel {
                bar: self.scale_x(self.bar, "GuiObject.Position"),
                bar_width: self.scale_x(self.bar, "GuiObject.Size"),
                fish: self.scale_x(self.fish, "GuiObject.Position"),
                progress: self.scale_x(self.prog, "GuiObject.Size"),
                // hidden on most reels, still holding "-100% Progress Speed": only what the game shows
                progress_speed: if self.vis.read(p, o, self.prog_speed).unwrap_or(false) {
                    gui_text(p, o, self.prog_speed).unwrap_or_default()
                } else {
                    String::new()
                },
            });
        }
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(v: &[(&str, &str)]) -> Slots {
        v.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
    }
    fn rods(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn bot_hotbar_key_matches_rod_inside_mutated_name() {
        let s = slots(&[("Equipment Bag", "2"), ("Abyssal Fallen Rod", "1"), ("GPS", "5")]);
        assert_eq!(hotbar_key_for(&rods(&["Fallen Rod"]), &s), Some(1));
        assert!(hotbar_key_for(&rods(&["Rod Of The Depths"]), &s).is_none()); // rod not on the hotbar
        assert!(hotbar_key_for(&rods(&[""]), &s).is_none()); // unnamed tool never matches everything
        assert_eq!(hotbar_key_for(&rods(&["GPS"]), &slots(&[("GPS", "0")])), Some(0));
        assert!(hotbar_key_for(&rods(&["GPS"]), &slots(&[("GPS", "10")])).is_none()); // not a single key
        // rods named inside other rods (the wiki has these)
        let nolife = slots(&[("Original No-Life Rod", "1"), ("No-Life Rod", "2")]);
        assert_eq!(hotbar_key_for(&rods(&["No-Life Rod"]), &nolife), Some(2));
        assert!(hotbar_key_for(&rods(&["Bloomspire"]), &slots(&[("Bloomspire: Twisted Toxins", "1")])).is_none());
        assert_eq!(enchanted_rod("No-Life Rod", &nolife), "No-Life Rod");
    }

    #[test]
    fn reader_enchanted_rod_from_hotbar_label() {
        let s = slots(&[("Equipment Bag", "2"), ("Momentum Carrot Rod", "1")]);
        assert_eq!(enchanted_rod("Carrot Rod", &s), "[Momentum] Carrot Rod");
        assert_eq!(
            enchanted_rod("Carrot Rod", &slots(&[("Carrot Rod", "1")])),
            "Carrot Rod"
        ); // no enchant
        assert_eq!(enchanted_rod("Fallen Rod", &s), "Fallen Rod"); // not on the hotbar
        assert!(enchanted_rod("", &s).is_empty());
    }

    #[test]
    fn bot_reel_state_excludes_cast_states() {
        assert!(reel_state(Some(7)) && reel_state(Some(8)) && reel_state(Some(9)) && reel_state(Some(2)));
        for s in [Some(3), Some(4), Some(5), Some(6), None] {
            assert!(!reel_state(s));
        }
    }

    #[test]
    fn bot_hotbar_key_prefers_rods_in_order() {
        let s = slots(&[("Abyssal Fallen Rod", "1"), ("Bestiary", "2"), ("Steady Rod", "3")]);
        assert_eq!(hotbar_key_for(&rods(&["Steady Rod", "Fallen Rod"]), &s), Some(3)); // preferred first, in slot 3
        assert_eq!(hotbar_key_for(&rods(&["Kings Rod", "Fallen Rod"]), &s), Some(1)); // preferred not there: any rod
    }

    // Live: needs Roblox in Fisch + network. Read-only: no hooks, no input. The Updater works in a temp dir, so it
    // never races a running handys-fisch.
    #[test]
    #[ignore]
    fn live_reader_costs() {
        use handy_sdk::app::updater_thread::{AppStatus, Updater, production_smoke_test};
        use handy_sdk::roblox::http::win_http_get;
        use handy_sdk::roblox::process::RobloxProcess;
        use handy_sdk::util::thread::StopThread;
        use std::time::{Duration, Instant};

        if RobloxProcess::find().is_none() {
            println!("  SKIP: Roblox not running");
            return;
        }
        let u = Arc::new(Updater::new(
            Arc::new(win_http_get),
            production_smoke_test,
            std::env::temp_dir().join("handys_fisch_live"),
            crate::app::offset_keys::required_keys(),
        ));
        let uu = u.clone();
        let _t = StopThread::spawn(move |st| uu.run(st));
        for _ in 0..300 {
            if u.status() == AppStatus::Ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let Some(s) = u.session() else {
            println!("  SKIP: offsets not ready");
            return;
        };
        let mut r = GameReader::new(s);
        let st = r.sample();
        assert!(st.is_some());
        if let Some(st) = &st {
            println!("  rod='{}' phase={}", st.rod, crate::r#macro::bot::describe_phase(st));
        }
        // Average reads and microseconds of `f` over `n` calls, after one warm-up call.
        let mut cost = |what: &str, n: u32, f: &mut dyn FnMut(&mut GameReader)| {
            f(&mut r);
            let r0 = RobloxProcess::reads_on_this_thread();
            let t0 = Instant::now();
            for _ in 0..n {
                f(&mut r);
            }
            let us = t0.elapsed().as_secs_f64() * 1e6 / f64::from(n);
            let reads = (RobloxProcess::reads_on_this_thread() - r0) as f64 / f64::from(n);
            println!("  {what:<16} {reads:7.1} reads {us:9.1} us");
        };
        cost("sample", 2000, &mut |r| {
            let _ = r.sample();
        });
        cost("castPower", 2000, &mut |r| {
            let _ = r.cast_power();
        });
        cost("castFeedback", 2000, &mut |r| {
            let _ = r.cast_feedback();
        });
        cost("coins", 500, &mut |r| {
            let _ = r.coins();
        });
        cost("errorPrompt", 500, &mut |r| {
            let _ = r.error_prompt();
        });
        cost("levelProgress", 500, &mut |r| {
            let _ = r.level_progress();
        });
        cost("takeCatchPopup", 500, &mut |r| {
            let _ = r.take_catch_popup();
        });
        cost("playerInfo", 100, &mut |r| {
            let _ = r.player_info();
        });
        if let Some(pi) = r.player_info() {
            println!("  rod='{}' rod_stats={:?}", pi.rod, pi.rod_stats);
        }
    }
}
