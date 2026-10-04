//! Real-time play: each tic, the pilot decides and the game advances; what
//! happened becomes events for the commander (`wait_for_events`).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::events::EventLog;
use super::session::Session;
use crate::engine::ffi::State;
use crate::engine::{Controls, TICRATE};
use crate::pilot::PilotEvent;
use crate::pilot::combat::CombatEvent;
use crate::pilot::travel::TravelEvent;
use crate::viewer::{self, LogKind};
use crate::world::nav::Goal;
use crate::world::observe::{GS_FINALE, GS_INTERMISSION, GS_LEVEL, PST_DEAD, thing_type};

/// Pause the game when the commander has been silent this long.
pub const IDLE_PAUSE: Duration = Duration::from_secs(60);
/// After dying, respawn after this many tics.
const RESPAWN_TICS: u32 = 3 * TICRATE;
/// On intermission/finale screens, press use this often to move on.
const CONTINUE_EVERY: u32 = 2 * TICRATE;
/// Damage is reported in batches at most this often.
const DAMAGE_BATCH_TICS: i32 = TICRATE as i32;
/// Update the minimap's route this often.
const SPECTATOR_EVERY: u32 = 7;

/// Monsters worth waking the commander for the first time they show up.
const BIG_MONSTERS: &[&str] = &[
    "Baron of Hell",
    "Cacodemon",
    "Hell Knight",
    "Cyberdemon",
    "Spider Mastermind",
    "Arch-vile",
    "Mancubus",
    "Revenant",
    "Pain Elemental",
    "Arachnotron",
];

/// Real-time bookkeeping that lives in the session.
pub struct Realtime {
    pub events: EventLog,
    last_contact: Instant,
    paused: bool,
    prev: Option<State>,
    damage: i32,
    damage_since: i32,
    dead_tics: u32,
    screen_tics: u32,
    warned_low: bool,
    warned_critical: bool,
    seen_types: HashSet<i32>,
    tics: u32,
}

impl Default for Realtime {
    fn default() -> Self {
        Self {
            events: EventLog::default(),
            last_contact: Instant::now(),
            paused: false,
            prev: None,
            damage: 0,
            damage_since: 0,
            dead_tics: 0,
            screen_tics: 0,
            warned_low: false,
            warned_critical: false,
            seen_types: HashSet::new(),
            tics: 0,
        }
    }
}

impl Session {
    /// Whether the game thread should run tics now: real-time mode, a game
    /// in progress (not the title screen), not paused, engine alive.
    pub fn ticking(&self) -> bool {
        let Some(rt) = &self.realtime else {
            return false;
        };
        if rt.paused || self.engine.check_alive().is_err() {
            return false;
        }
        let s = self.engine.state();
        if s.demoplayback != 0 {
            return false;
        }
        match s.gamestate {
            GS_LEVEL => s.in_level != 0,
            GS_INTERMISSION | GS_FINALE => true,
            _ => false,
        }
    }

    /// The commander did something: note it, and resume if idle-paused.
    pub fn touch(&mut self) {
        let Some(rt) = self.realtime.as_mut() else {
            return;
        };
        rt.last_contact = Instant::now();
        if rt.paused {
            rt.paused = false;
            self.event(false, "Resumed".to_string());
        }
    }

    /// Pause when the commander has gone quiet (the game shouldn't play on
    /// forever by itself).
    pub fn check_idle(&mut self) {
        let idle = self
            .realtime
            .as_ref()
            .is_some_and(|rt| rt.last_contact.elapsed() >= IDLE_PAUSE);
        if idle && self.ticking() {
            if let Some(rt) = self.realtime.as_mut() {
                rt.paused = true;
            }
            self.event(
                true,
                format!(
                    "Paused: no commands for {} seconds. Any tool call resumes.",
                    IDLE_PAUSE.as_secs()
                ),
            );
        }
    }

    /// Record an event (and show it on the spectator page).
    pub fn event(&mut self, important: bool, text: String) {
        let s = self.engine.state();
        let Some(rt) = self.realtime.as_mut() else {
            return;
        };
        if rt.events.push(
            s.gametic,
            f64::from(s.leveltime) / f64::from(TICRATE),
            important,
            text.clone(),
        ) {
            viewer::log(
                if important {
                    LogKind::Alert
                } else {
                    LogKind::Event
                },
                &text,
            );
        }
    }

