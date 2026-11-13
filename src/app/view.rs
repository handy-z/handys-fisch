use handy_sdk::ui::dashboard::Span;
use handy_sdk::ui::theme::rgb;
use handy_sdk::util::text::group_thousands;
use windows::Win32::Foundation::COLORREF;

use crate::game::catch::{Catch, HUD_STROKE, LABEL_STROKE, Piece, catch_pieces, weather_effect};
use crate::game::fisch_names::RARITIES;
use crate::game::reader::World;

/// "ff6600 ffc738" -> those colors; anything that isn't 6 hex digits is skipped.
pub fn hex_colors(hex: &str) -> Vec<COLORREF> {
    hex.split(' ')
        .filter(|h| h.len() == 6 && h.bytes().all(|c| c.is_ascii_hexdigit()))
        .filter_map(|h| u32::from_str_radix(h, 16).ok())
        .map(|v| rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
        .collect()
}

/// One span in a game style: colors parsed (hex_colors), outlined when the stroke has a color.
pub fn game_span(text: String, color: &str, bold: bool, italic: bool, stroke: &str) -> Span {
    let mut out = Span {
        text,
        colors: hex_colors(color),
        bold,
        italic,
        ..Default::default()
    };
    if let Some(edge) = hex_colors(stroke).first() {
        out.stroke = *edge;
        out.stroke_opacity = 1.0;
    }
    out
}

/// Pieces as dashboard spans (game_span).
pub fn spans(pieces: &[Piece]) -> Vec<Span> {
    pieces
        .iter()
        .map(|p| game_span(p.text.clone(), &p.color, p.bold, p.italic, &p.stroke))
        .collect()
}

/// A catch row in the game's colors (catch_pieces); a popup that didn't parse as its plain text, nothing as "–".
pub fn catch_spans(fish: &Option<Catch>, popup: &str) -> Vec<Span> {
    match fish {
        None => vec![Span::plain(if popup.is_empty() { "–" } else { popup })],
        Some(f) => spans(&catch_pieces(f)),
    }
}

const MUTED: &str = "9a9a9a";

/// "Windy · Aurora Borealis" bold, then their wiki effects muted on a second line ("7× luck · Aurora 1%"); "–"
/// unread.
pub fn weather_spans(w: &World) -> Vec<Span> {
    if w.weather.is_empty() {
        return styled("–", "", false, "");
    }
    let mut out = styled(&w.weather.join(" · "), "", true, "");
    let effects: Vec<&str> = w
        .weather
        .iter()
        .map(|n| weather_effect(n))
        .filter(|e| !e.is_empty())
        .collect();
    if !effects.is_empty() {
        out.extend(styled(
            &format!(
                "
{}",
                effects.join(" · ")
            ),
            MUTED,
            false,
            "",
        ));
    }
    out
}

/// "Night · Winter"
pub fn time_text(w: &World) -> String {
    let parts: Vec<&str> = [w.cycle.as_str(), w.season.as_str()]
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        "–".into()
    } else {
        parts.join(" · ")
    }
}

/// Events bold, then the server multipliers ("Luck ×2 · XP ×1.5"); a muted "none" when there are neither.
pub fn boost_spans(w: &World) -> Vec<Span> {
    let boosts: Vec<String> = w.boosts.iter().map(|(n, x)| format!("{n} ×{}", times(*x))).collect();
    if w.events.is_empty() && boosts.is_empty() {
        return styled("none", MUTED, false, "");
    }
    let mut out = styled(&w.events.join(" · "), "", true, "");
    if !boosts.is_empty() {
        let sep = if w.events.is_empty() { "" } else { " · " };
        out.extend(styled(&format!("{sep}{}", boosts.join(" · ")), "", false, ""));
    }
    out
}

/// 2 -> "2", 1.15 -> "1.15"
fn times(x: f64) -> String {
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').into()
}

