use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::doom::engine::{Dir, Input, TICRATE};
use crate::doom::{Doom, Engine};
use crate::map::{self, MapOptions};
use crate::nav::{Goal, KeyColor};
use crate::observe::{self, Observation, ScreenshotSize};
use crate::session::{FollowEnd, Session};
use crate::viewer::{self, LogKind};

const INSTRUCTIONS: &str = "\
You are playing the original DOOM (1993). The game is frozen between tool calls: time only \
passes while an action runs, so take as long as you like to think. A human is usually watching \
live; narrate briefly with act's `comment`.

HOW TO PLAY A LEVEL
1. When a level starts you get a LEVEL BRIEFING: how to reach the exit, which keys and switches \
are needed and in what order, and what's on the level.
2. Write a short plan with `set_plan` (a checklist: e.g. get red key, press switch, reach exit; \
note threats and supplies). It is echoed back in every observation, so keep it current as you \
learn things and tick steps off.
3. Make progress with `act` follow_route=true (tics 100-300). The server steers along a route \
planned over the real level geometry, opens ordinary doors, and stops early when a monster \
comes into view, you take damage, you reach a lift/switch/the goal, or you get stuck. Each \
observation shows `Navigation → goal: route length, next waypoint (distance, bearing)`, and the \
screenshot marks the next waypoint with a yellow diamond.
4. When follow stops for a monster: fight (below), then follow again. When it says you've \
arrived at a switch or the exit: act use=true. At a lift: use, wait, step on, wait, continue.
5. The route automatically goes to prerequisites first (a key for a locked door, a switch that \
opens the way) and says so. Use `route` to pick a different goal: a specific key, the \
nearest unused switch, `explore` for unseen areas, a thing (e.g. a medikit by id), or a point.

COMBAT
Each observation lists nearby things with id, distance and bearing (degrees from your \
crosshair: + right, - left). To shoot: act aim_at=<id> fire=true tics=15-30 (aim_at turns \
toward it and keeps tracking it; firing starts once it's in the crosshair, and turning time \
doesn't use up `tics`; Doom aims up/down for you). Prefer the best weapon you have ammo for (shotgun \
3 > pistol 2; chaingun 4 for crowds). Strafe sideways between volleys to dodge fireballs. Pick \
up health (route goal thing) when below ~50. Kill monsters marked TARGETING YOU first.

