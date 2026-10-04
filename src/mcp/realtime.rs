//! Real-time front-end: the game runs continuously and the pilot plays; the
//! model commands (`command`) and reacts to what happens (`wait_for_events`).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

use super::common::{
    Core, MapParams, ModeText, NewGameParams, ObserveParams, PlanParams, PressKeysParams,
    RouteGoal, goal_from, observation_result, tool_error, weapon_ammo,
};
use crate::game::session::Session;
use crate::pilot::Stance;
use crate::viewer::{self, LogKind};
use crate::world::nav::Goal;
use crate::world::observe::{self, ScreenshotSize};

const INSTRUCTIONS: &str = "\
You are commanding a DOOM (1993) marine in REAL TIME. The game runs continuously at 35 tics per \
second. A fast built-in pilot does the moment-to-moment playing: it walks the route to your \
navigation goal (opening doors, riding lifts, pressing the switches it's sent to, going for the \
keys and switches that open the way first) and fights whatever your stance allows (choosing \
weapons, aiming, dodging). You are the commander: decide where to go and how to fight, and \
react to what happens. A human is usually watching live.

LOOP
1. new_game starts the game. Its result includes a LEVEL BRIEFING (route to the exit, keys and \
switches needed, monsters).
2. Write a short plan with set_plan; keep it current.
3. Call wait_for_events (timeout 10-20 s). It returns as soon as something important happens \
(level start or completion, low health, death, getting stuck, a big monster, a message from the \
human) or when the timeout ends, with everything since your last call and the current status.
4. React with command: a new navigation goal (a key, a switch, explore, a health pickup by thing \
id, the exit), a stance (aggressive / balanced / cautious / hold_fire), a focus target, holding \
position, a preferred weapon, or pressing use. Orders last until you change them. Then call \
wait_for_events again.
Keep calling tools: after 60 s without any call the game pauses until the next one.

Intervene for strategy: grab health when low, avoid or seek fights, hunt secrets, and follow \
the human's requests. observe gives the full picture with a screenshot; get_map draws an ASCII \
map. press_keys is for menus only.

MESSAGES FROM THE HUMAN
The person watching can type instructions on the spectator page. They arrive at the top of \
your next tool result (and wake wait_for_events), marked 📣 MESSAGE FROM THE HUMAN WATCHING. \
They come from your user, so they take priority over your default goal: carry them out with \
command, and if one is impossible or unclear, do the closest sensible thing and say why. \
Acknowledge each in your next command `comment`, and update your plan with set_plan when it \
changes what you're doing.";

pub const TEXT: ModeText = ModeText {
    briefing_next: "The pilot is heading for the exit. Adjust with command (goal, stance, focus) and keep \
                    your plan current with set_plan.",
    acknowledge_in: "command `comment`",
};