/// "12 Common · 3 Rare" (most first, each in its rarity's color), then muted "2 mutated · 1 shiny · 4/6 perfect"; the
/// second line only with something on it.
pub fn tally_spans(
    rarities: &[(String, i32)],
    mutated: i32,
    shiny: i32,
    sparkling: i32,
    perfect: (i32, i32),
) -> Vec<Span> {
    let mut sorted = rarities.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.1)); // stable: ties stay in first-caught order
    let mut out = Vec::new();
    for (i, (name, n)) in sorted.iter().enumerate() {
        let color = RARITIES.iter().find(|r| r.name == name).map_or("", |r| r.color);
        out.push(Span::plain(&format!("{}{n} ", if i > 0 { " · " } else { "" })));
        out.push(game_span(name.clone(), color, true, false, ""));
    }
    if out.is_empty() {
        out.push(Span::plain("–"));
    }
    let mut extra = Vec::new();
    for (n, what) in [(mutated, "mutated"), (shiny, "shiny"), (sparkling, "sparkling")] {
        if n > 0 {
            extra.push(format!("{n} {what}"));
        }
    }
    if perfect.1 > 0 {
        extra.push(format!("{}/{} perfect", perfect.0, perfect.1));
    }
    if !extra.is_empty() {
        out.extend(styled(
            &format!(
                "
{}",
                extra.join(" · ")
            ),
            MUTED,
            false,
            "",
        ));
    }
    out
}

const COINS: &str = "fffde4"; // hud/safezone/coins at rest
const XP: &str = "fff4d9"; // xpup/xp

/// The player list's color for a Bestiary percent ("58%"), as hex: pale cream to orange, linear between these
/// points, read live from the game's list (36%, 87% and 99% fell exactly on the lines). None for anything else.
pub fn bestiary_color(text: &str) -> Option<String> {
    const RAMP: [(f32, [f32; 3]); 7] = [
        (21.0, [255.0, 250.38, 227.70]),
        (38.0, [255.0, 246.64, 205.60]),
        (58.0, [255.0, 232.80, 172.72]),
        (74.0, [255.0, 210.40, 138.16]),
        (88.0, [255.0, 190.80, 107.92]),
        (94.0, [255.0, 182.40, 94.96]),
        (100.0, [255.0, 174.0, 82.0]),
    ];
    let pct: f32 = text.trim().strip_suffix('%')?.parse().ok()?;
    let pct = pct.clamp(RAMP[0].0, 100.0); // below 21% not seen: kept at the palest
    let i = RAMP.iter().rposition(|(p, _)| *p <= pct)?.min(RAMP.len() - 2);
    let ((p0, c0), (p1, c1)) = (RAMP[i], RAMP[i + 1]);
    let t = (pct - p0) / (p1 - p0);
    let ch = |k: usize| (c0[k] + (c1[k] - c0[k]) * t).round() as u8;
    Some(format!("{:02x}{:02x}{:02x}", ch(0), ch(1), ch(2)))
}

/// The GPS readout: "XYZ: 2499.3, -1108.2, 823.5 | 12.4 s/s"; the speed part only when known.
pub fn gps_text(pos: &[f32; 3], speed: Option<f32>) -> String {
    let mut text = format!("XYZ: {:.1}, {:.1}, {:.1}", pos[0], pos[1], pos[2]);
    if let Some(speed) = speed {
        text += &format!(" | {speed:04.1} s/s"); // the GPS pads to "00.0"
    }
    text
}

/// Plain text in one game color (hex as in Piece), optionally bold and outlined: the bestiary's "% Completed" grey...
pub fn styled(text: &str, color: &str, bold: bool, stroke: &str) -> Vec<Span> {
    vec![game_span(text.into(), color, bold, false, stroke)]
}

