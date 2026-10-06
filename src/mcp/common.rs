//! What both MCP front-ends share: parameter types, turning game state into
//! tool results, spectator-message delivery, and the tools that work the
//! same in both modes (new game, map, plan, keys).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use rmcp::ErrorData;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars::{self, JsonSchema};
use serde::Deserialize;

use crate::engine::{Engine, TICRATE};
use crate::game::Game;
use crate::game::session::Session;
use crate::viewer::{self, LogKind};
use crate::world::map::{self, MapOptions};
use crate::world::nav::{Goal, KeyColor};
use crate::world::observe::{self, Observation, ScreenshotSize};

pub fn default_true() -> bool {
    true
}
fn default_skill() -> i32 {
    3
}
fn default_one() -> i32 {
    1
}
fn default_cells() -> usize {
    41
}
fn default_cell_size() -> f64 {
    32.0
}
fn default_gap() -> u32 {
    4
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ObserveParams {
    /// Include a screenshot in the result. Defaults to true.
    #[serde(default = "default_true")]
    pub screenshot: bool,
    #[serde(default)]
    pub screenshot_size: ScreenshotSize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NewGameParams {
    /// 1 = I'm too young to die, 2 = Hey, not too rough, 3 = Hurt me plenty (default),
    /// 4 = Ultra-Violence, 5 = Nightmare!
    #[serde(default = "default_skill")]
    pub skill: i32,
    /// Episode 1-4 (Doom 1 only; the shareware WAD only has episode 1).
    #[serde(default = "default_one")]
    pub episode: i32,
    /// Map number: 1-9 in Doom 1, 1-32 in Doom 2.
    #[serde(default = "default_one")]
    pub map: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MapParams {
    /// Grid width/height in characters (11-81, default 41).
    #[serde(default = "default_cells")]
    pub cells: usize,
    /// Map units per character (8-256, default 32). The player is 32 units wide;
    /// a typical room is 256-1024 units across.
    #[serde(default = "default_cell_size")]
    pub cell_size: f64,
    /// Also draw walls you haven't seen yet and things out of sight (like the
    /// computer map powerup). Defaults to false.
    #[serde(default)]
    pub reveal: bool,
    /// Draw with north up (walls stay axis-aligned and easier to read) instead of
    /// rotating the map so the direction you face is up. Defaults to false.
    #[serde(default)]
    pub north_up: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PressKeysParams {
    /// Keys to tap in order. Named keys: enter, escape, up, down, left, right, tab,
    /// space, backspace, fire, use, f1-f12, pause. Any other single character
    /// (letters, digits, punctuation) is typed as-is, e.g. ["i","d","k","f","a"].
    pub keys: Vec<String>,
    /// Tics to wait after each key (default 4).
    #[serde(default = "default_gap")]
    pub gap_tics: u32,
    /// Include a screenshot in the result. Defaults to true.
    #[serde(default = "default_true")]
    pub screenshot: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanParams {
    /// Your plan, as short plain text (a checklist works well).
    pub plan: String,
}

/// A navigation goal as a tool parameter.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RouteGoal {
    /// The level exit (switch or walk-over line).
    Exit,
    SecretExit,
    BlueKey,
    YellowKey,
    RedKey,
    /// The nearest switch you haven't used yet.
    Switch,
    /// The nearest area you haven't seen yet.
    Explore,
    /// A thing from the observation list; set thing_id.
    Thing,
    /// A map coordinate; set x and y.
    Point,
}

/// Turn a goal parameter (plus its thing id / coordinates) into a `Goal`.
pub fn goal_from(
    goal: RouteGoal,
    thing_id: Option<u32>,
    x: Option<f64>,
    y: Option<f64>,
) -> Result<Goal, String> {
    Ok(match goal {
        RouteGoal::Exit => Goal::Exit,
        RouteGoal::SecretExit => Goal::SecretExit,
        RouteGoal::BlueKey => Goal::Key(KeyColor::Blue),
        RouteGoal::YellowKey => Goal::Key(KeyColor::Yellow),
        RouteGoal::RedKey => Goal::Key(KeyColor::Red),
        RouteGoal::Switch => Goal::Switch,
        RouteGoal::Explore => Goal::Explore,
        RouteGoal::Thing => Goal::Thing(thing_id.ok_or("goal \"thing\" needs thing_id")?),
        RouteGoal::Point => match (x, y) {
            (Some(x), Some(y)) => Goal::Point(x, y),
            _ => return Err("goal \"point\" needs x and y".into()),
        },
    })
}

#[derive(Clone)]
pub struct ViewerConfig {
    pub url: String,
    /// Open the browser on the first `new_game`.
    pub auto_open: bool,
    pub opened: Arc<AtomicBool>,
}

/// Wording that differs between the two modes.
#[derive(Clone, Copy)]
pub struct ModeText {
    /// Closing line of a level briefing.
    pub briefing_next: &'static str,
}

/// What every tool handler needs: the game, the viewer, and the mode's wording.
#[derive(Clone)]
pub struct Core {
    pub game: Game,
    pub viewer: Option<ViewerConfig>,
    pub text: ModeText,
}

pub fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

impl Core {
    /// Run `f` on the game thread. Engine failures (e.g. the game was quit
    /// from the menu) become tool errors so the model gets to read them.
    pub async fn run<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&mut Session) -> anyhow::Result<CallToolResult> + Send + 'static,
    {
        // Any tool call means the commander is still there (real-time mode
        // pauses when they go quiet).
        let result = match self
            .game
            .with(move |session| {
                session.touch();
                f(session)
            })
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(e)) | Err(e) => tool_error(e.to_string()),
        };
        Ok(result)
    }

    /// Note for the model about the spectator view; opens it on first use.
    pub fn viewer_note(&self) -> Option<String> {
        let v = self.viewer.as_ref()?;
        let first = !v.opened.swap(true, Ordering::Relaxed);
        Some(if first && v.auto_open {
            viewer::open_browser(&v.url);
            format!(
                "Live spectator view opened in the user's browser: {} (the human can watch you play there).",
                v.url
            )
        } else {
            format!("Live spectator view for the human: {}", v.url)
        })
    }

    /// Start a new game at the given skill/episode/map. `prepare` runs on the
    /// game thread just before the game starts (e.g. to reset the pilot).
    pub async fn new_game(
        &self,
        p: NewGameParams,
        prepare: fn(&mut Session),
    ) -> Result<CallToolResult, ErrorData> {
        let briefing_next = self.text.briefing_next;
        let mut result = self
            .run(move |session| {
                let commercial = session.state().gamemode == 2;
                let shareware = session.state().gamemode == 0;
                if !(1..=5).contains(&p.skill) {
                    return Ok(tool_error("skill must be 1-5"));
                }
                let (episode, max_map) = if commercial { (1, 32) } else { (p.episode, 9) };
                if !(1..=max_map).contains(&p.map) {
                    return Ok(tool_error(format!("map must be 1-{max_map}")));
                }
                if !commercial && !(1..=if shareware { 1 } else { 4 }).contains(&episode) {
                    return Ok(tool_error(if shareware {
                        "the shareware WAD only has episode 1".to_string()
                    } else {
                        "episode must be 1-4".to_string()
                    }));
                }
                viewer::log(
                    LogKind::Action,
                    &format!(
                        "new game · skill {} · episode {episode} map {}",
                        p.skill, p.map
                    ),
                );
                prepare(session);
                session.new_game(p.skill - 1, episode, p.map)?;
                Ok(observation_result(
                    session,
                    Vec::new(),
                    Some(ScreenshotSize::Medium),
                    true,
                    briefing_next,
                ))
            })
            .await?;
        if result.is_error != Some(true)
            && let Some(note) = self.viewer_note()
        {
            result.content.push(ContentBlock::text(note));
        }
        Ok(result)
    }

    pub async fn get_map(&self, p: MapParams) -> Result<CallToolResult, ErrorData> {
        let opts = MapOptions {
            cells: p.cells.clamp(11, 81),
            cell_size: p.cell_size.clamp(8.0, 256.0),
            reveal: p.reveal,
            north_up: p.north_up,
        };
        self.run(move |session| {
            session.check_alive()?;
            let route: Vec<(f64, f64)> = session
                .route()
                .map(|r| r.waypoints.iter().map(|w| (w.x, w.y)).collect())
                .unwrap_or_default();
            Ok(CallToolResult::success(vec![ContentBlock::text(
                map::render(session, &opts, &route),
            )]))
        })
        .await
    }

    pub async fn set_plan(&self, p: PlanParams) -> Result<CallToolResult, ErrorData> {
        let plan = p.plan.trim().to_string();
        if plan.is_empty() || plan.len() > 2000 {
            return Ok(tool_error("plan must be 1-2000 characters"));
        }
        self.run(move |session| {
            let s = session.state();
            session.plan = Some((plan.clone(), (s.episode, s.map)));
            viewer::publish_plan(&plan);
            viewer::log(LogKind::Comment, "Updated the plan");
            Ok(CallToolResult::success(vec![ContentBlock::text(
                "Plan saved. It will be shown in every observation.",
            )]))
        })
        .await
    }

    pub async fn press_keys(&self, p: PressKeysParams) -> Result<CallToolResult, ErrorData> {
        if p.keys.is_empty() || p.keys.len() > 64 {
            return Ok(tool_error("keys must contain 1-64 entries"));
        }
        let gap = p.gap_tics.clamp(1, TICRATE * 2);
        let briefing_next = self.text.briefing_next;
        self.run(move |session| {
            let mut codes = Vec::new();
            for name in &p.keys {
                match key_code(name, session) {
                    Some(c) => codes.push(c),
                    None => return Ok(tool_error(format!("unknown key {name:?}"))),
                }
            }
            viewer::log(LogKind::Action, &format!("keys: {}", p.keys.join(" ")));
            session.press_keys(&codes, gap)?;
            Ok(observation_result(
                session,
                Vec::new(),
                p.screenshot.then_some(ScreenshotSize::Medium),
                true,
                briefing_next,
            ))
        })
        .await
    }
}

/// Observation text + optional screenshot + structured JSON, as one tool result.
/// With `spectate`, the outcome is also written to the spectator log.
pub fn observation_result(
    session: &mut Session,
    events: Vec<String>,
    screenshot: Option<ScreenshotSize>,
    spectate: bool,
    briefing_next: &str,
) -> CallToolResult {
    if let Err(e) = session.check_alive() {
        viewer::log(LogKind::Result, &e.to_string());
        return tool_error(e.to_string());
    }
    let briefing = session.take_briefing(briefing_next);
    let mut obs = observe::observe(session, events);
    obs.briefing = briefing;
    if obs.status == "playing" {
        obs.navigation = session.nav_summary();
        let s = session.state();
        obs.plan = session.plan_text(&s);
    }
    if spectate {
        viewer::log(LogKind::Result, &spectator_summary(&obs));
    }
    session.publish_spectator(obs.navigation.as_ref());
    let waypoint = obs
        .navigation
        .as_ref()
        .and_then(|n| n.next.as_ref())
        .map(|n| n.bearing);
    let mut content = vec![ContentBlock::text(obs.to_text())];
    if let Some(size) = screenshot {
        let png = observe::screenshot_png(session, size, obs.status == "playing", waypoint);
        content.push(ContentBlock::image(
            base64::engine::general_purpose::STANDARD.encode(png),
            "image/png",
        ));
    }
    let mut result = CallToolResult::success(content);
    result.structured_content = serde_json::to_value(&obs).ok();
    result
}

/// "50/200" -> "50".
fn count(ammo: &str) -> &str {
    ammo.split('/').next().unwrap_or(ammo)
}

/// One line for the spectator log describing the outcome of an action.
pub fn spectator_summary(obs: &Observation) -> String {
    let mut parts = Vec::new();
    if let Some(pl) = obs
        .player
        .as_ref()
        .filter(|_| matches!(obs.status, "playing" | "dead"))
    {
        parts.push(format!(
            "Health {} · Armor {} · {}{} · Kills {}",
            pl.health,
            pl.armor,
            pl.weapon,
            weapon_ammo(pl),
            pl.kills
        ));
    }
    if obs.status != "playing" {
        parts.push(format!("[{}]", obs.status));
    }
    parts.extend(
        obs.events
            .iter()
            .filter(|e| !e.starts_with("Moved "))
            .cloned(),
    );
    parts.join(" — ")
}

/// " (14 shells)" for the weapon in hand, or "" for melee weapons.
pub fn weapon_ammo(pl: &observe::Player) -> String {
    match pl.weapon {
        "Pistol" | "Chaingun" => format!(" ({} bullets)", count(&pl.ammo.bullets)),
        "Shotgun" | "Super Shotgun" => format!(" ({} shells)", count(&pl.ammo.shells)),
        "Rocket Launcher" => format!(" ({} rockets)", count(&pl.ammo.rockets)),
        "Plasma Rifle" | "BFG 9000" => format!(" ({} cells)", count(&pl.ammo.cells)),
        _ => String::new(),
    }
}

/// Compass name for a map angle (0 = east, 90 = north).
fn compass(angle: f64) -> &'static str {
    const NAMES: [&str; 8] = [
        "east",
        "north-east",
        "north",
        "north-west",
        "west",
        "south-west",
        "south",
        "south-east",
    ];
    NAMES[((angle.rem_euclid(360.0) + 22.5) / 45.0) as usize % 8]
}