fn default_timeout() -> f64 {
    10.0
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StanceParam {
    /// Engage anything in sight and close in on distant targets.
    Aggressive,
    /// Engage monsters that are attacking or close by (the default).
    Balanced,
    /// Only fight back against monsters attacking you; keep your distance.
    Cautious,
    /// Never shoot (sneak past, or let the human enjoy the scenery).
    HoldFire,
}

impl From<StanceParam> for Stance {
    fn from(s: StanceParam) -> Self {
        match s {
            StanceParam::Aggressive => Stance::Aggressive,
            StanceParam::Balanced => Stance::Balanced,
            StanceParam::Cautious => Stance::Cautious,
            StanceParam::HoldFire => Stance::HoldFire,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommandParams {
    /// New navigation goal. The route goes to prerequisites (keys, switches) first.
    #[serde(default)]
    pub goal: Option<RouteGoal>,
    /// For goal "thing": the thing's id (e.g. a medikit from the things list).
    #[serde(default)]
    pub thing_id: Option<u32>,
    /// For goal "point": map coordinates.
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    /// How readily to fight.
    #[serde(default)]
    pub stance: Option<StanceParam>,
    /// Engage this thing (by id) first whenever it's in sight; 0 clears the focus.
    #[serde(default)]
    pub focus: Option<u32>,
    /// true: move toward the goal; false: hold position (still fights).
    #[serde(default)]
    pub travel: Option<bool>,
    /// Preferred weapon slot (1 fist/chainsaw, 2 pistol, 3 shotgun, 4 chaingun,
    /// 5 rocket launcher, 6 plasma, 7 BFG); 0 = choose automatically (default).
    #[serde(default)]
    pub weapon: Option<u8>,
    /// Press use once now (a switch or door you're facing).
    #[serde(default, rename = "use")]
    pub use_: bool,
    /// Optional one-line note on your intent, shown to the human watching.
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitParams {
    /// Longest wait in seconds (0.5-30, default 10). Returns early on anything
    /// important.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: f64,
    /// Include a screenshot (default false; use observe for the full picture).
    #[serde(default)]
    pub screenshot: bool,
}

#[derive(Clone)]
pub struct RealtimeServer {
    core: Core,
    /// Newest event the model has been given.
    cursor: Arc<AtomicU64>,
    tool_router: ToolRouter<Self>,
}

/// Most events listed in one `wait_for_events` result.
const MAX_EVENTS_SHOWN: usize = 40;

/// The current situation in a few lines: player, what the pilot is doing,
/// orders, and visible threats.
fn status_text(session: &mut Session) -> String {
    let s = session.state();
    let (status, hint) = observe::status(&s);
    let mut t = String::new();
    if status != "playing" {
        let _ = writeln!(
            t,
            "Status: {status}{}",
            hint.map(|h| format!(" ({h})")).unwrap_or_default()
        );
        return t;
    }
    let obs = observe::observe(session, Vec::new());
    if let Some(pl) = &obs.player {
        let _ = writeln!(
            t,
            "{} {:.1}s · Health {} · Armor {} · {}{} · Kills {} · Keys: {}",
            obs.level.as_deref().unwrap_or(""),
            obs.level_time_seconds,
            pl.health,
            pl.armor,
            pl.weapon,
            weapon_ammo(pl),
            pl.kills,
            if pl.keys.is_empty() {
                "none".to_string()
            } else {
                pl.keys.join(", ")
            }
        );
    }
    let o = &session.pilot.orders;
    let _ = writeln!(
        t,
        "Pilot: {} · stance {}{}{}",
        session.pilot.activity(),
        o.stance.name(),
        if o.travel { "" } else { " · holding position" },
        o.focus
            .map(|f| format!(" · focus [{f}]"))
            .unwrap_or_default()
    );
    // From the pilot's current plan: cheaper than planning afresh, which would
    // hold up the game for a moment.
    let _ = write!(t, "Goal: {}", session.goal.describe());
    if let Some(route) = session.pilot.route() {
        if route.goal != session.goal.describe() {
            let _ = write!(t, " (heading to {} first)", route.goal);
        }
        if let Some(pre) = &route.prerequisite {
            let _ = write!(t, ". {pre}");
        }
    }
    t.push('\n');
    let threats: Vec<String> = obs
        .things
        .iter()
        .filter(|th| th.category == "monster" && th.line_of_sight)
        .map(|th| {
            format!(
                "{} [{}] {}u {:+.0}°{}",
                th.name,
                th.id,
                th.distance,
                th.bearing,
                if th.targeting_you {
                    " (targeting you)"
                } else {
                    ""
                }
            )
        })
        .collect();
    if !threats.is_empty() {
        let _ = writeln!(t, "In sight: {}", threats.join(", "));
    }
    if let Some(plan) = session.plan_text(&s) {
        let _ = writeln!(t, "Your plan:\n{}", plan.trim_end());
    } else {
        t.push_str("Your plan: (none yet; write one with set_plan)\n");
    }
    t
}

fn describe_orders(p: &CommandParams, goal: Option<&Goal>) -> String {
    let mut parts = Vec::new();
    if let Some(g) = goal {
        parts.push(format!("goal → {}", g.describe()));
    }
    if let Some(s) = p.stance {
        parts.push(format!("stance {}", Stance::from(s).name()));
    }
    match p.focus {
        Some(0) => parts.push("clear focus".into()),
        Some(id) => parts.push(format!("focus [{id}]")),
        None => {}
    }
    match p.travel {
        Some(true) => parts.push("move".into()),
        Some(false) => parts.push("hold position".into()),
        None => {}
    }
    match p.weapon {
        Some(0) => parts.push("weapon auto".into()),
        Some(w) => parts.push(format!("weapon {w}")),
        None => {}
    }
    if p.use_ {
        parts.push("USE".into());
    }
    if parts.is_empty() {
        "no change".into()
    } else {
        parts.join(", ")
    }
}

#[tool_router]
impl RealtimeServer {
    pub fn new(core: Core) -> Self {
        Self {
            core,
            cursor: Arc::new(AtomicU64::new(0)),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Start a new game, skipping the menus. The game then runs in real time with \
            the pilot heading for the exit; the result includes the level briefing.",
        annotations(title = "New game", destructive_hint = true, open_world_hint = false)
    )]
    async fn new_game(
        &self,
        Parameters(p): Parameters<NewGameParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = self
            .core
            .new_game(p, |session| {
                session.reset_realtime();
                session.goal = Goal::Exit;
            })
            .await;
        self.cursor.store(0, Ordering::SeqCst);
        result
    }

    #[tool(
        description = "Give the pilot standing orders: navigation goal, fighting stance, focus \
            target, hold position or move, preferred weapon, or press use. Only the fields you set \
            change; orders last until changed. Takes effect on the next tic.",
        annotations(title = "Command", destructive_hint = false, open_world_hint = false)
    )]
    async fn command(
        &self,
        Parameters(p): Parameters<CommandParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if p.weapon.is_some_and(|w| w > 7) {
            return Ok(tool_error(
                "weapon must be 0 (automatic) or a slot from 1 to 7",
            ));
        }
        let goal = match p.goal {
            Some(g) => match goal_from(g, p.thing_id, p.x, p.y) {
                Ok(goal) => Some(goal),
                Err(e) => return Ok(tool_error(e)),
            },
            None => None,
        };
        self.core
            .run(move |session| {
                session.check_alive()?;
                if let Some(comment) = p
                    .comment
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                {
                    viewer::log(LogKind::Comment, comment);
                }
                let summary = describe_orders(&p, goal.as_ref());
                viewer::log(LogKind::Action, &format!("command: {summary}"));
                if let Some(goal) = goal {
                    session.goal = goal;
                    session.pilot.reset_travel();
                }
                let orders = &mut session.pilot.orders;
                if let Some(s) = p.stance {
                    orders.stance = s.into();
                }
                match p.focus {
                    Some(0) => orders.focus = None,
                    Some(id) => orders.focus = Some(id),
                    None => {}
                }
                if let Some(travel) = p.travel {
                    orders.travel = travel;
                }
                match p.weapon {
                    Some(0) => orders.weapon = None,
                    Some(w) => orders.weapon = Some(w),
                    None => {}
                }
                if p.use_ {
                    orders.use_now = true;
                }
                let text = format!("Orders: {summary}.\n{}", status_text(session));
                Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
            })
            .await
    }

    #[tool(
        description = "Wait until something important happens (or the timeout), then get every \
            event since your last call plus the current status. Your main way to follow the game.",
        annotations(
            title = "Wait for events",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn wait_for_events(
        &self,
        Parameters(p): Parameters<WaitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let timeout = Duration::from_secs_f64(p.timeout_seconds.clamp(0.5, 30.0));
        let deadline = Instant::now() + timeout;
        let cursor = self.cursor.load(Ordering::SeqCst);
        loop {
            let important = self
                .core
                .game
                .with(move |session| {
                    session.touch();
                    session
                        .realtime
                        .as_ref()
                        .is_some_and(|rt| rt.events.has_important_since(cursor))
                })
                .await
                .unwrap_or(true);
            if important || viewer::has_messages() || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let shared_cursor = self.cursor.clone();
        let screenshot = p.screenshot;
        self.core
            .run(move |session| {
                session.check_alive()?;
                let (events, latest) = match &session.realtime {
                    Some(rt) => (rt.events.since(cursor), rt.events.latest()),
                    None => (Vec::new(), cursor),
                };
                shared_cursor.store(latest, Ordering::SeqCst);
                let mut t = String::new();
                if events.is_empty() {
                    t.push_str("No new events.\n");
                } else {
                    let skipped = events.len().saturating_sub(MAX_EVENTS_SHOWN);
                    if skipped > 0 {
                        let _ = writeln!(t, "({skipped} earlier events not shown)");
                    }
                    t.push_str("Events (oldest first):\n");
                    for e in events.iter().skip(skipped) {
                        let _ = writeln!(
                            t,
                            "[{:.1}s]{} {}",
                            e.level_time,
                            if e.important { " ⚠" } else { "" },
                            e.text
                        );
                    }
                }
                t.push_str(&status_text(session));
                let mut content = vec![ContentBlock::text(t)];
                if screenshot {
                    let s = session.state();
                    let playing = observe::status(&s).0 == "playing";
                    let png =
                        observe::screenshot_png(session, ScreenshotSize::Medium, playing, None);
                    content.push(ContentBlock::image(
                        base64::engine::general_purpose::STANDARD.encode(png),
                        "image/png",
                    ));
                }
                Ok(CallToolResult::success(content))
            })
            .await
    }

    #[tool(
        description = "The full picture right now: status, nearby things with ids, navigation, the \
            pilot's activity, and a screenshot. Doesn't consume events.",
        annotations(title = "Observe", read_only_hint = true, open_world_hint = false)
    )]
    async fn observe(
        &self,
        Parameters(p): Parameters<ObserveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let cursor = self.cursor.load(Ordering::SeqCst);
        self.core
            .run(move |session| {
                let mut result = observation_result(
                    session,
                    Vec::new(),
                    p.screenshot.then_some(p.screenshot_size),
                    false,
                    TEXT.briefing_next,
                );
                let unread = session
                    .realtime
                    .as_ref()
                    .map(|rt| rt.events.since(cursor).len())
                    .unwrap_or(0);
                let o = &session.pilot.orders;
                let extra = format!(
                    "Pilot: {} · stance {}{}{} · {unread} unread event(s) (wait_for_events)",
                    session.pilot.activity(),
                    o.stance.name(),
                    if o.travel { "" } else { " · holding position" },
                    o.focus
                        .map(|f| format!(" · focus [{f}]"))
                        .unwrap_or_default(),
                );
                result.content.insert(1, ContentBlock::text(extra));
                Ok(result)
            })
            .await
    }

    #[tool(
        description = "Write or replace your plan for the current level (a short checklist works \
            well). It is shown back to you in every status, and to the human watching.",
        annotations(title = "Set plan", read_only_hint = false, open_world_hint = false)
    )]
    async fn set_plan(
        &self,
        Parameters(p): Parameters<PlanParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.core.set_plan(p).await
    }

    #[tool(
        description = "Draw an ASCII top-down map centred on you: walls, doors, locked doors, \
            lifts, switches, the level EXIT, monsters, pickups, obstacles and your route.",
        annotations(title = "Map", read_only_hint = true, open_world_hint = false)
    )]
    async fn get_map(
        &self,
        Parameters(p): Parameters<MapParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.core.get_map(p).await
    }

    #[tool(
        description = "Tap raw keyboard keys in order: for menus or cheat codes. Returns an \
            observation afterwards.",
        annotations(title = "Press keys", destructive_hint = true, open_world_hint = false)
    )]
    async fn press_keys(
        &self,
        Parameters(p): Parameters<PressKeysParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.core.press_keys(p).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RealtimeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("doom-mcp", env!("CARGO_PKG_VERSION"))
                    .with_title("DOOM (real-time)")
                    .with_description(
                        "Command a DOOM (1993) marine in real time through MCP tools",
                    ),
            )
            .with_instructions(INSTRUCTIONS)
    }
}
