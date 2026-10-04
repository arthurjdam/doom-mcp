//! Turns raw engine state into observations for the model: a structured
//! JSON form, a compact text summary, and a PNG screenshot.

use std::fmt::Write as _;
use std::io::Cursor;

use rmcp::schemars::{self, JsonSchema};
use serde::Serialize;

use crate::engine::Engine;
use crate::engine::TICRATE;
use crate::engine::ffi::{MF_COUNTKILL, MF_SOLID, SCREEN_H, SCREEN_W, State, Thing};
use crate::engine::things::THING_TYPES;
use crate::world::nav::NavSummary;

// Engine enum values (doomdef.h, d_player.h, d_mode.h).
pub const GS_LEVEL: i32 = 0;
pub const GS_INTERMISSION: i32 = 1;
pub const GS_FINALE: i32 = 2;
pub const PST_DEAD: i32 = 1;
const COMMERCIAL: i32 = 2;

pub const WEAPON_NAMES: [&str; 9] = [
    "Fist",
    "Pistol",
    "Shotgun",
    "Chaingun",
    "Rocket Launcher",
    "Plasma Rifle",
    "BFG 9000",
    "Chainsaw",
    "Super Shotgun",
];
const KEY_NAMES: [&str; 6] = [
    "blue keycard",
    "yellow keycard",
    "red keycard",
    "blue skull key",
    "yellow skull key",
    "red skull key",
];
const POWER_NAMES: [&str; 6] = [
    "invulnerability",
    "berserk",
    "invisibility",
    "radiation suit",
    "computer map",
    "light amp",
];

/// Half of Doom's 90 degree horizontal field of view.
const HALF_FOV: f64 = 45.0;