/// Game text styles, each read off the game's own label: the HUD's coins ("5,914,618 C$") bold #fffde4 (upright,
/// though the game's is italic), outlined in HUD_STROKE.
pub fn coin_spans(coins: &str) -> Vec<Span> {
    if coins.is_empty() {
        return vec![Span::plain("–")];
    }
    vec![game_span(coins.into(), COINS, true, false, HUD_STROKE)]
}

/// "≈ 13,391 C$" like the HUD's coins, then the rate plain on a second line (a dashboard tile's small line).
pub fn earned_spans(value: i64, rate: &str) -> Vec<Span> {
    vec![
        game_span(
            format!("≈ {} C$", group_thousands(value)),
            COINS,
            true,
            false,
            HUD_STROKE,
        ),
        Span::plain(&format!("\n{rate}")),
    ]
}

/// "+6,000 xp" bold in the XP popup's #fff4d9 ("+247.5xp") outlined in LABEL_STROKE, then the rate plain on a
/// second line.
pub fn xp_spans(xp: i64, rate: &str) -> Vec<Span> {
    vec![
        game_span(format!("+{} xp", group_thousands(xp)), XP, true, false, LABEL_STROKE),
        Span::plain(&format!("\n{rate}")),
    ]
}

/// "1,234 fish\n" "56 shiny" " · " "7 sparkling" " · 120 h played", shiny and sparkling in the bestiary's #ffe669
/// and #fff9a5, outlined as its mutation list is. "–" before the counts are read (fish <= 0).
pub fn lifetime_spans(fish: i64, shiny: i64, sparkling: i64, hours: i64) -> Vec<Span> {
    if fish <= 0 {
        return vec![Span::plain("–")];
    }
    vec![
        Span::plain(&format!("{} fish\n", group_thousands(fish))), // a tile: the count big, the rest small
        game_span(
            format!("{} shiny", group_thousands(shiny)),
            "ffe669",
            false,
            false,
            LABEL_STROKE,
        ), // shinypercent
        Span::plain(" · "),
        game_span(
            format!("{} sparkling", group_thousands(sparkling)),
            "fff9a5",
            false,
            false,
            LABEL_STROKE,
        ),
        Span::plain(&format!(" · {} h played", group_thousands(hours))),
    ]
}

/// "1,300/h": total over active_ms, per hour, grouped. "–/h" before 1 min of fishing (too noisy).
pub fn per_hour(total: i64, ms: i64) -> String {
    if ms < 60000 {
        return "–/h".into();
    }
    format!("{}/h", group_thousands(total * 3600000 / ms))
}

/// 3723000 -> "1:02:03".
pub fn hms(ms: i64) -> String {
    format!("{}:{:02}:{:02}", ms / 3600000, ms / 60000 % 60, ms / 1000 % 60)
}

/// The in-game pill: "Auto fish · Reeling 45%" while on; "Auto fish off · chat opened" when the bot stopped itself.
pub fn overlay_text(on: bool, phase: &str, stopped: &str) -> String {
    if on {
        return format!("Auto fish · {phase}");
    }
    if stopped.is_empty() {
        "Auto fish off".into()
    } else {
        format!("Auto fish off · {stopped}")
    }
}

