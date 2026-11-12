use super::fisch_names::{ENCHANTS, EnchantInfo, MUTATIONS, RARITIES, RODS, SIZE_COLORS, SPECIES, SpeciesInfo};

/// One line of Fisch's catch popup: "You just caught a Big Fallen Bigeye Trevally at 3.5kg! (1/12)".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catch {
    /// Tiny / Small / Big / Giant, or empty
    pub size: String,
    /// its one mutation plus stacking attributes (Shiny, Sparkling, Glitched...)
    pub mutations: Vec<String>,
    /// "Bigeye Trevally"
    pub species: String,
    /// 0: the popup gave no weight ("Rare Find!")
    pub kg: f64,
    /// the "(1/N)" catch chance
    pub odds: i32,
    // From the wiki (tools/update_fisch_names.py), only for species it knows:
    /// "Common"
    pub rarity: String,
    /// "Coral Bastion"
    pub location: String,
    /// base stats, % as the wiki shows them: "85", "+65" (empty: none)
    pub resilience: String,
    pub progress_speed: String,
    /// estimated sell price, C$
    pub value: Option<f64>,
    /// from the game's "+45xp" popup, filled in by the bot
    pub xp: Option<i64>,
}

// Text outline colors as the game draws them (a UIStroke, or the label's own TextStroke), read live with
// tools/debug/ui_style_probe.py. Drawn opaque, as the wiki draws them: the game's see-through edges vanish on a dark
// card.
/// the catch line's TextStroke (0.58 transparent in game)
pub const CATCH_STROKE: &str = "000000";
/// the HUD's coins and level (30% transparent)
pub const HUD_STROKE: &str = "362d03";
/// most names and titles in the game's panels (36% transparent)
pub const LABEL_STROKE: &str = "000000";

/// A piece of dashboard text in the game's style: color hex "rrggbb", several space-separated for a gradient, empty for
/// plain text; stroke: its outline's color, empty for none.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Piece {
    pub text: String,
    pub color: String,
    pub bold: bool,
    pub italic: bool,
    pub stroke: String,
}

fn piece(text: impl Into<String>, color: &str, bold: bool, italic: bool, stroke: &str) -> Piece {
    Piece {
        text: text.into(),
        color: color.into(),
        bold,
        italic,
        stroke: stroke.into(),
    }
}

/// "Sizes" on the wiki: the fish's weight against its base weight.
const SIZES: [&str; 4] = ["Tiny", "Small", "Big", "Giant"];

/// Strips leading size/mutation words (longest name first: "Tidal Surge" before "Tidal") until none match.
/// True when all of s was consumed; otherwise s keeps the unmatched rest.
fn peel_prefixes(s: &mut &str, c: &mut Catch) -> bool {
    loop {
        let (mut best, mut is_size) = ("", false);
        let words = SIZES
            .iter()
            .map(|w| (*w, true))
            .chain(MUTATIONS.iter().map(|m| (m.name, false)));
        for (w, size) in words {
            if w.len() > best.len()
                && (*s == w || (s.len() > w.len() && s.starts_with(w) && s.as_bytes()[w.len()] == b' '))
            {
                (best, is_size) = (w, size);
            }
        }
        if best.is_empty() {
            return s.is_empty();
        }
        if is_size {
            c.size = best.into();
        } else {
            c.mutations.push(best.into());
        }
        *s = &s[s.len().min(best.len() + 1)..];
    }
}

/// std::from_chars over the whole text: no leading '+', and a value out of range fails.
fn whole_f64(n: &str) -> Option<f64> {
    if n.starts_with('+') {
        return None;
    }
    let v: f64 = n.parse().ok()?;
    let inf_text = n.trim_start_matches('-').to_ascii_lowercase().starts_with("inf");
    (v.is_finite() || v.is_nan() || inf_text).then_some(v)
}

fn whole_i32(n: &str) -> Option<i32> {
    if n.starts_with('+') { None } else { n.parse().ok() }
}

/// Commas dropped, then the whole text as a number.
fn number_f64(text: &str) -> Option<f64> {
    whole_f64(&text.replace(',', ""))
}

