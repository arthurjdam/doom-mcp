//! Reflexes: deciding what to do each tic.
//!
//! - `travel`: follow the route to a navigation goal (doors, lifts, getting unstuck)
//! - `combat`: choose a target and weapon, track it, shoot, dodge
//! - `Pilot` (here): combines the two under the commander's standing `Orders`
//!
//! The pilot reads the world and produces one tic's `Controls`; it never
//! advances the game itself. The caller runs the tic.

pub mod combat;
pub mod travel;

use crate::engine::ffi::State;
use crate::engine::{Controls, Engine};
use crate::world::nav::{Goal, Navigator};

use combat::{Combat, CombatEvent};
use travel::{Travel, TravelEvent};

/// After a fight, wait this long without targets before moving on.
const CALM_TICS: u32 = 10;
/// Re-plan the route this often while travelling (fights push us off route).
const REPLAN_TICS: u32 = 70;
/// After arriving somewhere (a key, a switch), pause this long, then plan again
/// (the next prerequisite or the goal itself).
const AFTER_ARRIVAL_TICS: u32 = 35;
/// After getting blocked or finding no route, try again after this long.
const RETRY_TICS: u32 = 105;

/// How readily the pilot fights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stance {
    /// Engage anything in sight, close in on distant targets.
    Aggressive,
    /// Engage monsters that are attacking or close by (default).
    #[default]
    Balanced,
    /// Only fight back against monsters attacking us; keep distance.
    Cautious,
    /// Never shoot.
    HoldFire,
}

impl Stance {
    pub fn name(self) -> &'static str {
        match self {
            Self::Aggressive => "aggressive",
            Self::Balanced => "balanced",
            Self::Cautious => "cautious",
            Self::HoldFire => "hold fire",
        }
    }
}

/// Standing orders from the commander. They last until changed.
#[derive(Debug, Clone)]
pub struct Orders {
    pub stance: Stance,
    /// Engage this thing (observation id) first whenever it's in sight.
    pub focus: Option<u32>,
    /// Move toward the navigation goal; false holds position (still fighting).
    pub travel: bool,
    /// Preferred weapon slot (1-7); `None` chooses automatically.
    pub weapon: Option<u8>,
    /// Press use on the next tic (one-shot).
    pub use_now: bool,
}

impl Default for Orders {
    fn default() -> Self {
        Self {
            stance: Stance::Balanced,
            focus: None,
            travel: true,
            weapon: None,
            use_now: false,
        }
    }
}

/// Something the pilot did or ran into, for the event log.
#[derive(Debug, Clone, PartialEq)]
pub enum PilotEvent {
    Travel(TravelEvent),
    Combat(CombatEvent),
    /// Pressed use on the switch or exit it arrived at.
    UsedSwitch(String),
}

pub struct Pilot {
    pub orders: Orders,
    travel: Travel,
    combat: Combat,
    /// Tics since the last target was in sight.
    calm: u32,
    /// The route is stale (we were fighting); plan afresh before travelling.
    replan: bool,
    /// Countdown to `travel.reset()` after an arrival or a failure.
    restart_travel_in: Option<u32>,
    /// Press use on the next tic (arrived at a switch).
    pending_use: bool,
    /// Name of the most recent combat target (for kill reports).
    last_target: Option<&'static str>,
}

impl Default for Pilot {
    fn default() -> Self {
        Self::new()
    }
}

impl Pilot {
    pub fn new() -> Self {
        Self {
            orders: Orders::default(),
            travel: Travel::new(true, Some(REPLAN_TICS)),
            combat: Combat::default(),
            calm: CALM_TICS,
            replan: false,
            restart_travel_in: None,
            pending_use: false,
            last_target: None,
        }
    }

    /// The monster most recently fought (it may just have died).
    pub fn last_target_name(&self) -> Option<&'static str> {
        self.last_target
    }

    /// Start over (new level, respawn, or a new goal).
    pub fn reset_travel(&mut self) {
        self.travel.reset();
        self.combat.reset();
        self.restart_travel_in = None;
        self.pending_use = false;
    }

    /// What the pilot is doing right now, in a few words.
    pub fn activity(&self) -> String {
        if let Some(t) = self.combat.target() {
            return format!("fighting {} [{}]", t.name, t.id);
        }
        if !self.orders.travel {
            return "standing still".into();
        }
        self.travel.activity().into()
    }

    /// The route the pilot is following, if it has one planned.
    pub fn route(&self) -> Option<&crate::world::nav::Route> {
        self.travel.route()
    }

    /// The route being followed right now, if any (for the minimap).
    pub fn route_points(&self) -> Vec<(f64, f64)> {
        self.travel.remaining_points()
    }

    /// Decide this tic's controls.
    pub fn tick(
        &mut self,
        engine: &mut Engine,
        nav: &mut Navigator,
        goal: &Goal,
        s: &State,
    ) -> (Controls, Vec<PilotEvent>) {
        let mut events = Vec::new();
        if std::mem::take(&mut self.orders.use_now) || std::mem::take(&mut self.pending_use) {
            return (
                Controls {
                    use_: true,
                    ..Default::default()
                },
                events,
            );
        }

        if let Some((controls, happened)) = self.combat.tick(engine, &self.orders, s) {
            self.last_target = self.combat.target().map(|t| t.name).or(self.last_target);
            self.calm = 0;
            self.replan = true;
            events.extend(happened.into_iter().map(PilotEvent::Combat));
            return (controls, events);
        }
        if self.calm < CALM_TICS {
            self.calm += 1;
            return (Controls::default(), events);
        }
        if !self.orders.travel {
            return (Controls::default(), events);
        }

        if let Some(n) = self.restart_travel_in {
            if n > 0 {
                self.restart_travel_in = Some(n - 1);
                return (Controls::default(), events);
            }
            self.restart_travel_in = None;
            self.travel.reset();
        }
        if std::mem::take(&mut self.replan) {
            self.travel.reset();
        }

        let (controls, happened) = self.travel.tick(engine, nav, goal, true);
        for e in happened {
            match &e {
                TravelEvent::Arrived { goal, switch, .. } => {
                    if *switch {
                        self.pending_use = true;
                        events.push(PilotEvent::UsedSwitch(goal.clone()));
                    }
                    self.restart_travel_in = Some(AFTER_ARRIVAL_TICS);
                }
                TravelEvent::Blocked
                | TravelEvent::DoorStuck
                | TravelEvent::NoRoute(_)
                | TravelEvent::AtLift => {
                    self.restart_travel_in = Some(RETRY_TICS);
                }
                _ => {}
            }
            events.push(PilotEvent::Travel(e));
        }
        (controls, events)
    }
}