/// The full route to the current goal, waypoint by waypoint. `closing` is a
/// last line on how to follow it.
pub fn describe_route(session: &mut Session, closing: &str) -> String {
    let goal = session.goal.describe();
    let route = match session.route() {
        Ok(r) => r,
        Err(e) => return format!("Navigation goal set to {goal}, but: {e}"),
    };
    let s = session.state();
    let mut t = format!(
        "Navigation goal: {goal}. Route: {:.0} units, {} waypoint(s){}.\n",
        route.length,
        route.waypoints.len(),
        if route.crosses_hazard {
            ", crosses a damaging floor"
        } else {
            ""
        }
    );
    if let Some(pre) = &route.prerequisite {
        let _ = writeln!(t, "{pre} This route leads to {}.", route.goal);
    }
    let mut prev = (s.x, s.y);
    for (i, w) in route.waypoints.iter().enumerate() {
        let leg = (w.x - prev.0).hypot(w.y - prev.1);
        let heading = (w.y - prev.1).atan2(w.x - prev.0).to_degrees();
        let _ = write!(
            t,
            "{}. {:.0}u heading {} ({:.0}°) to ({:.0}, {:.0}): {}",
            i + 1,
            leg,
            compass(heading),
            heading.rem_euclid(360.0),
            w.x,
            w.y,
            w.kind.describe()
        );
        if i == 0 {
            let _ = write!(
                t,
                " [bearing {:+.1}° from your crosshair]",
                crate::world::nav::bearing_to(&s, w.x, w.y)
            );
        }
        t.push('\n');
        prev = (w.x, w.y);
    }
    if let Some(hint) = &route.arrival_hint {
        let _ = writeln!(t, "On arrival: {hint}");
    }
    t.push_str(closing);
    t
}

fn key_code(name: &str, engine: &Engine) -> Option<i32> {
    let k = engine.key_bindings();
    let lower = name.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => 13,
        "escape" | "esc" => 27,
        "tab" => 9,
        "space" => b' ' as i32,
        "backspace" => 0x7f,
        "up" => 0xad,
        "down" => 0xaf,
        "left" => 0xac,
        "right" => 0xae,
        "fire" => k.fire,
        "use" => k.use_,
        "pause" => 0xff,
        f if f.starts_with('f') && f.len() > 1 => {
            let n: i32 = f[1..].parse().ok()?;
            match n {
                1..=10 => 0x80 + 0x3a + n,
                11 => 0x80 + 0x57,
                12 => 0x80 + 0x58,
                _ => return None,
            }
        }
        _ => {
            let mut chars = lower.chars();
            let c = chars.next()?;
            if chars.next().is_some() || !c.is_ascii_graphic() {
                return None;
            }
            c as i32
        }
    };
    Some(code)
}