fn number_i32(text: &str) -> Option<i32> {
    whole_i32(&text.replace(',', ""))
}

pub fn parse_catch(popup: &str) -> Option<Catch> {
    // Event items (seen live): "You just got a Jack-o' Admin Relic at 21kg!", no odds and not on the wiki.
    const HEAD: &str = "You just caught a";
    const ITEM_HEAD: &str = "You just got a";
    // The game can put a word before the line (seen live: "Extra! You just caught a ..."), so skip any lead-in.
    let (head, item) = match popup.find(HEAD) {
        Some(h) => (h + HEAD.len(), false),
        None => (popup.find(ITEM_HEAD)? + ITEM_HEAD.len(), true),
    };
    let mut s = &popup[head..];
    if s.starts_with('n') {
        s = &s[1..]; // "an"
    }
    s = s.strip_prefix(' ')?;
    let mut c = Catch::default();
    match s.rfind(" at ") {
        None => {
            // no weight (seen live): "Rare Find! You just caught a Shell! (1/100)"
            const BANG: &str = "! (1/";
            let bang = s.rfind(BANG);
            let bang = bang.filter(|_| !item && s.ends_with(')'))?;

            c.odds = number_i32(&s[bang + BANG.len()..s.len() - 1])?;
            s = &s[..bang];
        }
        Some(at) => {
            // "3.5kg! (1/12)"; big numbers get thousands separators: "15,505kg! (1/9,662)"
            let tail = &s[at + 4..];
            if item {
                if at == 0 || !tail.ends_with("kg!") {
                    return None;
                }
                c.kg = number_f64(&tail[..tail.len() - 3])?;
                c.species = s[..at].into(); // the game's name as is: it drops letters ("a xperimental Admin Relic")
                return Some(c);
            }
            const MID: &str = "kg! (1/";
            let mid = tail.find(MID)?;
            if !tail.ends_with(')') {
                return None;
            }
            c.kg = number_f64(&tail[..mid])?;
            c.odds = number_i32(&tail[mid + MID.len()..tail.len() - 1])?;
            s = &s[..at];
        }
    }
    // Species first: 255 species start with a mutation word ("Abyssal Goliath"), so take the longest known species
    // the name ends with whose prefix is all size/mutation words. New fish not on the wiki yet: peel words greedily.
    let mut info: Option<&SpeciesInfo> = None;
    for sp in SPECIES {
        let n = sp.name;
        if info.is_some_and(|i| n.len() <= i.name.len()) || !s.ends_with(n) {
            continue;
        }
        if s.len() > n.len() && s.as_bytes()[s.len() - n.len() - 1] != b' ' {
            continue;
        }
        let mut t = Catch::default();
        let mut prefix = if s.len() > n.len() {
            &s[..s.len() - n.len() - 1]
        } else {
            ""
        };
        if peel_prefixes(&mut prefix, &mut t) && prefix.is_empty() {
            info = Some(sp);
            c.size = t.size;
            c.mutations = t.mutations;
        }
    }
    let Some(info) = info else {
        peel_prefixes(&mut s, &mut c);
        if s.is_empty() {
            return None;
        }
        c.species = s.into();
        return Some(c);
    };
    c.species = info.name.into();
    c.rarity = info.rarity.into();
    c.location = info.location.into();
    c.resilience = info.resilience.into();
    c.progress_speed = info.progress_speed.into();
    let mut mult = 1.0;
    for m in &c.mutations {
        for known in MUTATIONS {
            if known.name == m {
                mult *= known.multiplier;
            }
        }
    }
    if info.base_weight > 0.0 && c.kg > 0.0 {
        c.value = Some(sell_value(info.base_value, info.base_weight, c.kg, mult));
    }
    Some(c)
}

/// An item handed to you, not a catch, so it isn't counted (no weight, seen live): "You just got a Whale Egg!",
/// "You just got 10× Smokescreen Totem!".
pub fn is_item_popup(s: &str) -> bool {
    s.contains("You just got ") && !s.contains(" at ")
}