/// The status line's detail when something isn't reaching disk: "3 log rows waiting: is a CSV open in Excel?" and/or
/// "config.ini not saved"; empty when all is saved.
pub fn save_warning(unsaved_rows: i32, settings_unsaved: bool) -> String {
    let mut out = String::new();
    if unsaved_rows != 0 {
        out = format!("{unsaved_rows} log rows waiting: is a CSV open in Excel?");
    }
    if settings_unsaved {
        out += if out.is_empty() { "" } else { " · " };
        out += "config.ini not saved";
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &[Span]) -> String {
        s.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn conditions_and_tally_text() {
        let w = World {
            weather: vec!["Windy".into(), "Aurora Borealis".into()],
            cycle: "Night".into(),
            season: "Winter".into(),
            events: vec!["Meteor".into()],
            boosts: vec![("Luck", 2.0), ("Mutation", 1.15)],
            ..World::default()
        };
        assert_eq!(
            text(&weather_spans(&w)),
            "Windy · Aurora Borealis
7× luck · Aurora 1%"
        );
        assert_eq!(time_text(&w), "Night · Winter");
        assert_eq!(text(&boost_spans(&w)), "Meteor · Luck ×2 · Mutation ×1.15");
        assert_eq!(text(&boost_spans(&World::default())), "none");
        assert_eq!(time_text(&World::default()), "–");
        let r = [("Common".to_owned(), 2), ("Rare".to_owned(), 5)];
        assert_eq!(
            text(&tally_spans(&r, 1, 0, 0, (3, 4))),
            "5 Rare · 2 Common
1 mutated · 3/4 perfect"
        );
        assert_eq!(text(&tally_spans(&[], 0, 0, 0, (0, 0))), "–");
    }

    #[test]
    fn view_bestiary_color_like_the_player_list() {
        // as read live from the game's list
        assert_eq!(bestiary_color("21%").as_deref(), Some("fffae4"));
        assert_eq!(bestiary_color("36%").as_deref(), Some("fff7d0"));
        assert_eq!(bestiary_color("87%").as_deref(), Some("ffc06e"));
        assert_eq!(bestiary_color("99%").as_deref(), Some("ffaf54"));
        assert_eq!(bestiary_color("100%").as_deref(), Some("ffae52"));
        assert_eq!(bestiary_color("5%").as_deref(), Some("fffae4"));
        assert!(bestiary_color("").is_none() && bestiary_color("–").is_none());
    }
    use crate::game::catch::{odds_color, parse_catch};

    #[test]
    fn view_rates_need_a_minute() {
        assert_eq!(per_hour(10, 59999), "–/h");
    }

    #[test]
    fn view_rates_per_hour() {
        assert!(per_hour(1300, 3600000) == "1,300/h" && per_hour(10, 120000) == "300/h");
    }

    #[test]
    fn view_hms() {
        assert_eq!(hms(3723000), "1:02:03");
    }

    #[test]
    fn view_save_warning() {
        assert!(save_warning(0, false).is_empty());
        assert_eq!(save_warning(3, false), "3 log rows waiting: is a CSV open in Excel?");
        assert_eq!(save_warning(0, true), "config.ini not saved");
        assert_eq!(
            save_warning(3, true),
            "3 log rows waiting: is a CSV open in Excel? · config.ini not saved"
        );
    }

    #[test]
    fn view_gps_text_like_the_game() {
        // the GPS, standing
        assert_eq!(
            gps_text(&[2499.3, -1108.2, 823.5], Some(0.0)),
            "XYZ: 2499.3, -1108.2, 823.5 | 00.0 s/s"
        );
        assert_eq!(gps_text(&[1.0, 2.0, 3.0], Some(12.44)), "XYZ: 1.0, 2.0, 3.0 | 12.4 s/s");
        assert_eq!(gps_text(&[1.0, 2.0, 3.0], None), "XYZ: 1.0, 2.0, 3.0"); // speed not known yet
    }

    #[test]
    fn view_hex_colors() {
        assert_eq!(hex_colors("ff6600 ffc738"), vec![rgb(255, 102, 0), rgb(255, 199, 56)]);
        assert!(hex_colors("").is_empty() && hex_colors("red ff00zz 12345").is_empty());
    }

    #[test]
    fn view_catch_spans_in_game_colors() {
        let c = parse_catch("You just caught a Big Carrot Mackerel at 3.9kg! (1/13)");
        assert!(c.is_some());
        let s = catch_spans(&c, "ignored when parsed");
        assert!(s.len() >= 9);
        assert!(s[0].text == "Big " && s[0].colors == hex_colors("8bff89")); // the wiki's size color
        assert!(s[1].text == "Carrot " && s[1].colors == hex_colors("ffa200")); // mutation color
        assert!(s[2].text == "Mackerel" && s[2].colors.len() == 1); // its rarity's color
        assert!(s[0].bold && s[1].bold && s[2].bold); // the game's name words are bold
        // as the game's catch line: "<b>3.9</b>" in #b3b3b3, a plain "kg", "<b>1/13</b>" in #ffe5a5 (read live)
        assert!(s[3].text == "\n" && s[4].text == "3.9" && s[4].bold && s[4].colors == hex_colors("b3b3b3"));
        assert!(s[5].text == " kg · " && s[5].colors.is_empty() && !s[5].bold);
        assert!(s[6].text == "1/13" && s[6].bold && s[6].colors == hex_colors(&odds_color(13)));
        assert!(s[8].text.starts_with("≈ ") && s[8].text.ends_with(" C$") && s[8].colors == hex_colors("a2eaa6"));
        // the catch line's own outline (black, 0.58 transparent) on its pieces; the wiki's price is not the game's text
        assert!(s[0].stroke_opacity == 1.0 && s[0].stroke == rgb(0, 0, 0) && s[5].stroke_opacity == 1.0);
        assert!(s[3].stroke_opacity == 0.0 && s[8].stroke_opacity == 0.0);
        assert_eq!(
            catch_spans(&None, "You just caught nothing"),
            vec![Span::plain("You just caught nothing")]
        );
        assert_eq!(catch_spans(&None, ""), vec![Span::plain("–")]);
    }

    #[test]
    fn view_overlay_text() {
        assert_eq!(overlay_text(true, "Reeling 45%", ""), "Auto fish · Reeling 45%");
        assert_eq!(overlay_text(false, "Off · Idle", ""), "Auto fish off");
        assert_eq!(
            overlay_text(false, "Off · Idle (stopped: chat opened)", "chat opened"),
            "Auto fish off · chat opened"
        );
    }

    #[test]
    fn view_game_styles() {
        // each read off the game's own label: hud coins, the xpup popup, the bestiary's shiny/sparkling percents
        assert_eq!(
            coin_spans("5,914,618 C$"), // bold, outlined #362D03
            vec![Span {
                text: "5,914,618 C$".into(),
                colors: hex_colors("fffde4"),
                bold: true,
                italic: false,
                stroke: rgb(0x36, 0x2d, 0x03),
                stroke_opacity: 1.0
            }]
        );
        assert_eq!(coin_spans(""), vec![Span::plain("–")]);
        let earned = earned_spans(13391, "94,893/h");
        assert!(
            earned.len() == 2 && earned[0].text == "≈ 13,391 C$" && !earned[0].italic && earned[1].text == "\n94,893/h"
        );
        let xp = xp_spans(6000, "–/h");
        assert!(
            xp.len() == 2
                && xp[1].text == "\n–/h"
                && xp[0].text == "+6,000 xp"
                && xp[0].colors == hex_colors("fff4d9")
                && xp[0].bold
        );
        let all = lifetime_spans(1234, 56, 7, 120);
        let text: String = all.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "1,234 fish\n56 shiny · 7 sparkling · 120 h played");
        assert!(all.len() == 5 && all[1].colors == hex_colors("ffe669") && all[3].colors == hex_colors("fff9a5"));
        assert_eq!(lifetime_spans(0, 0, 0, 0), vec![Span::plain("–")]);
        let level = Piece {
            text: "Level 136".into(),
            color: "ffdcac".into(),
            italic: true,
            ..Default::default()
        };
        assert_eq!(
            spans(&[level]),
            vec![Span {
                text: "Level 136".into(),
                colors: hex_colors("ffdcac"),
                italic: true,
                ..Default::default()
            }]
        ); // italic carried over
        assert_eq!(
            styled("42% overall", "787878", false, LABEL_STROKE)[0].stroke_opacity,
            1.0
        );
    }
}
