//! Everything that lives on the engine thread besides the engine itself:
//! navigation, the model's plan, and per-level bookkeeping.

use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

use anyhow::Result;

use crate::engine::Engine;
use crate::engine::ffi::State;
use crate::pilot::Pilot;
use crate::pilot::travel::{Travel, TravelEvent};

use super::realtime::Realtime;
use crate::viewer;
use crate::world::map;
use crate::world::nav::{self, Goal, NavSummary, Navigator, NextWaypoint, Route};
use crate::world::observe::{self, thing_type};

const PST_DEAD: i32 = 1;

pub struct Session {
    pub engine: Engine,
    pub nav: Navigator,
    /// Real-time reflexes (also used in turn mode by `follow`'s route following).
    pub pilot: Pilot,
    /// Real-time mode bookkeeping and event log; `None` in turn mode.
    pub realtime: Option<Realtime>,
    pub goal: Goal,
    /// The model's plan and the level it was written for.
    pub plan: Option<(String, (i32, i32))>,
    /// Level the briefing was last given for.
    briefed: Option<(i32, i32, i32)>,
    /// Level whose geometry the spectator page has.
    published_level: Option<(i32, i32)>,
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
    pub fn new(engine: Engine, realtime: bool) -> Self {
        Self {
            engine,
            nav: Navigator::default(),
            pilot: Pilot::new(),
            realtime: realtime.then(Realtime::default),
            goal: Goal::Exit,
            plan: None,
            briefed: None,
            published_level: None,
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
    /// `next` is the closing advice, which differs between modes.
    pub fn take_briefing(&mut self, next: &str) -> Option<String> {
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

        let level_name = observe::level_name(&s);
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
        b.push_str(next);
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

    /// Walk the route to the current goal for up to `tics` tics (turn-based
    /// `follow_route`), using the pilot's route following. Stops early when
    /// something needs the model: a new monster in view, damage, arriving
    /// somewhere, or getting stuck.
    pub fn follow(&mut self, tics: u32, run: bool, events: &mut Vec<String>) -> Result<FollowEnd> {
        let start = self.engine.state();
        let level = (start.episode, start.map);
        let mut seen: HashSet<u32> = self.visible_monsters().into_iter().map(|m| m.0).collect();
        // Lifts are handed back to the model; one plan per call.
        let mut travel = Travel::new(false, None);
        let mut used = 0;
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

            let (controls, happened) =
                travel.tick(&mut self.engine, &mut self.nav, &self.goal, run);
            let mut stop = None;
            for event in happened {
                match event {
                    TravelEvent::DoorOpened => events.push("Opened a door".into()),
                    TravelEvent::Teleported => events.push("Went through a teleporter".into()),
                    TravelEvent::WaitingForMover => events.push("Waiting for a moving floor or door".into()),
                    TravelEvent::RodeLift => events.push("Rode a lift".into()),
                    TravelEvent::Arrived { goal, hint, .. } => {
                        stop = Some(FollowEnd::Arrived(format!("Arrived at {goal}. {}", hint.unwrap_or_default())))
                    }
                    TravelEvent::AtLift => {
                        stop = Some(FollowEnd::Arrived(
                            "Reached a lift and facing it. Press use (act use=true), wait ~1s for it to \
                             lower, walk onto it, wait for it to rise, then continue following the route."
                                .into(),
                        ))
                    }
                    TravelEvent::DoorStuck => {
                        stop = Some(FollowEnd::Arrived(
                            "Reached a door but it didn't open. It may need a key, a switch, or to be \
                             used from the other side."
                                .into(),
                        ))
                    }
                    TravelEvent::Blocked => stop = Some(FollowEnd::Blocked),
                    TravelEvent::NoRoute(e) => stop = Some(FollowEnd::NoRoute(e)),
                    TravelEvent::TeleportedUnexpectedly => {
                        stop = Some(FollowEnd::Other("You were teleported somewhere unexpected.".into()))
                    }
                }
            }
            if let Some(end) = stop {
                break end;
            }

            self.engine.tic(&controls)?;
            used += 1;

            let now = self.engine.state();
            if now.health < s.health {
                break FollowEnd::Damaged;
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
        };
        self.engine.release_controls();
        if used > 0 {
            events.push(format!("Followed the route for {used} tics"));
        }
        Ok(end)
    }

    /// Send the level's geometry to the minimap when the level changes.
    pub fn publish_level_if_new(&mut self, s: &State) {
        let id = (s.episode, s.map);
        if self.published_level == Some(id) {
            return;
        }
        self.published_level = Some(id);
        let lines: Vec<viewer::MapLine> = self
            .engine
            .lines()
            .iter()
            .map(|l| {
                let kind = map::classify(l).unwrap_or(if l.two_sided != 0 { '-' } else { '#' });
                (l.x1, l.y1, l.x2, l.y2, kind)
            })
            .collect();
        viewer::publish_level(&observe::level_name(s), &lines);
    }

    /// Send the spectator page's minimap data: the level geometry (when the
    /// level changes) and the current route and goal.
    pub fn publish_spectator(&mut self, nav: Option<&NavSummary>) {
        if !viewer::enabled() {
            return;
        }
        let s = self.engine.state();
        if s.in_level == 0 || s.demoplayback != 0 || s.gamestate != 0 {
            viewer::publish_status(None);
            return;
        }
        self.publish_level_if_new(&s);
        let route = self
            .route()
            .map(|r| {
                r.waypoints
                    .iter()
                    .map(|w| (w.x.round(), w.y.round()))
                    .collect()
            })
            .unwrap_or_default();
        viewer::publish_status(Some(&viewer::SpectatorStatus {
            goal: nav.map(|n| n.goal.clone()),
            route_length: nav.and_then(|n| n.route_length),
            note: nav.and_then(|n| n.note.clone()),
            route,
            player: [s.x, s.y, s.angle],
        }));
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