/// The XP popup's text: "+45xp", "+1,450xp", "+247.5xp" (the fraction is dropped).
pub fn parse_xp(text: &str) -> Option<i64> {
    let n = text.replace(',', "");
    let n = n.strip_prefix('+').unwrap_or(&n);
    // from_chars reads the number, and the rest must be "xp" ('x' can't continue a number)
    let v = whole_f64(n.strip_suffix("xp")?)?;
    if !(0.0..1e15).contains(&v) {
        return None; // also NaN: no real popup is near this, and the cast needs a range
    }
    Some(v as i64)
}

/// Wiki formula: ceil(ceil(baseValue / baseWeight * kg) * product of mutation/attribute multipliers).
pub fn sell_value(base_value: f64, base_weight: f64, kg: f64, multiplier: f64) -> f64 {
    let up = |x: f64| (x - 1e-9).ceil(); // 100 * 1.1 = 110.00000000000001 must stay 110
    up(up(base_value / base_weight * kg) * multiplier)
}

/// "15505.5" -> "15,505.5"
fn with_commas(mut n: String) -> String {
    let mut i = n.find('.').unwrap_or(n.len());
    while i > 3 {
        n.insert(i - 3, ',');
        i -= 3;
    }
    n
}

/// printf("%.10g"): as the game shows a weight, 0.1 and not 0.100000.
pub(crate) fn kg_text(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "nan".into()
        } else if x < 0.0 {
            "-inf".into()
        } else {
            "inf".into()
        };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let trim = |s: &str| -> String {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').into()
        } else {
            s.into()
        }
    };
    let e = format!("{x:.9e}"); // 10 significant digits, rounded: its exponent picks the style
    let (mantissa, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..10).contains(&exp) {
        format!("{}e{}{:02}", trim(mantissa), if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        trim(&format!("{:.*}", (9 - exp) as usize, x))
    }
}

/// The color the game gives "1/odds" in its catch line: pale yellow #fff6b2 shading to red as the odds near 1/1.
pub fn odds_color(odds: i32) -> String {
    // Fitted on 12 live popups (1/7 #ffd79b ... 1/1,429 #fff6b2), within 3 per channel.
    let p = if odds > 0 { 1.0 / f64::from(odds) } else { 0.0 };
    let ch = |pale: i32, red: i32| (f64::from(pale) + f64::from(red - pale) * p).round() as i32;
    format!("{:02x}{:02x}{:02x}", ch(0xff, 0xff), ch(0xf6, 0x12), ch(0xb2, 0x0c))
}

/// "Big " "Fallen " "Bigeye Trevally" (bold, in their size, mutation and rarity colors from the wiki), " · "
/// "Legendary" in the tier's color, "\n", "15,505" (bold grey) " kg · " "1/9,662" (bold, odds_color), the game's parts
/// outlined as the catch line is (CATCH_STROKE); then " · " "≈ 612 C$" (the shops' price green). Numbers are grouped
/// like the game; unknown parts and the Common tier left out.
pub fn catch_pieces(c: &Catch) -> Vec<Piece> {
    // As the game's catch line styles it (its rich text, read live): name words bold in their size, mutation and
    // rarity colors, the weight number bold #b3b3b3 before a plain "kg", the odds bold in odds_color.
    let mut tier = ""; // the species' rarity color
    for r in RARITIES {
        if r.name == c.rarity {
            tier = r.color;
        }
    }
    let mut out = Vec::new();
    // the game's catch line, outlined as it is
    let line = |out: &mut Vec<Piece>, text: String, color: &str, bold: bool| {
        out.push(piece(text, color, bold, false, CATCH_STROKE));
    };
    for (i, size) in SIZES.iter().enumerate() {
        if *size == c.size {
            line(&mut out, format!("{} ", c.size), SIZE_COLORS[i], true);
        }
    }
    for m in &c.mutations {
        let mut color = "";
        for known in MUTATIONS {
            if known.name == m {
                color = known.color;
            }
        }
        line(&mut out, format!("{m} "), color, true);
    }
    line(&mut out, c.species.clone(), tier, true);
    // most catches are Common: only the rarer tiers are worth a word
    if !c.rarity.is_empty() && c.rarity != "Common" {
        out.push(piece(" · ", "", false, false, ""));
        out.push(piece(c.rarity.clone(), tier, false, false, ""));
    }
    out.push(piece("\n", "", false, false, ""));
    if c.kg > 0.0 {
        // "Rare Find!" catches have no weight
        line(&mut out, with_commas(kg_text(c.kg)), "b3b3b3", true);
        line(&mut out, (if c.odds != 0 { " kg · " } else { " kg" }).into(), "", false); // event items have no odds
    }
    if c.odds != 0 {
        line(
            &mut out,
            format!("1/{}", with_commas(c.odds.to_string())),
            &odds_color(c.odds),
            true,
        );
    }
    if let Some(v) = c.value {
        // the popup has no price: the shops' C$ green (#a2eaa6 on their price labels)
        out.push(piece(" · ", "", false, false, ""));
        out.push(piece(
            format!("≈ {} C$", with_commas((v as i64).to_string())),
            "a2eaa6",
            false,
            false,
            "",
        ));
    }
    out
}