    /// Clear the event log and per-level state (new game).
    pub fn reset_realtime(&mut self) {
        if self.realtime.is_some() {
            self.realtime = Some(Realtime::default());
        }
        self.pilot = crate::pilot::Pilot::new();
    }

    /// Advance the game by one tic in real-time mode.
    pub fn realtime_tick(&mut self) -> Result<()> {
        let s = self.engine.state();
        let Some(rt) = self.realtime.as_mut() else {
            return Ok(());
        };
        rt.tics += 1;

        match s.gamestate {
            GS_LEVEL if s.playerstate == PST_DEAD => {
                rt.dead_tics += 1;
                let respawn = rt.dead_tics == RESPAWN_TICS;
                self.engine.tic(&Controls {
                    use_: respawn,
                    ..Default::default()
                })?;
            }
            GS_LEVEL => {
                rt.dead_tics = 0;
                let goal = self.goal.clone();
                let (controls, happened) =
                    self.pilot.tick(&mut self.engine, &mut self.nav, &goal, &s);
                self.engine.tic(&controls)?;
                for e in happened {
                    self.pilot_event(e);
                }
            }
            _ => {
                rt.screen_tics += 1;
                let press = rt.screen_tics % CONTINUE_EVERY == 0;
                self.engine.tic(&Controls {
                    use_: press,
                    ..Default::default()
                })?;
            }
        }

        let now = self.engine.state();
        self.notice_changes(&s, &now);
        if let Some(rt) = self.realtime.as_mut() {
            rt.prev = Some(now);
            if rt.tics % SPECTATOR_EVERY == 0 {
                self.publish_realtime_spectator(&now);
            }
        }
        Ok(())
    }

    fn pilot_event(&mut self, e: PilotEvent) {
        let goal = self.goal.describe();
        let (important, text) = match e {
            PilotEvent::UsedSwitch(what) => (false, format!("Pressed the switch at {what}")),
            PilotEvent::Travel(t) => match t {
                TravelEvent::Arrived { goal: reached, .. } => {
                    (false, format!("Arrived at {reached}"))
                }
                TravelEvent::Blocked => (
                    true,
                    format!("Stuck on the way to {goal}; trying again shortly"),
                ),
                TravelEvent::NoRoute(why) => (true, format!("No route: {why}")),
                TravelEvent::DoorStuck => (
                    true,
                    "A door on the way won't open; trying again shortly".into(),
                ),
                TravelEvent::DoorOpened => (false, "Opened a door".into()),
                TravelEvent::RodeLift => (false, "Rode a lift".into()),
                TravelEvent::AtLift => (false, "At a lift".into()),
                TravelEvent::Teleported => (false, "Went through a teleporter".into()),
                TravelEvent::TeleportedUnexpectedly => {
                    (false, "Teleported somewhere unexpected".into())
                }
                TravelEvent::WaitingForMover => {
                    (false, "Waiting for a moving floor or door".into())
                }
            },
            PilotEvent::Combat(c) => match c {
                CombatEvent::Engaging { id, name, distance } => {
                    (false, format!("Engaging {name} [{id}] at {distance} units"))
                }
                CombatEvent::SwitchedWeapon(w) => (false, format!("Switched to the {w}")),
                CombatEvent::OutOfAmmo => (true, "Out of ammo: fighting with fists".into()),
            },
        };
        self.event(important, text);
    }