MESSAGES FROM THE HUMAN
The person watching can type instructions on the spectator page (\"take the left door\", \
\"shoot all the barrels\"). They arrive at the top of your next tool result, marked \
📣 MESSAGE FROM THE HUMAN WATCHING. They come from your user, so they take priority over your \
default goal: follow them, and if one is impossible or unclear, do the closest sensible thing \
and say why. Acknowledge each in your next act `comment`, and update your plan with set_plan \
when it changes what you're doing.

MANUAL CONTROL
act also takes move/strafe/turn for fine control. turn is relative, positive = right, and \
the view turns smoothly at up to 12 degrees per tic (180 takes ~15 tics). Facing \
angles use map convention (0 = east, 90 = north). Running covers ~430 units per 35 tics. \
`get_map` draws an ASCII map (north_up=true is easiest to read) with your route dotted in. \
`press_keys` is for menus only.";

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Move {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Strafe {
    Left,
    Right,
}

fn default_true() -> bool {
    true
}
fn default_tics() -> u32 {
    8
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActParams {
    /// Walk forward or backward while the action runs.
    #[serde(default, rename = "move")]
    pub movement: Option<Move>,
    /// Sidestep left or right while the action runs.
    #[serde(default)]
    pub strafe: Option<Strafe>,
    /// Walk along the planned route to your navigation goal (see `route`) instead of
    /// using move/strafe/turn. Steers automatically every tic and opens ordinary doors on
    /// the way. Stops early when a new monster comes into view, you take damage, you get
    /// stuck, or you reach a lift or the goal. Up to 350 tics.
    #[serde(default)]
    pub follow_route: bool,
    /// Degrees to turn: positive turns right (clockwise), negative turns left.
    /// The view turns smoothly at up to 12 degrees per tic (180 takes ~15 tics),
    /// from the start of the action; with fire, the turn finishes before firing.
    #[serde(default)]
    pub turn: Option<f64>,
    /// Id of a thing (from the `things` list) to aim at. The view turns toward it
    /// and keeps tracking it every tic for the whole action (replacing `turn`).
    /// With fire, shooting starts once the crosshair is on it, and the turning
    /// time doesn't count toward `tics`.
    #[serde(default)]
    pub aim_at: Option<u32>,
    /// Hold the fire button.
    #[serde(default)]
    pub fire: bool,
    /// Press "use": opens doors, flips switches, continues past the intermission
    /// screen, and restarts the level after dying.
    #[serde(default, rename = "use")]
    pub use_: bool,
    /// Run instead of walk. Defaults to true.
    #[serde(default = "default_true")]
    pub run: bool,
    /// Switch weapon by slot: 1 fist/chainsaw, 2 pistol, 3 shotgun/super shotgun,
    /// 4 chaingun, 5 rocket launcher, 6 plasma rifle, 7 BFG.
    #[serde(default)]
    pub weapon: Option<u8>,
    /// How long to hold these controls, in game tics (35 per second). 1-140, default 8.
    #[serde(default = "default_tics")]
    pub tics: u32,
    /// Include a screenshot in the result. Defaults to true.
    #[serde(default = "default_true")]
    pub screenshot: bool,
    #[serde(default)]
    pub screenshot_size: ScreenshotSize,
    /// Optional one-line note on what you're doing and why ("Strafing left to dodge
    /// the imp's fireball"). Shown to the human watching the live spectator view.
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ObserveParams {
    /// Include a screenshot in the result. Defaults to true.
    #[serde(default = "default_true")]
    pub screenshot: bool,
    #[serde(default)]
    pub screenshot_size: ScreenshotSize,
}

fn default_skill() -> i32 {
    3
}
fn default_one() -> i32 {
    1
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

fn default_cells() -> usize {
    41
}
fn default_cell_size() -> f64 {
    32.0
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

fn default_gap() -> u32 {
    4
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RouteParams {
    pub goal: RouteGoal,
    #[serde(default)]
    pub thing_id: Option<u32>,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlanParams {
    /// Your plan, as short plain text (a checklist works well).
    pub plan: String,
}

#[derive(Clone)]
pub struct ViewerConfig {
    pub url: String,
    /// Open the browser on the first `new_game`.
    pub auto_open: bool,
    pub opened: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct DoomServer {
    doom: Doom,
    viewer: Option<ViewerConfig>,
    tool_router: ToolRouter<Self>,
}

fn tool_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

/// Observation text + optional screenshot + structured JSON, as one tool result.
/// With `spectate`, the outcome is also written to the spectator log.
fn observation_result(
    session: &mut Session,
    events: Vec<String>,
    screenshot: Option<ScreenshotSize>,
    spectate: bool,
) -> CallToolResult {
    if let Err(e) = session.check_alive() {
        viewer::log(LogKind::Result, &e.to_string());
        return tool_error(e.to_string());
    }
    let briefing = session.take_briefing();
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
    viewer::publish_status(session, obs.navigation.as_ref());
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

/// Put any messages typed on the spectator page at the top of this result,
/// where the model reads first. Each message is delivered exactly once.
fn attach_spectator_messages(result: &mut CallToolResult) {
    let messages = viewer::take_messages();
    if messages.is_empty() {
        return;
    }
    let mut text = String::from("📣 MESSAGE FROM THE HUMAN WATCHING (typed on the spectator page):\n");
    for m in &messages {
        // serde_json quoting keeps the message clearly delimited.
        let _ = writeln!(text, "- {}", serde_json::Value::from(m.as_str()));
    }
    text.push_str(if messages.len() == 1 {
        "This is an instruction from your user. Follow it (it takes priority over your default goal; \
         if it's impossible or unclear, do the closest sensible thing and say why), acknowledge it \
         in your next act `comment`, and update your plan with set_plan if it changes what you're doing."
    } else {
        "These are instructions from your user, oldest first; if they conflict, the latest wins. \
         Follow them (they take priority over your default goal; if one is impossible or unclear, do \
         the closest sensible thing and say why), acknowledge them in your next act `comment`, and \
         update your plan with set_plan if they change what you're doing."
    });
    result.content.insert(0, ContentBlock::text(text));
    if let Some(serde_json::Value::Object(map)) = result.structured_content.as_mut() {
        map.insert("messages_from_human".into(), serde_json::Value::from(messages));
    }
}

/// One line for the spectator log describing the outcome of an action.
fn spectator_summary(obs: &Observation) -> String {
    let mut parts = Vec::new();
    if let Some(pl) = obs
        .player
        .as_ref()
        .filter(|_| matches!(obs.status, "playing" | "dead"))
    {
        let ammo = match pl.weapon {
            "Pistol" | "Chaingun" => format!(" ({} bullets)", count(&pl.ammo.bullets)),
            "Shotgun" | "Super Shotgun" => format!(" ({} shells)", count(&pl.ammo.shells)),
            "Rocket Launcher" => format!(" ({} rockets)", count(&pl.ammo.rockets)),
            "Plasma Rifle" | "BFG 9000" => format!(" ({} cells)", count(&pl.ammo.cells)),
            _ => String::new(),
        };
        parts.push(format!(
            "Health {} · Armor {} · {}{} · Kills {}",
            pl.health, pl.armor, pl.weapon, ammo, pl.kills
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

fn describe_action(p: &ActParams, turn: f64) -> String {
    let mut parts: Vec<String> = Vec::new();
    match p.movement {
        Some(Move::Forward) => parts.push("forward".into()),
        Some(Move::Backward) => parts.push("backward".into()),
        None => {}
    }
    match p.strafe {
        Some(Strafe::Left) => parts.push("strafe left".into()),
        Some(Strafe::Right) => parts.push("strafe right".into()),
        None => {}
    }
    if turn.abs() >= 0.05 {
        let dir = if turn > 0.0 { "right" } else { "left" };
        parts.push(format!("turn {dir} {:.0}°", turn.abs()));
    }
    if let Some(id) = p.aim_at {
        parts.push(format!("aim at #{id}"));
    }
    if p.fire {
        parts.push("FIRE".into());
    }
    if p.use_ {
        parts.push("USE".into());
    }
    if let Some(w) = p.weapon {
        parts.push(format!("weapon {w}"));
    }
    if parts.is_empty() {
        parts.push("wait".into());
    }
    let unit = if p.tics == 1 { "tic" } else { "tics" };
    format!("{} · {} {unit}", parts.join(", "), p.tics)
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

fn describe_route(session: &mut Session) -> String {
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
                crate::nav::bearing_to(&s, w.x, w.y)
            );
        }
        t.push('\n');
        prev = (w.x, w.y);
    }
    if let Some(hint) = &route.arrival_hint {
        let _ = writeln!(t, "On arrival: {hint}");
    }
    t.push_str(
        "Use act with follow_route=true to walk it; it stops when something needs your attention.",
    );
    t
}

fn follow_route(session: &mut Session, p: &ActParams) -> anyhow::Result<CallToolResult> {
    if let Some(comment) = p
        .comment
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        viewer::log(LogKind::Comment, comment);
    }
    viewer::log(
        LogKind::Action,
        &format!(
            "follow route → {} · up to {} tics",
            session.goal.describe(),
            p.tics
        ),
    );
    let before = session.state();
    let mut events = Vec::new();
    let end = session.follow(p.tics, p.run, &mut events)?;
    observe::diff_events(&before, &session.state(), &mut events);
    events.push(match end {
        FollowEnd::Arrived(msg) => msg.trim().to_string(),
        FollowEnd::Spotted(msg) => {
            format!("STOPPED: {msg}. Deal with it, then continue following the route.")
        }
        FollowEnd::Damaged => {
            "STOPPED: you are taking damage. Find the attacker (see things and the attacker bearing).".into()
        }
        FollowEnd::Blocked => "STOPPED: BLOCKED, not making progress. Something is in the way (a \
            monster, a closed door or bars, or an obstacle the planner missed). Check the \
            screenshot and get_map."
            .into(),
        FollowEnd::TimeUp => "Still on the way; follow the route again to continue.".into(),
        FollowEnd::NoRoute(e) => format!("No route: {e}"),
        FollowEnd::Other(msg) => msg,
    });
    Ok(observation_result(
        session,
        events,
        p.screenshot.then_some(p.screenshot_size),
        true,
    ))
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

impl DoomServer {
    /// Run `f` on the engine thread. Engine failures (e.g. the game was quit
    /// from the menu) become tool errors so the model gets to read them.
    async fn run<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(&mut Session) -> anyhow::Result<CallToolResult> + Send + 'static,
    {
        let mut result = match self.doom.with(f).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) | Err(e) => tool_error(e.to_string()),
        };
        attach_spectator_messages(&mut result);
        Ok(result)
    }

    /// Note for the model about the spectator view; opens it on first use.
    fn viewer_note(&self) -> Option<String> {
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
}

#[tool_router]
impl DoomServer {
    pub fn new(doom: Doom, viewer: Option<ViewerConfig>) -> Self {
        Self {
            doom,
            viewer,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Start a new game, skipping the menus. Returns the first observation.",
        annotations(title = "New game", destructive_hint = true, open_world_hint = false)
    )]
    async fn new_game(
        &self,
        Parameters(p): Parameters<NewGameParams>,
    ) -> Result<CallToolResult, ErrorData> {
        viewer::wait_for_playback().await;
        let mut result = self
            .run(move |engine| {
                let commercial = engine.state().gamemode == 2;
                let shareware = engine.state().gamemode == 0;
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
                engine.new_game(p.skill - 1, episode, p.map)?;
                Ok(observation_result(
                    engine,
                    Vec::new(),
                    Some(ScreenshotSize::Medium),
                    true,
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

    #[tool(
        description = "Perform an action: hold the chosen movement/turn/fire/use controls for \
            `tics` game tics (35 per second), then return the resulting screenshot, player status, \
            events (damage, pickups, kills), and nearby things with distance and bearing. With no \
            controls set it just waits.",
        annotations(title = "Act", destructive_hint = false, open_world_hint = false)
    )]
    async fn act(&self, Parameters(p): Parameters<ActParams>) -> Result<CallToolResult, ErrorData> {
        let max_tics = if p.follow_route { 350 } else { 140 };
        if !(1..=max_tics).contains(&p.tics) {
            return Ok(tool_error(format!("tics must be between 1 and {max_tics}")));
        }
        if p.weapon.is_some_and(|w| !(1..=7).contains(&w)) {
            return Ok(tool_error("weapon must be a slot from 1 to 7"));
        }
        viewer::wait_for_playback().await;
        self.run(move |engine| {
            if p.follow_route {
                return follow_route(engine, &p);
            }
            let mut turn = p.turn.unwrap_or(0.0);
            // aim_at: the engine pointer and type of the target, to track it every tic.
            let mut aim_target = None;
            if let Some(id) = p.aim_at {
                let target = engine
                    .things(8192.0)
                    .into_iter()
                    .find(|t| engine.thing_id(t.id) == id);
                match target {
                    Some(t) => {
                        turn = t.bearing;
                        aim_target = Some((t.id, t.type_));
                    }
                    None => {
                        return Ok(tool_error(format!(
                            "no thing with id {id} (it may be dead, picked up, or the level changed)"
                        )));
                    }
                }
            }
            let input = Input {
                movement: match p.movement {
                    Some(Move::Forward) => Dir::Pos,
                    Some(Move::Backward) => Dir::Neg,
                    None => Dir::None,
                },
                strafe: match p.strafe {
                    Some(Strafe::Right) => Dir::Pos,
                    Some(Strafe::Left) => Dir::Neg,
                    None => Dir::None,
                },
                turn_degrees: turn,
                fire: p.fire,
                use_: p.use_,
                run: p.run,
                weapon: p.weapon,
                tics: p.tics,
            };

            if let Some(comment) = p.comment.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
                viewer::log(LogKind::Comment, comment);
            }
            viewer::log(LogKind::Action, &describe_action(&p, turn));

            let before = engine.state();
            match aim_target {
                // Keep the crosshair on the target for the whole action; stop
                // turning once it's gone (dead monsters stay put anyway).
                Some((ptr, kind)) => engine.step_aimed(&Input { turn_degrees: 0.0, ..input.clone() }, |e| {
                    e.things(8192.0)
                        .into_iter()
                        .find(|t| t.id == ptr && t.type_ == kind && t.health > 0)
                        .map(|t| t.bearing)
                })?,
                None => engine.step(&input)?,
            }
            let after = engine.state();
            let mut events = Vec::new();
            observe::diff_events(&before, &after, &mut events);
            if (input.movement != Dir::None || input.strafe != Dir::None)
                && after.in_level != 0
                && (before.episode, before.map) == (after.episode, after.map)
            {
                let moved = (after.x - before.x).hypot(after.y - before.y);
                // Walking from a standstill covers ~4 units per tic.
                if moved < 2.0 * input.tics as f64 {
                    events.push(format!(
                        "BLOCKED: you only moved {moved:.0} units. Something is in the way; \
                         check get_map, then turn or strafe around it"
                    ));
                } else {
                    events.push(format!("Moved {moved:.0} units"));
                }
            }
            Ok(observation_result(
                engine,
                events,
                p.screenshot.then_some(p.screenshot_size),
                true,
            ))
        })
        .await
    }

    #[tool(
        description = "Look at the current situation without letting any game time pass.",
        annotations(title = "Observe", read_only_hint = true, open_world_hint = false)
    )]
    async fn observe(
        &self,
        Parameters(p): Parameters<ObserveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |engine| {
            Ok(observation_result(
                engine,
                Vec::new(),
                p.screenshot.then_some(p.screenshot_size),
                false,
            ))
        })
        .await
    }

    #[tool(
        description = "Draw an ASCII top-down map centred on you: walls, doors, locked doors, \
            lifts, switches, the level EXIT, monsters, pickups and obstacles. Rotated so the \
            direction you face is up, or north-up with `north_up`. No game time passes.",
        annotations(title = "Map", read_only_hint = true, open_world_hint = false)
    )]
    async fn get_map(
        &self,
        Parameters(p): Parameters<MapParams>,
    ) -> Result<CallToolResult, ErrorData> {
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

    #[tool(
        description = "Choose where to go and get the full route there. Plans a path over the \
            level's real geometry (steps, ledges, doors, keys, lifts, damaging floors) and sets it \
            as your navigation goal: every observation then shows the next waypoint, the \
            screenshot marks it with a yellow diamond, and act with follow_route=true walks it. \
            No game time passes.",
        annotations(title = "Route", read_only_hint = false, open_world_hint = false)
    )]
    async fn route(
        &self,
        Parameters(p): Parameters<RouteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let goal = match p.goal {
            RouteGoal::Exit => Goal::Exit,
            RouteGoal::SecretExit => Goal::SecretExit,
            RouteGoal::BlueKey => Goal::Key(KeyColor::Blue),
            RouteGoal::YellowKey => Goal::Key(KeyColor::Yellow),
            RouteGoal::RedKey => Goal::Key(KeyColor::Red),
            RouteGoal::Switch => Goal::Switch,
            RouteGoal::Explore => Goal::Explore,
            RouteGoal::Thing => match p.thing_id {
                Some(id) => Goal::Thing(id),
                None => return Ok(tool_error("goal \"thing\" needs thing_id")),
            },
            RouteGoal::Point => match (p.x, p.y) {
                (Some(x), Some(y)) => Goal::Point(x, y),
                _ => return Ok(tool_error("goal \"point\" needs x and y")),
            },
        };
        self.run(move |session| {
            session.check_alive()?;
            session.goal = goal;
            viewer::log(
                LogKind::Action,
                &format!("route → {}", session.goal.describe()),
            );
            let text = describe_route(session);
            let summary = session.nav_summary();
            viewer::publish_status(session, summary.as_ref());
            Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
        })
        .await
    }

    #[tool(
        description = "Write or replace your plan for the current level (a short checklist works \
            well: goals, what you've learned, what's next). It is shown back to you in every \
            observation, and to the human watching. Update it as things change. No game time passes.",
        annotations(title = "Set plan", read_only_hint = false, open_world_hint = false)
    )]
    async fn set_plan(
        &self,
        Parameters(p): Parameters<PlanParams>,
    ) -> Result<CallToolResult, ErrorData> {
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

    #[tool(
        description = "Tap raw keyboard keys in order: for menus, the automap (tab), cheat codes, \
            or anything `act` can't do. Returns an observation afterwards.",
        annotations(title = "Press keys", destructive_hint = true, open_world_hint = false)
    )]
    async fn press_keys(
        &self,
        Parameters(p): Parameters<PressKeysParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if p.keys.is_empty() || p.keys.len() > 64 {
            return Ok(tool_error("keys must contain 1-64 entries"));
        }
        let gap = p.gap_tics.clamp(1, TICRATE * 2);
        viewer::wait_for_playback().await;
        self.run(move |engine| {
            let mut codes = Vec::new();
            for name in &p.keys {
                match key_code(name, engine) {
                    Some(c) => codes.push(c),
                    None => return Ok(tool_error(format!("unknown key {name:?}"))),
                }
            }
            viewer::log(LogKind::Action, &format!("keys: {}", p.keys.join(" ")));
            engine.press_keys(&codes, gap)?;
            Ok(observation_result(
                engine,
                Vec::new(),
                p.screenshot.then_some(ScreenshotSize::Medium),
                true,
            ))
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DoomServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("doom-mcp", env!("CARGO_PKG_VERSION"))
                    .with_title("DOOM")
                    .with_description("Play the original DOOM (1993) through MCP tools"),
            )
            .with_instructions(INSTRUCTIONS)
    }
}