/// The wiki's stats for a rod: "Control 0.12 · Resilience 30 · Lure 70 · Luck 95 · Max 80,000 kg"; empty if unknown.
pub fn rod_stats(rod: &str) -> String {
    for r in RODS {
        if r.name == rod {
            return format!(
                "Control {} · Resilience {} · Lure {} · Luck {} · Max {} kg",
                r.control, r.resilience, r.lure, r.luck, r.max_weight
            );
        }
    }
    String::new()
}

/// XP to go from `level` to the next (wiki: 190 × level, doubled every 500 levels from 1000).
pub fn level_xp(level: i32) -> i64 {
    (190 * i64::from(level)) << (if level >= 1000 { (level - 1000) / 500 + 1 } else { 0 })
}

/// "Level 122" (the HUD's #ffdcac, HUD_STROKE) " · 42% · " "9,800 / 23,180 xp" (the XP popup's #fff4d9) from the level
/// and the XP into it.
pub fn level_pieces(level: i32, xp: f64) -> Vec<Piece> {
    let need = level_xp(level);
    let have = (xp as i64).clamp(0, need); // xp is fractional
    vec![
        piece(format!("Level {level}"), "ffdcac", false, false, HUD_STROKE),
        piece(format!(" · {}% · ", have * 100 / need), "", false, false, ""),
        piece(
            format!(
                "{} / {} xp",
                with_commas(have.to_string()),
                with_commas(need.to_string())
            ),
            "fff4d9",
            false,
            false,
            "",
        ),
    ]
}

fn find_enchant(enchant: &str) -> Option<&'static EnchantInfo> {
    let mut key = String::new(); // the wiki's CSS name: "Glimmering Crown" -> "glimmering-crown"
    for ch in enchant.bytes() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_lowercase() as char);
        } else if !key.is_empty() && !key.ends_with('-') {
            key.push('-');
        }
    }
    while key.ends_with('-') {
        key.pop();
    }
    ENCHANTS.iter().find(|e| e.name == key)
}

