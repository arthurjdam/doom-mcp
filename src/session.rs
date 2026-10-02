//! Everything that lives on the engine thread besides the engine itself:
//! navigation, the model's plan, and per-level bookkeeping.

use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

use anyhow::Result;
use serde::Serialize;

use crate::doom::Engine;
use crate::doom::engine::{Dir, Input};
use crate::doom::ffi::State;
use crate::nav::{self, Goal, Navigator, Route, WaypointKind};
use crate::observe::thing_type;

const PST_DEAD: i32 = 1;
/// Distance at which a plain waypoint counts as reached.
const ARRIVE: f64 = 24.0;
const ARRIVE_STOP: f64 = 20.0;
/// Most tics to wait for a door to open.
const DOOR_WAIT: u32 = 70;

pub struct Session {
    pub engine: Engine,
    pub nav: Navigator,
    pub goal: Goal,
    /// The model's plan and the level it was written for.
    pub plan: Option<(String, (i32, i32))>,
    /// Level the briefing was last given for.
    briefed: Option<(i32, i32, i32)>,
}

impl Deref for Session {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.engine
    }
}

impl DerefMut for Session {
    fn deref_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NextWaypoint {
    pub distance: i32,
    /// Degrees from the crosshair: positive = right.
    pub bearing: f64,
    pub what: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavSummary {
    pub goal: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_length: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<NextWaypoint>,
    pub waypoints_left: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Why `follow` stopped.
pub enum FollowEnd {
    Arrived(String),
    Spotted(String),
    Damaged,
    Blocked,
    TimeUp,
    NoRoute(String),
    Other(String),
}

impl Session {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            nav: Navigator::default(),
            goal: Goal::Exit,
            plan: None,
            briefed: None,
        }
    }

    pub fn route(&mut self) -> Result<Route, String> {
        self.nav.route(&mut self.engine, &self.goal)
    }

    pub fn nav_summary(&mut self) -> Option<NavSummary> {
        let s = self.engine.state();
        if s.in_level == 0 || s.playerstate == PST_DEAD || s.demoplayback != 0 {
            return None;
        }
        let goal = self.goal.describe();
        Some(match self.route() {
            Ok(route) => {
                let next = route.waypoints.first().map(|w| NextWaypoint {
                    distance: (w.x - s.x).hypot(w.y - s.y).round() as i32,
                    bearing: (nav::bearing_to(&s, w.x, w.y) * 10.0).round() / 10.0,
                    what: w.kind.describe(),
                });
                // The arrival hint only matters once the goal is the next stop.
                let mut note = route
                    .arrival_hint
                    .clone()
                    .filter(|_| route.waypoints.len() == 1);
                let mut goal = goal;
                if let Some(pre) = &route.prerequisite {
                    goal = format!("{} (on the way to {goal})", route.goal);
                    note = Some(format!(
                        "{pre}{}",
                        note.map(|n| format!(" {n}")).unwrap_or_default()
                    ));
                }
                if route.crosses_hazard {
                    note = Some(format!(
                        "Route crosses a damaging floor; move quickly.{}",
                        note.map(|n| format!(" {n}")).unwrap_or_default()
                    ));
                }
                NavSummary {
                    goal,
                    route_length: Some(route.length.round() as i32),
                    next,
                    waypoints_left: route.waypoints.len(),
                    note,
                }
            }
            Err(e) => NavSummary {
                goal,
                route_length: None,
                next: None,
                waypoints_left: 0,
                note: Some(e),
            },
        })
    }

    /// A one-time briefing when a level starts (or restarts).
    pub fn take_briefing(&mut self) -> Option<String> {
        let s = self.engine.state();
        if s.in_level == 0 || s.demoplayback != 0 || s.gamestate != 0 {
            return None;
        }
        let id = (s.episode, s.map, s.leveltime);
        if let Some((e, m, t)) = self.briefed
            && (e, m) == (s.episode, s.map)
            && s.leveltime >= t
        {
            self.briefed = Some((e, m, s.leveltime));
            return None;
        }
        let new_level = self
            .briefed
            .is_none_or(|(e, m, _)| (e, m) != (s.episode, s.map));
        self.briefed = Some(id);
        if new_level {
            self.goal = Goal::Exit;
        }

        let level_name = crate::observe::level_name(&s);
        let mut b = format!("LEVEL BRIEFING — {level_name}");
        if !new_level {
            b.push_str(" (restarted)");
        }
        b.push('\n');
        match self.nav.route(&mut self.engine, &Goal::Exit) {
            Ok(r) => match &r.prerequisite {
                None => b.push_str(&format!(
                    "- Exit: route of {:.0} units with {} waypoints from here.\n",
                    r.length,
                    r.waypoints.len()
                )),
                Some(pre) => b.push_str(&format!(
                    "- Exit: not directly reachable. {pre} First stop: {} ({:.0} units).\n",
                    r.goal, r.length
                )),
            },
            Err(e) => b.push_str(&format!("- Exit: {e}\n")),
        }
        let locked = self
            .nav
            .level(&self.engine)
            .map(|l| l.locked_doors())
            .unwrap_or_default();
        let mut keys: Vec<String> = self
            .engine
            .things(1e9)
            .into_iter()
            .filter_map(|t| {
                let (name, category) = thing_type(t.type_);
                (category == "key").then(|| name.to_string())
            })
            .collect();
        keys.dedup();
        if locked.is_empty() {
            b.push_str("- No locked doors.\n");
        } else {
            let colors: Vec<&str> = locked.iter().map(|k| k.name()).collect();
            b.push_str(&format!(
                "- Locked doors: {}. Keys on this level: {}.\n",
                colors.join(", "),
                if keys.is_empty() {
                    "none found".to_string()
                } else {
                    keys.join(", ")
                }
            ));
        }
        b.push_str(&format!(
            "- Monsters: {} · Items: {} · Secrets: {}\n",
            s.totalkills, s.totalitems, s.totalsecrets
        ));
        b.push_str(
            "Next: write a short plan with set_plan, then make progress with act follow_route=true, \
             fighting whatever shows up.",
        );
        Some(b)
    }

    /// Visible, living monsters (ids).
    fn visible_monsters(&mut self) -> Vec<(u32, &'static str, f64)> {
        let things = self.engine.things(2048.0);
        things
            .into_iter()
            .filter(|t| {
                thing_type(t.type_).1 == "monster"
                    && t.health > 0
                    && t.line_of_sight != 0
                    && t.bearing.abs() <= 45.0
            })
            .map(|t| (self.engine.thing_id(t.id), thing_type(t.type_).0, t.bearing))
            .collect()
    }

    /// Turn in place to face (x, y), spending up to two tics.
    fn face(&mut self, x: f64, y: f64) -> Result<()> {
        for _ in 0..2 {
            let s = self.engine.state();
            let b = nav::bearing_to(&s, x, y);
            if b.abs() < 1.0 {
                break;
            }
            self.engine.turn_next_tic(b);
            self.engine.run_tic()?;
        }
        Ok(())
    }

    /// Is a sector between the player and waypoint `wp` currently moving?
    fn mover_nearby(&self, s: &State, wp: &nav::Waypoint) -> bool {
        let sectors = self.engine.sectors();
        (0..=4).any(|i| {
            let t = i as f64 / 4.0;
            let (x, y) = (s.x + (wp.x - s.x) * t, s.y + (wp.y - s.y) * t);
            self.engine
                .point_sector(x, y)
                .and_then(|sec| sectors.get(sec))
                .is_some_and(|sec| sec.moving != 0)
        })
    }

    /// Walk the route to the current goal for up to `tics` tics, steering
    /// every tic and opening ordinary doors on the way. Stops early when
    /// something needs the model's attention.
    pub fn follow(&mut self, tics: u32, run: bool, events: &mut Vec<String>) -> Result<FollowEnd> {
        let route = match self.route() {
            Ok(r) => r,
            Err(e) => return Ok(FollowEnd::NoRoute(e)),
        };
        let start = self.engine.state();
        let level = (start.episode, start.map);
        let mut seen: HashSet<u32> = self.visible_monsters().into_iter().map(|m| m.0).collect();
        let walk = Input {
            movement: Dir::Pos,
            run,
            ..Default::default()
        };
        let mut held = self.engine.press(&walk)?;

        let mut idx = 0;
        let mut used = 0;
        let mut waited_for_mover = 0;
        let mut tried_unstick = false;
        let mut checkpoint = (start.x, start.y, 0u32);
        let end = loop {
            if used >= tics {
                break FollowEnd::TimeUp;
            }
            let s = self.engine.state();
            if s.in_level == 0 || (s.episode, s.map) != level {
                break FollowEnd::Other("The level changed.".into());
            }
            if s.playerstate == PST_DEAD {
                break FollowEnd::Other("You died.".into());
            }
            let Some(wp) = route.waypoints.get(idx).copied() else {
                break FollowEnd::Arrived(format!(
                    "Arrived at {}. {}",
                    route.goal,
                    route.arrival_hint.clone().unwrap_or_default()
                ));
            };
            let dist = (wp.x - s.x).hypot(wp.y - s.y);

            match wp.kind {
                WaypointKind::Walk if dist < ARRIVE => {
                    idx += 1;
                    continue;
                }
                WaypointKind::Door { sector, fx, fy } if dist < ARRIVE_STOP => {
                    self.engine.release(std::mem::take(&mut held));
                    self.face(fx, fy)?;
                    self.engine.tap_use()?;
                    used += 3;
                    let mut waited = 0;
                    let health = self.engine.state().health;
                    while waited < DOOR_WAIT
                        && !self
                            .nav
                            .level(&self.engine)
                            .is_some_and(|l| l.door_open(&self.engine, sector))
                    {
                        self.engine.run_tic()?;
                        waited += 1;
                    }
                    used += waited;
                    if self.engine.state().health < health {
                        break FollowEnd::Damaged;
                    }
                    if !self
                        .nav
                        .level(&self.engine)
                        .is_some_and(|l| l.door_open(&self.engine, sector))
                    {
                        break FollowEnd::Arrived(
                            "Reached a door but it didn't open. It may need a key, a switch, or to be \
                             used from the other side."
                                .into(),
                        );
                    }
                    events.push("Opened a door".into());
                    held = self.engine.press(&walk)?;
                    checkpoint = (s.x, s.y, used);
                    idx += 1;
                    continue;
                }
                WaypointKind::Lift { fx, fy, .. } if dist < ARRIVE_STOP => {
                    self.engine.release(std::mem::take(&mut held));
                    self.face(fx, fy)?;
                    break FollowEnd::Arrived(
                        "Reached a lift and facing it. Press use (act use=true), wait ~1s for it to \
                         lower, walk onto it, wait for it to rise, then continue following the route."
                            .into(),
                    );
                }
                WaypointKind::Goal { fx, fy, face } if dist < ARRIVE_STOP => {
                    self.engine.release(std::mem::take(&mut held));
                    if face {
                        self.face(fx, fy)?;
                    }
                    break FollowEnd::Arrived(format!(
                        "Arrived at {}. {}",
                        route.goal,
                        route.arrival_hint.clone().unwrap_or_default()
                    ));
                }
                _ => {}
            }

            let bearing = nav::bearing_to(&s, wp.x, wp.y);
            self.engine.turn_next_tic(bearing.clamp(-45.0, 45.0));
            self.engine.run_tic()?;
            used += 1;

            let now = self.engine.state();
            if now.health < s.health {
                break FollowEnd::Damaged;
            }
            if (now.x - s.x).hypot(now.y - s.y) > 64.0 {
                // Teleported: carry on from the waypoint after the teleporter.
                if matches!(wp.kind, WaypointKind::Teleport { .. }) {
                    idx += 1;
                    events.push("Went through a teleporter".into());
                } else {
                    break FollowEnd::Other("You were teleported somewhere unexpected.".into());
                }
                checkpoint = (now.x, now.y, used);
                continue;
            }
            let fresh: Vec<_> = self
                .visible_monsters()
                .into_iter()
                .filter(|m| !seen.contains(&m.0))
                .collect();
            if let Some((id, name, b)) = fresh.first() {
                seen.extend(fresh.iter().map(|m| m.0));
                break FollowEnd::Spotted(format!(
                    "{name} [{id}] came into view at bearing {b:+.0}°"
                ));
            }
            if used - checkpoint.2 >= 10 {
                if (now.x - checkpoint.0).hypot(now.y - checkpoint.1) < 12.0 {
                    // A floor, lift or door on the way that's still moving isn't
                    // an obstacle; wait for it (up to ~4 seconds).
                    if waited_for_mover < 140 && self.mover_nearby(&now, &wp) {
                        if waited_for_mover == 0 {
                            events.push("Waiting for a moving floor or door".into());
                        }
                        waited_for_mover += used - checkpoint.2;
                        checkpoint = (now.x, now.y, used);
                        continue;
                    }
                    if !tried_unstick {
                        // Probably snagged on a corner: back off and sidestep
                        // toward the waypoint, then carry on.
                        tried_unstick = true;
                        self.engine.release(std::mem::take(&mut held));
                        let side = if nav::bearing_to(&now, wp.x, wp.y) >= 0.0 {
                            Dir::Pos
                        } else {
                            Dir::Neg
                        };
                        for input in [
                            Input {
                                movement: Dir::Neg,
                                run,
                                tics: 6,
                                ..Default::default()
                            },
                            Input {
                                strafe: side,
                                run,
                                tics: 8,
                                ..Default::default()
                            },
                        ] {
                            self.engine.step(&input)?;
                            used += input.tics;
                        }
                        held = self.engine.press(&walk)?;
                        let now = self.engine.state();
                        checkpoint = (now.x, now.y, used);
                        continue;
                    }
                    break FollowEnd::Blocked;
                }
                checkpoint = (now.x, now.y, used);
            }
        };
        self.engine.release(held);
        events.push(format!("Followed the route for {used} tics"));
        Ok(end)
    }

    pub fn plan_text(&self, s: &State) -> Option<String> {
        let (plan, level) = self.plan.as_ref()?;
        if *level == (s.episode, s.map) {
            Some(plan.clone())
        } else {
            Some(format!(
                "(written for the previous level; replace it with set_plan)\n{plan}"
            ))
        }
    }
}