#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    /// One of: title, menu, playing, dead, intermission, finale.
    pub status: &'static str,
    /// What to do next, when the situation needs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    pub level_time_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<Player>,
    pub things: Vec<ThingInfo>,
    /// What happened during the last action (pickups, damage, kills...).
    pub events: Vec<String>,
    /// Given once when a level starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub briefing: Option<String>,
    /// Where the route to the current navigation goal goes next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigation: Option<NavSummary>,
    /// The model's own plan (set_plan), echoed back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Player {
    pub health: i32,
    pub armor: i32,
    pub weapon: &'static str,
    pub weapons: Vec<String>,
    pub ammo: Ammo,
    pub keys: Vec<&'static str>,
    pub powerups: Vec<&'static str>,
    pub x: i32,
    pub y: i32,
    /// Facing direction in degrees: 0 = east, 90 = north, 180 = west, 270 = south.
    pub angle: f64,
    pub kills: String,
    pub items: String,
    pub secrets: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Ammo {
    pub bullets: String,
    pub shells: String,
    pub rockets: String,
    pub cells: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThingInfo {
    /// Stable id for this level; pass to act's `aim_at`.
    pub id: u32,
    pub name: &'static str,
    pub category: &'static str,
    pub distance: i32,
    /// Degrees from the crosshair: positive = to your right, negative = left.
    pub bearing: f64,
    /// In line of sight and within the field of view (i.e. on screen).
    pub visible: bool,
    pub line_of_sight: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<i32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    /// The monster has noticed you and is hunting or shooting at you.
    pub targeting_you: bool,
}

pub fn thing_type(t: i32) -> (&'static str, &'static str) {
    THING_TYPES
        .get(t as usize)
        .copied()
        .unwrap_or(("Unknown", "other"))
}

pub fn level_name(s: &State) -> String {
    if s.gamemode == COMMERCIAL {
        format!("MAP{:02}", s.map)
    } else {
        format!("E{}M{}", s.episode, s.map)
    }
}

pub fn status(s: &State) -> (&'static str, Option<&'static str>) {
    if s.menuactive != 0 {
        return (
            "menu",
            Some("A menu is open and the game is paused. press_keys [\"escape\"] closes it."),
        );
    }
    match s.gamestate {
        GS_LEVEL if s.demoplayback != 0 => (
            "title",
            Some("This is the attract-mode demo. Call new_game to start playing."),
        ),
        GS_LEVEL if s.in_level != 0 && s.playerstate == PST_DEAD => (
            "dead",
            Some("You died. Call act with use=true to restart the level."),
        ),
        GS_LEVEL => ("playing", None),
        GS_INTERMISSION => (
            "intermission",
            Some(
                "Level complete! Call act with use=true (a few times) to continue to the next level.",
            ),
        ),
        GS_FINALE => (
            "finale",
            Some("Episode finale. Call act with use=true to continue."),
        ),
        _ => (
            "title",
            Some("Title screen. Call new_game to start playing."),
        ),
    }
}

fn is_interesting(t: &Thing, category: &str) -> bool {
    match category {
        "monster" => t.health > 0 && (t.line_of_sight != 0 || t.distance < 384.0),
        "projectile" => t.line_of_sight != 0,
        "barrel" => t.health > 0 && t.line_of_sight != 0,
        "player" | "other" | "obstacle" => false,
        // Pickups.
        _ => t.line_of_sight != 0 || t.distance < 256.0,
    }
}

/// Things around the player. With `everything`, includes pickups and
/// obstacles regardless of distance or sight (for the map); otherwise only
/// what is worth mentioning in an observation.
pub fn things(engine: &mut Engine, radius: f64, limit: usize, everything: bool) -> Vec<ThingInfo> {
    let mut out: Vec<ThingInfo> = engine
        .things(radius)
        .into_iter()
        .filter_map(|t| {
            let (name, mut category) = thing_type(t.type_);
            if category == "other" && t.flags & MF_SOLID != 0 {
                category = "obstacle";
            }
            let keep = if everything {
                !matches!(category, "player" | "other") && !(category == "monster" && t.health <= 0)
            } else {
                is_interesting(&t, category)
            };
            if !keep {
                return None;
            }
            let los = t.line_of_sight != 0;
            Some(ThingInfo {
                id: engine.thing_id(t.id),
                name,
                category,
                distance: t.distance.round() as i32,
                bearing: (t.bearing * 10.0).round() / 10.0,
                visible: los && t.bearing.abs() <= HALF_FOV,
                line_of_sight: los,
                health: (t.flags & MF_COUNTKILL != 0).then_some(t.health),
                targeting_you: t.targeting_player != 0,
            })
        })
        .collect();
    // Threats first, then whatever is closest.
    out.sort_by(|a, b| {
        let rank = |t: &ThingInfo| match t.category {
            "monster" | "projectile" if t.line_of_sight => 0,
            "monster" | "projectile" => 1,
            _ => 2,
        };
        rank(a).cmp(&rank(b)).then(a.distance.cmp(&b.distance))
    });
    out.truncate(limit);
    out
}

fn player(s: &State) -> Option<Player> {
    if s.gamestate != GS_LEVEL || s.in_level == 0 || s.demoplayback != 0 {
        return None;
    }
    let ammo = |i: usize| format!("{}/{}", s.ammo[i], s.maxammo[i]);
    Some(Player {
        health: s.health,
        armor: s.armor,
        weapon: WEAPON_NAMES
            .get(s.readyweapon as usize)
            .copied()
            .unwrap_or("?"),
        weapons: (0..9)
            .filter(|&i| s.weaponowned[i] != 0)
            .map(|i| format!("{} (key {})", WEAPON_NAMES[i], weapon_slot(i)))
            .collect(),
        ammo: Ammo {
            bullets: ammo(0),
            shells: ammo(1),
            cells: ammo(2),
            rockets: ammo(3),
        },
        keys: (0..6)
            .filter(|&i| s.cards[i] != 0)
            .map(|i| KEY_NAMES[i])
            .collect(),
        powerups: (0..6)
            .filter(|&i| s.powers[i] != 0)
            .map(|i| POWER_NAMES[i])
            .collect(),
        x: s.x.round() as i32,
        y: s.y.round() as i32,
        angle: (s.angle * 10.0).round() / 10.0,
        kills: format!("{}/{}", s.kills, s.totalkills),
        items: format!("{}/{}", s.items, s.totalitems),
        secrets: format!("{}/{}", s.secrets, s.totalsecrets),
    })
}

/// Number key that selects weapon index `i`.
fn weapon_slot(i: usize) -> u8 {
    match i {
        7 => 1, // chainsaw shares the fist slot
        8 => 3, // super shotgun shares the shotgun slot
        _ => i as u8 + 1,
    }
}

/// Describe what changed between two states.
pub fn diff_events(before: &State, after: &State, events: &mut Vec<String>) {
    if before.gamestate != after.gamestate
        || before.map != after.map
        || before.episode != after.episode
    {
        return;
    }
    if after.in_level == 0 || before.in_level == 0 {
        return;
    }
    let lost = before.health - after.health;
    if lost > 0 {
        let mut e = format!(
            "You took {lost} damage (health {} -> {})",
            before.health, after.health
        );
        if after.has_attacker != 0 {
            let (name, _) = thing_type(after.attacker_type);
            let _ = write!(
                e,
                ", attacked by {name} at bearing {:+.0}°",
                after.attacker_bearing
            );
        }
        events.push(e);
    }
    if after.kills > before.kills {
        events.push(format!("Killed {} monster(s)", after.kills - before.kills));
    }
    if after.secrets > before.secrets {
        events.push("Found a secret!".into());
    }
    if before.playerstate != PST_DEAD && after.playerstate == PST_DEAD {
        events.push("You died.".into());
    }
}

pub fn observe(engine: &mut Engine, mut events: Vec<String>) -> Observation {
    let s = engine.state();
    let (status, hint) = status(&s);
    events.extend(engine.take_messages());
    let things = if status == "playing" {
        things(engine, 2048.0, 12, false)
    } else {
        Vec::new()
    };
    Observation {
        status,
        hint,
        level: (s.gamestate == GS_LEVEL || s.gamestate == GS_INTERMISSION).then(|| level_name(&s)),
        level_time_seconds: (s.leveltime as f64 / TICRATE as f64 * 10.0).round() / 10.0,
        player: player(&s),
        things,
        events,
        briefing: None,
        navigation: None,
        plan: None,
    }
}

impl Observation {
    pub fn to_text(&self) -> String {
        let mut t = String::new();
        let _ = write!(t, "Status: {}", self.status);
        if let Some(level) = &self.level {
            let _ = write!(t, " | Level {level} | time {:.1}s", self.level_time_seconds);
        }
        t.push('\n');
        if let Some(hint) = self.hint {
            let _ = writeln!(t, "Hint: {hint}");
        }
        if let Some(b) = &self.briefing {
            let _ = writeln!(t, "\n{b}\n");
        }
        if let Some(p) = &self.player {
            let _ = writeln!(
                t,
                "Health {} | Armor {} | Weapon {} | Ammo: bullets {}, shells {}, rockets {}, cells {}",
                p.health,
                p.armor,
                p.weapon,
                p.ammo.bullets,
                p.ammo.shells,
                p.ammo.rockets,
                p.ammo.cells
            );
            let _ = writeln!(t, "Weapons: {}", p.weapons.join(", "));
            let keys = if p.keys.is_empty() {
                "none".to_string()
            } else {
                p.keys.join(", ")
            };
            let _ = write!(t, "Keys: {keys}");
            if !p.powerups.is_empty() {
                let _ = write!(t, " | Powerups: {}", p.powerups.join(", "));
            }
            t.push('\n');
            let _ = writeln!(
                t,
                "Position ({}, {}) facing {:.0}° | Kills {} Items {} Secrets {}",
                p.x, p.y, p.angle, p.kills, p.items, p.secrets
            );
        }
        if !self.events.is_empty() {
            let _ = writeln!(t, "Events: {}", self.events.join("; "));
        }
        if let Some(n) = &self.navigation {
            let _ = write!(t, "Navigation → {}: ", n.goal);
            match (&n.next, n.route_length) {
                (Some(next), Some(len)) => {
                    let _ = write!(
                        t,
                        "route {len}u, {} waypoint(s). Next: {}u at bearing {:+.1}° ({}).",
                        n.waypoints_left, next.distance, next.bearing, next.what
                    );
                }
                _ => t.push_str("no route."),
            }
            if let Some(note) = &n.note {
                let _ = write!(t, " {note}");
            }
            t.push('\n');
        }
        match &self.plan {
            Some(plan) => {
                let _ = writeln!(t, "Your plan:\n{}", plan.trim_end());
            }
            None if self.status == "playing" => {
                t.push_str("Your plan: (none yet; write one with set_plan)\n");
            }
            None => {}
        }
        if self.status == "playing" {
            if self.things.is_empty() {
                t.push_str("Nothing notable in sight.\n");
            } else {
                t.push_str("Things (bearing: + = right of crosshair, - = left):\n");
                for th in &self.things {
                    let _ = write!(
                        t,
                        "  [{}] {} ({}) {}u, bearing {:+.1}°",
                        th.id, th.name, th.category, th.distance, th.bearing
                    );
                    if th.visible {
                        t.push_str(", ON SCREEN");
                    } else if th.line_of_sight {
                        t.push_str(", in line of sight (off screen)");
                    } else {
                        t.push_str(", not in sight");
                    }
                    if let Some(hp) = th.health {
                        let _ = write!(t, ", hp {hp}");
                    }
                    if th.targeting_you {
                        t.push_str(", ATTACKING YOU");
                    }
                    t.push('\n');
                }
            }
        }
        t
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotSize {
    /// 320x240
    Small,
    /// 640x480
    #[default]
    Medium,
    /// 960x720
    Large,
}

impl ScreenshotSize {
    fn dims(self) -> (usize, usize) {
        match self {
            Self::Small => (320, 240),
            Self::Medium => (640, 480),
            Self::Large => (960, 720),
        }
    }
}

/// Encode the frame as PNG, scaled to the 4:3 aspect Doom was designed for.
/// A small crosshair marks the centre of the 3D view while playing.
/// `waypoint` is the bearing of the next route waypoint, drawn as a yellow
/// marker (or an edge arrow when it's outside the field of view).
pub fn screenshot_png(
    engine: &Engine,
    size: ScreenshotSize,
    crosshair: bool,
    waypoint: Option<f64>,
) -> Vec<u8> {
    let mut src = engine.frame_rgb();
    if crosshair {
        draw_crosshair(&mut src);
    }
    if let Some(bearing) = waypoint {
        draw_waypoint(&mut src, bearing);
    }
    let (w, h) = size.dims();
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let sy = y * SCREEN_H / h;
        for x in 0..w {
            let sx = x * SCREEN_W / w;
            let i = (sy * SCREEN_W + sx) * 3;
            out.extend_from_slice(&src[i..i + 3]);
        }
    }

    let mut png_bytes = Vec::new();
    let mut encoder = png::Encoder::new(Cursor::new(&mut png_bytes), w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&out).expect("png data");
    writer.finish().expect("png finish");
    png_bytes
}

fn draw_waypoint(rgb: &mut [u8], bearing: f64) {
    const YELLOW: [u8; 3] = [255, 220, 0];
    const OUTLINE: [u8; 3] = [0, 0, 0];
    let cy = (SCREEN_H as i32 - 32) / 2;
    let mut put = |x: i32, y: i32, c: [u8; 3]| {
        if x >= 0 && y >= 0 && (x as usize) < SCREEN_W && (y as usize) < SCREEN_H - 32 {
            let i = ((y as usize) * SCREEN_W + x as usize) * 3;
            rgb[i..i + 3].copy_from_slice(&c);
        }
    };
    if bearing.abs() < 44.0 {
        // Perspective projection with Doom's 90 degree field of view.
        let x = (SCREEN_W as f64 / 2.0 * (1.0 + bearing.to_radians().tan())).round() as i32;
        // Diamond, outlined so it reads on any background.
        for r in [5, 4] {
            let c = if r == 5 { OUTLINE } else { YELLOW };
            for d in 0..=r {
                for (dx, dy) in [(d, r - d), (-d, r - d), (d, d - r), (-d, d - r)] {
                    put(x + dx, cy + dy, c);
                }
            }
        }
    } else {
        // Triangle at the screen edge pointing toward the waypoint.
        let (base, dir) = if bearing > 0.0 {
            (SCREEN_W as i32 - 9, 1)
        } else {
            (8, -1)
        };
        for col in 0..7 {
            let half = 6 - col;
            for dy in -half..=half {
                put(base + dir * col, cy + dy, YELLOW);
            }
        }
    }
}

fn draw_crosshair(rgb: &mut [u8]) {
    // Centre of the 3D view above the 32px status bar.
    let (cx, cy) = (SCREEN_W as i32 / 2, (SCREEN_H as i32 - 32) / 2);
    let mut put = |x: i32, y: i32| {
        let i = ((y as usize) * SCREEN_W + x as usize) * 3;
        rgb[i..i + 3].copy_from_slice(&[0, 255, 0]);
    };
    for d in 2..=4 {
        put(cx + d, cy);
        put(cx - d, cy);
        put(cx, cy + d);
        put(cx, cy - d);
    }
}