/// A rod as the game's equipment panel shows it, from the label "[Momentum] Carrot Rod": "★ Momentum ★ " bold
/// in the enchant's color and the wiki's outline for it (else LABEL_STROKE), "Carrot Rod" bold and outlined,
/// " (not equipped)" plain; then each stat line on its own line,
/// bold #afafaf with a bonus "(+28%)" in #ffd43a ("Lure Speed: 85% <font color="#ffd43a">(+28%)</font>").
pub fn rod_pieces(label: &str, equipped: bool, stats: &[String]) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rod = label;
    if label.starts_with('[') {
        // "[Momentum] Carrot Rod": the panel shows "★ Momentum ★" over the rod
        if let Some(close) = label.find("] ") {
            let enchant = &label[1..close];
            let e = find_enchant(enchant);
            let stroke = e.map(|e| e.stroke).filter(|s| !s.is_empty()).unwrap_or(LABEL_STROKE);
            out.push(piece(
                format!("★ {enchant} ★ "),
                e.map_or("", |e| e.color),
                true,
                false,
                stroke,
            ));
            rod = &label[close + 2..];
        }
    }
    out.push(piece(rod, "", true, false, LABEL_STROKE));
    if !equipped {
        out.push(piece(" (not equipped)", "", false, false, ""));
    }
    for s in stats {
        out.push(piece("\n", "", false, false, "")); // one to a line, as the game lists them
        let bonus = if s.ends_with(')') { s.rfind(" (") } else { None };
        out.push(piece(&s[..bonus.map_or(s.len(), |b| b + 1)], "afafaf", true, false, ""));
        if let Some(b) = bonus {
            out.push(piece(&s[b + 1..], "ffd43a", true, false, ""));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(pieces: &[Piece]) -> String {
        // the text, styles dropped
        pieces.iter().map(|p| p.text.as_str()).collect()
    }
    fn plain_catch(c: &Catch) -> String {
        plain(&catch_pieces(c))
    }
    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn catch_parses_size_mutation_species() {
        let c = parse_catch("You just caught a Big Fallen Bigeye Trevally at 3.5kg! (1/12)").unwrap();
        assert_eq!(c.size, "Big");
        assert_eq!(c.mutations, strs(&["Fallen"]));
        assert_eq!(c.species, "Bigeye Trevally");
        assert!(c.kg == 3.5 && c.odds == 12);
        assert!(plain_catch(&c).starts_with("Big Fallen Bigeye Trevally · Unusual\n3.5 kg · 1/12"));
    }

    #[test]
    fn catch_plain_and_multiword_species() {
        let c = parse_catch("You just caught a Stalactite at 7.9kg! (1/4)").unwrap();
        assert!(c.size.is_empty() && c.mutations.is_empty() && c.species == "Stalactite" && c.odds == 4);
        assert!(plain_catch(&c).starts_with("Stalactite · Trash\n7.9 kg · 1/4"));
        let c = parse_catch("You just caught a Fallen Regal Angelfish at 1kg! (1/37)").unwrap();
        assert!(c.mutations == strs(&["Fallen"]) && c.species == "Regal Angelfish" && c.kg == 1.0);
    }

    #[test]
    fn catch_longest_and_stacked_mutations() {
        let c = parse_catch("You just caught an Abyssal Tidal Surge Shiny Cod at 12kg! (1/900)").unwrap();
        assert!(c.mutations == strs(&["Abyssal", "Tidal Surge", "Shiny"]) && c.species == "Cod");
        assert!(plain_catch(&c).starts_with("Abyssal Tidal Surge Shiny Cod · Uncommon\n12 kg · 1/900"));
    }

    #[test]
    fn catch_with_lead_in() {
        // seen live: "Extra! You just caught a Abyssal Deep-sea Hatchetfish ..."
        let c = parse_catch("Extra! You just caught a Abyssal Deep-sea Hatchetfish at 3.2kg! (1/5)").unwrap();
        assert!(c.species == "Deep-sea Hatchetfish" && c.mutations == strs(&["Abyssal"]) && c.kg == 3.2 && c.odds == 5);
        let c = parse_catch("Extra! You just got a Jack-o' Admin Relic at 21kg!").unwrap();
        assert!(c.species == "Jack-o' Admin Relic");
    }

    #[test]
    fn catch_item_popup_is_not_a_catch() {
        assert!(is_item_popup("You just got a Whale Egg!")); // seen live
        assert!(is_item_popup("Extra! You just got a Whale Egg!"));
        assert!(is_item_popup("You just got 10× Smokescreen Totem!")); // seen live
        assert!(!is_item_popup("You just got a Jack-o' Admin Relic at 21kg!")); // fished up: it has a weight
        assert!(!is_item_popup("You just caught a Cod at 1kg! (1/2)"));
        assert!(!is_item_popup("Some new popup")); // unknown text stays visible, as it was how "Extra!" was found
    }

    #[test]
    fn catch_xp_popup() {
        assert_eq!(parse_xp("+45xp"), Some(45));
        assert_eq!(parse_xp("+1,450xp"), Some(1450));
        assert_eq!(parse_xp("+247.5xp"), Some(247)); // used to read 2475
        assert_eq!(parse_xp("+51187.5xp"), Some(51187));
        assert_eq!(parse_xp("+3,000,000,000xp"), Some(3000000000)); // past int: used to wrap negative
        assert!(parse_xp("").is_none());
        assert!(parse_xp("xp").is_none());
        assert!(parse_xp("-45xp").is_none());
        assert!(parse_xp("+45").is_none());
        assert!(parse_xp("+1e300xp").is_none());
    }

    #[test]
    fn catch_rare_find_has_no_weight() {
        // seen live
        let c = parse_catch("Rare Find! You just caught a Lunar Thread! (1/1,000)").unwrap();
        assert!(c.odds == 1000 && c.kg == 0.0 && c.value.is_none()); // not on the wiki, so "Lunar" reads as a mutation
        assert_eq!(plain_catch(&c), "Lunar Thread\n1/1,000");
        let c = parse_catch("Rare Find! You just caught a Shell! (1/100)").unwrap();
        assert!(c.species == "Shell" && c.odds == 100 && plain_catch(&c).ends_with("\n1/100"));
        let c = parse_catch("Rare Find! You just caught a Shiny Cod! (1/100)").unwrap(); // known species: no price
        assert!(c.species == "Cod" && c.mutations == strs(&["Shiny"]) && c.value.is_none());
        assert!(parse_catch("Rare Find! You just caught a ! (1/100)").is_none());
        assert!(parse_catch("Rare Find! You just caught a Shell! (1/)").is_none());
        assert!(parse_catch("Rare Find! You just caught a Shell! (1/100").is_none());
        assert!(parse_catch("You just got a Shell! (1/100)").is_none());
    }

    #[test]
    fn catch_rejects_other_text() {
        assert!(parse_catch("").is_none());
        assert!(parse_catch("Ancient Weapon Parts: 0/3").is_none());
        assert!(parse_catch("You just caught a Cod at 1kg!").is_none()); // no odds
        assert!(parse_catch("You just caught a Cod at 1kg! (1/2) x").is_none()); // trailing junk
    }

    #[test]
    fn catch_event_item_without_odds() {
        // seen live
        let c = parse_catch("You just got a Jack-o' Admin Relic at 21kg!").unwrap();
        assert!(c.species == "Jack-o' Admin Relic" && c.kg == 21.0 && c.odds == 0 && c.mutations.is_empty());
        assert_eq!(plain_catch(&c), "Jack-o' Admin Relic\n21 kg");
        let c = parse_catch("You just got a xperimental Admin Relic at 21kg!").unwrap();
        assert!(c.species == "xperimental Admin Relic");
        assert!(parse_catch("You just got a at 21kg!").is_none());
        assert!(parse_catch("You just got a Relic at 21kg! (1/2)").is_none());
    }

    #[test]
    fn catch_species_that_start_with_a_mutation_word() {
        let c = parse_catch("You just caught a Abyssal Goliath at 500kg! (1/2000)").unwrap(); // not Abyssal + Goliath
        assert!(c.mutations.is_empty() && c.species == "Abyssal Goliath");
        let c = parse_catch("You just caught a Big Fallen Abyssal Goliath at 900kg! (1/9000)").unwrap();
        assert!(c.size == "Big" && c.mutations == strs(&["Fallen"]) && c.species == "Abyssal Goliath");
        let c = parse_catch("You just caught a Giant Grouper at 60kg! (1/20)").unwrap(); // starts with a size word
        assert!(c.size.is_empty() && c.species == "Giant Grouper");
        let c = parse_catch("You just caught a Giant Shiny Giant Grouper at 200kg! (1/90)").unwrap();
        assert!(c.size == "Giant" && c.mutations == strs(&["Shiny"]) && c.species == "Giant Grouper");
    }

    #[test]
    fn catch_all_sizes_and_unlisted_species() {
        for sz in ["Tiny", "Small", "Big", "Giant"] {
            let c = parse_catch(&format!("You just caught a {sz} Stalactite at 1kg! (1/4)")).unwrap();
            assert!(c.size == sz && c.species == "Stalactite");
        }
        let c = parse_catch("You just caught a Small Unsellable Brandnewfish at 1kg! (1/3)").unwrap(); // not on the wiki
        assert!(c.size == "Small" && c.mutations == strs(&["Unsellable"]) && c.species == "Brandnewfish");
    }

    #[test]
    fn catch_weight_with_thousands_separator() {
        let c = parse_catch("You just caught a Big Fallen Sand Tiger Shark at 15,505kg! (1/22)").unwrap(); // seen live
        assert!(c.kg == 15505.0 && c.odds == 22 && c.species == "Sand Tiger Shark");
        assert!(plain_catch(&c).starts_with("Big Fallen Sand Tiger Shark · Mythical\n15,505 kg · 1/22"));
        let c = parse_catch("You just caught a Cod at 1,234,567.5kg! (1/2)").unwrap();
        assert!(c.kg > 1234567.0 && c.kg < 1234568.0);
        assert!(parse_catch("You just caught a Cod at 1,2x5kg! (1/2)").is_none());
    }

    #[test]
    fn catch_odds_with_thousands_separator() {
        let c = parse_catch("You just caught a Shiny Squalled Seaweed at 0.1kg! (1/9,662)").unwrap(); // seen live
        assert!(c.odds == 9662 && c.mutations == strs(&["Shiny", "Squalled"]) && c.species == "Seaweed");
        assert!(parse_catch("You just caught a Cod at 1kg! (1/)").is_none());
        assert!(parse_catch("You just caught a Cod at kg! (1/2)").is_none());
    }

    #[test]
    fn catch_text_groups_digits_like_the_game() {
        let c = parse_catch("You just caught a Fallen Sand Tiger Shark at 11,345.8kg! (1/126)").unwrap();
        assert!(plain_catch(&c).starts_with("Fallen Sand Tiger Shark · Mythical\n11,345.8 kg · 1/126"));
        let c = parse_catch("You just caught a Shiny Squalled Seaweed at 0.1kg! (1/9,662)").unwrap();
        assert!(plain_catch(&c).starts_with("Shiny Squalled Seaweed · Trash\n0.1 kg · 1/9,662"));
        let c = parse_catch("You just caught a Cod at 999kg! (1/1,234,567)").unwrap();
        assert!(plain_catch(&c).starts_with("Cod · Uncommon\n999 kg · 1/1,234,567"));
    }

    #[test]
    fn catch_value_rarity_location_from_wiki() {
        assert_eq!(sell_value(110.0, 7.0, 5.8, 1.85), 171.0); // fischipedia's Shiny Grey Carp example: 92 C$ * 1.85
        assert_eq!(sell_value(100.0, 10.0, 10.0, 1.1), 110.0); // float noise must not round up
        let c = parse_catch("You just caught a Big Fallen Batfish at 11.6kg! (1/9)").unwrap();
        assert!(c.rarity == "Common" && c.location == "Coral Bastion");
        assert_eq!(c.value, Some(612.0)); // ceil(ceil(70 / 8 * 11.6) * 6); "Big" doesn't change the price
        let c = parse_catch("You just caught a Shiny Grey Carp at 5.8kg! (1/50)").unwrap();
        assert_eq!(c.value, Some(171.0));
        let c = parse_catch("You just caught a Brandnewfish at 1kg! (1/3)").unwrap(); // not on the wiki: no info
        assert!(c.rarity.is_empty() && c.value.is_none());
    }

    #[test]
    fn catch_details_line() {
        let mut c = parse_catch("You just caught a Big Fallen Batfish at 11.6kg! (1/9)").unwrap();
        assert!(plain_catch(&c).ends_with("11.6 kg · 1/9 · ≈ 612 C$")); // Common: no tier word
        c.rarity = "Legendary".into();
        assert!(plain_catch(&c).ends_with(" · Legendary\n11.6 kg · 1/9 · ≈ 612 C$")); // the tier follows the name
        let p = catch_pieces(&c);
        assert!(p.iter().any(|x| x.text == "Legendary" && x.color == "f0b56d"));
    }

    #[test]
    fn catch_and_rod_stats_from_wiki() {
        let c = parse_catch("You just caught a Blobfish at 8kg! (1/100)").unwrap();
        assert!(c.resilience == "100" && c.progress_speed == "+65");
        let c = parse_catch("You just caught a Batfish at 8kg! (1/10)").unwrap(); // no progress speed on the wiki
        assert!(c.resilience == "85" && c.progress_speed.is_empty());
        assert_eq!(
            rod_stats("Fallen Rod"),
            "Control 0.12 · Resilience 30 · Lure 70 · Luck 95 · Max 80,000 kg"
        );
        assert_eq!(
            rod_stats("Kings Rod"),
            "Control 0.15 · Resilience 35 · Lure 30 · Luck 85 · Max ∞ kg"
        );
        assert!(rod_stats("Not A Rod").is_empty());
    }

    #[test]
    fn catch_level_xp_follows_the_wiki() {
        assert_eq!(level_xp(1), 190);
        assert_eq!(level_xp(122), 23180);
        assert_eq!(level_xp(999), 189810);
        assert_eq!(level_xp(1000), 380000); // wiki table: level 1001 costs 1000 × 190 × 2
        assert_eq!(level_xp(1500), 1140000); // × 4 from level 1501
        assert_eq!(
            plain(&level_pieces(123, 7244.49)),
            "Level 123 · 30% · 7,244 / 23,370 xp"
        ); // the game's xp is fractional
        assert_eq!(plain(&level_pieces(123, -1.0)), "Level 123 · 0% · 0 / 23,370 xp");
        let p = level_pieces(123, 0.0);
        assert!(p[0].color == "ffdcac" && p.last().unwrap().color == "fff4d9"); // the HUD's
        assert!(p[0].stroke == HUD_STROKE);
    }

    #[test]
    fn catch_odds_color_matches_the_game() {
        // the game's catch line, read live: odds -> color of its "1/odds"
        let seen = [
            (7, "ffd79b"),
            (7, "ffd399"),
            (9, "ffdda0"),
            (13, "ffe5a5"),
            (14, "ffe6a7"),
            (35, "fff0ad"),
            (155, "fff5b1"),
            (300, "fff5b1"),
            (1429, "fff6b2"),
        ];
        for (odds, hex) in seen {
            let got = odds_color(odds);
            let mut close = got.len() == 6;
            for i in (0..6).step_by(2) {
                if !close {
                    break;
                }
                let ch = |s: &str| i32::from_str_radix(&s[i..i + 2], 16).unwrap();
                close = (ch(&got) - ch(hex)).abs() <= 4;
            }
            assert!(close, "{odds}: {got}"); // 1/7 shows two colors in game (its exact chance differs): within 4 of both
        }
    }

    #[test]
    fn catch_rod_pieces_like_the_equipment_panel() {
        // the panel's lines for a Momentum Carrot Rod, read live
        let p = rod_pieces(
            "[Momentum] Carrot Rod",
            true,
            &strs(&["Lure Speed: 85% (+28%)", "Luck: 125%", "Caught: 737"]),
        );
        assert_eq!(
            plain(&p),
            "★ Momentum ★ Carrot Rod\nLure Speed: 85% (+28%)\nLuck: 125%\nCaught: 737"
        );
        assert!(p.len() >= 5);
        assert_eq!(p[0], piece("★ Momentum ★ ", "d3b380", true, false, LABEL_STROKE)); // no outline on the wiki
        assert_eq!(p[1], piece("Carrot Rod", "", true, false, LABEL_STROKE));
        assert!(
            p[3] == piece("Lure Speed: 85% ", "afafaf", true, false, "")
                && p[4] == piece("(+28%)", "ffd43a", true, false, "")
        );
        assert_eq!(
            plain(&rod_pieces("Carrot Rod", false, &[])),
            "Carrot Rod (not equipped)"
        );
        // the wiki outlines some enchants: Chaotic is near-black with a white edge
        assert_eq!(
            rod_pieces("[Chaotic] Carrot Rod", true, &[])[0],
            piece("★ Chaotic ★ ", "141316", true, false, "ffffff")
        );
    }
}