    /// Turn state changes into events.
    fn notice_changes(&mut self, before: &State, now: &State) {
        let new_level = (before.episode, before.map) != (now.episode, now.map)
            || (now.gamestate == GS_LEVEL && now.leveltime < before.leveltime);
        if new_level && now.gamestate == GS_LEVEL && now.in_level != 0 {
            let changed_map = (before.episode, before.map) != (now.episode, now.map);
            self.pilot.reset_travel();
            if changed_map {
                self.goal = Goal::Exit;
                self.pilot.orders.focus = None;
            }
            if let Some(rt) = self.realtime.as_mut() {
                rt.warned_low = false;
                rt.warned_critical = false;
                rt.damage = 0;
            }
            if let Some(briefing) = self.take_briefing(
                "The pilot is heading for the exit. Adjust with command (goal, stance, focus) and keep \
                 your plan current with set_plan.",
            ) {
                self.event(true, briefing);
            }
            self.publish_level_if_new(now);
        }

        if before.gamestate == GS_LEVEL && now.gamestate == GS_INTERMISSION {
            self.event(
                true,
                format!(
                    "Level complete! Kills {}/{}, items {}/{}, secrets {}/{}. Continuing to the next level.",
                    before.kills, before.totalkills, before.items, before.totalitems, before.secrets, before.totalsecrets
                ),
            );
            if let Some(rt) = self.realtime.as_mut() {
                rt.screen_tics = 0;
            }
        }
        if now.gamestate != GS_LEVEL || now.in_level == 0 {
            return;
        }
        if before.playerstate != PST_DEAD && now.playerstate == PST_DEAD {
            self.event(
                true,
                "You died. Respawning at the start of the level in 3 seconds.".into(),
            );
        }

        if now.kills > before.kills && !new_level {
            let name = self.pilot.last_target_name().unwrap_or("a monster");
            self.event(
                false,
                format!("Killed {name} ({}/{})", now.kills, now.totalkills),
            );
        }
        for message in self.engine.take_messages() {
            self.event(false, message);
        }

        // Damage, batched; health warnings.
        let lost = before.health - now.health;
        let mut damage_report = None;
        if let Some(rt) = self.realtime.as_mut() {
            if lost > 0 && !new_level {
                if rt.damage == 0 {
                    rt.damage_since = now.gametic;
                }
                rt.damage += lost;
            }
            if rt.damage > 0 && now.gametic - rt.damage_since >= DAMAGE_BATCH_TICS {
                damage_report = Some(rt.damage);
                rt.damage = 0;
            }
        }
        if let Some(amount) = damage_report {
            let mut text = format!("Took {amount} damage (health {})", now.health);
            if now.has_attacker != 0 {
                text += &format!(
                    ", from a {} at bearing {:+.0}°",
                    thing_type(now.attacker_type).0,
                    now.attacker_bearing
                );
            }
            self.event(false, text);
        }
        let (low, critical) = self
            .realtime
            .as_ref()
            .map(|r| (r.warned_low, r.warned_critical))
            .unwrap_or_default();
        if now.health > 0 && now.health < 25 && !critical {
            self.event(true, format!("Health critical: {}", now.health));
        } else if now.health > 0 && now.health < 50 && !low {
            self.event(true, format!("Health low: {}", now.health));
        }
        if let Some(rt) = self.realtime.as_mut() {
            if now.health > 0 && now.health < 25 {
                rt.warned_critical = true;
                rt.warned_low = true;
            } else if now.health > 0 && now.health < 50 {
                rt.warned_low = true;
            } else if now.health >= 60 {
                rt.warned_low = false;
                rt.warned_critical = false;
            }
        }

        // First sighting of each monster type.
        if self.realtime.as_ref().is_some_and(|r| r.tics % 5 == 0) {
            let visible: Vec<i32> = self
                .engine
                .things(2048.0)
                .iter()
                .filter(|t| {
                    t.health > 0 && t.line_of_sight != 0 && thing_type(t.type_).1 == "monster"
                })
                .map(|t| t.type_)
                .collect();
            for kind in visible {
                let first = self
                    .realtime
                    .as_mut()
                    .is_some_and(|r| r.seen_types.insert(kind));
                if first {
                    let name = thing_type(kind).0;
                    self.event(
                        BIG_MONSTERS.contains(&name),
                        format!("First {name} sighted"),
                    );
                }
            }
        }
    }

    fn publish_realtime_spectator(&mut self, s: &State) {
        if !viewer::enabled() {
            return;
        }
        if s.gamestate != GS_LEVEL || s.in_level == 0 {
            viewer::publish_status(None);
            return;
        }
        self.publish_level_if_new(s);
        viewer::publish_status(Some(&viewer::SpectatorStatus {
            goal: Some(self.goal.describe()),
            route_length: None,
            note: Some(self.pilot.activity()),
            route: self.pilot.route_points(),
            player: [s.x, s.y, s.angle],
        }));
    }
}
