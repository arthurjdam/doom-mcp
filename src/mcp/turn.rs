//! Turn-based front-end: the game is frozen between tool calls and the model
//! controls every action (`act`), with route following and aiming helpers.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

use super::common::{
    Core, MapParams, ModeText, NewGameParams, ObserveParams, PlanParams, PressKeysParams,
    RouteGoal, default_true, describe_route, goal_from, observation_result, tool_error,
};
use crate::engine::{Dir, Input};
use crate::game::session::{FollowEnd, Session};
use crate::viewer::{self, LogKind};
use crate::world::observe::{self, ScreenshotSize};

/// Briefing closer and acknowledgement wording for this mode.
pub const TEXT: ModeText = ModeText {
    briefing_next: "Next: write a short plan with set_plan, then make progress with act follow_route=true, \
                    fighting whatever shows up.",
};

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
The spectator page is read-only: the person watching talks to you in this conversation. A \
message from them is an instruction from your user and takes priority over your default goal. \
Follow it, or if it's impossible or unclear do the closest sensible thing and say why. \
Acknowledge it in your next act `comment`, and update your plan with set_plan when it changes \
what you're doing.

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
pub struct RouteParams {
    pub goal: RouteGoal,
    #[serde(default)]
    pub thing_id: Option<u32>,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
}

#[derive(Clone)]
pub struct TurnServer {
    core: Core,
    tool_router: ToolRouter<Self>,
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
        TEXT.briefing_next,
    ))
}

#[tool_router]
impl TurnServer {
    pub fn new(core: Core) -> Self {
        Self {
            core,
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
        self.core.new_game(p, |_| {}).await
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
        self.core.run(move |engine| {
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
                TEXT.briefing_next,
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
        self.core
            .run(move |engine| {
                Ok(observation_result(
                    engine,
                    Vec::new(),
                    p.screenshot.then_some(p.screenshot_size),
                    false,
                    TEXT.briefing_next,
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
        self.core.get_map(p).await
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
        let goal = match goal_from(p.goal, p.thing_id, p.x, p.y) {
            Ok(g) => g,
            Err(e) => return Ok(tool_error(e)),
        };
        self.core.run(move |session| {
            session.check_alive()?;
            session.goal = goal;
            viewer::log(
                LogKind::Action,
                &format!("route → {}", session.goal.describe()),
            );
            let text = describe_route(
                session,
                "Use act with follow_route=true to walk it; it stops when something needs your attention.",
            );
            let summary = session.nav_summary();
            session.publish_spectator(summary.as_ref());
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
        self.core.set_plan(p).await
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
        viewer::wait_for_playback().await;
        self.core.press_keys(p).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TurnServer {
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
