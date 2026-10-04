//! Route following, one tic at a time.
//!
//! `Travel` walks the route to a navigation goal: it steers toward each
//! waypoint, turns in place on sharp corners, stops to open doors, waits for
//! moving floors, works itself free when snagged, and (optionally) rides
//! lifts. Each call to `tick` decides one tic's `Controls` and reports what
//! happened as `TravelEvent`s; the caller runs the tic and decides what the
//! events mean (the turn-based `follow_route` stops, the real-time pilot
//! carries on).

use crate::engine::ffi::State;
use crate::engine::{Controls, Dir, Engine};
use crate::world::nav::{self, Goal, Navigator, Route, WaypointKind};

/// Distance at which a plain waypoint counts as reached.
const ARRIVE: f64 = 24.0;
/// Distance at which doors, lifts and the goal count as reached.
const ARRIVE_STOP: f64 = 20.0;
/// Tics to wait for a door to open after pressing use.
const DOOR_WAIT: u32 = 70;
/// Sharper than this, turn in place instead of walking while turning.
const TURN_IN_PLACE: f64 = 60.0;
/// Stuck check: must move at least STUCK_DISTANCE every STUCK_TICS.
const STUCK_TICS: u32 = 10;
const STUCK_DISTANCE: f64 = 12.0;
/// Longest wait for a moving floor or door in the way.
const MOVER_WAIT: u32 = 140;
/// Unstick manoeuvre: back up, then sidestep toward the waypoint.
const UNSTICK_BACK: u32 = 6;
const UNSTICK_SIDE: u32 = 8;
/// Longest turn-in-place before giving up on facing something exactly.
const FACE_BUDGET: u32 = 20;
/// A position jump bigger than this in one tic is a teleport.
const TELEPORT_JUMP: f64 = 64.0;
/// Lift phases: how long to wait for it to come down, to step on, to ride.
const LIFT_DOWN_WAIT: u32 = 105;
const LIFT_BOARD_WAIT: u32 = 45;
const LIFT_RIDE_WAIT: u32 = 210;

/// What happened during a tic of travel.
#[derive(Debug, Clone, PartialEq)]
pub enum TravelEvent {
    /// Reached the goal. `switch` means it's something to `use` (and we're facing it).
    Arrived {
        goal: String,
        hint: Option<String>,
        switch: bool,
    },
    /// Stopped in front of a lift, facing it (only when lifts aren't automatic).
    AtLift,
    RodeLift,
    DoorOpened,
    /// Pressed use on a door but it didn't open.
    DoorStuck,
    /// Not making progress, even after trying to get unstuck.
    Blocked,
    NoRoute(String),
    Teleported,
    TeleportedUnexpectedly,
    /// Started waiting for a moving floor, lift or door in the way.
    WaitingForMover,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Then {
    Arrive,
    OpenDoor(usize),
    Lift(usize),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Walking,
    /// Turn in place toward (x, y), then do `then`.
    Face {
        x: f64,
        y: f64,
        then: Then,
        budget: u32,
    },
    DoorUse {
        sector: usize,
    },
    DoorWait {
        sector: usize,
        waited: u32,
    },
    Unstick {
        tic: u32,
        side: Dir,
    },
    LiftUse {
        sector: usize,
    },
    LiftDown {
        sector: usize,
        waited: u32,
    },
    LiftBoard {
        sector: usize,
        waited: u32,
    },
    LiftRide {
        sector: usize,
        waited: u32,
    },
    /// Done (arrived, blocked, door stuck, or waiting at a lift): hold still
    /// until `reset`.
    Stopped,
}

pub struct Travel {
    /// Ride lifts automatically; otherwise stop in front of them (`AtLift`).
    auto_lifts: bool,
    /// Re-plan the route this often while walking (real-time play drifts off
    /// route while fighting); `None` plans once.
    replan_every: Option<u32>,
    route: Option<Route>,
    planned_for: Option<Goal>,
    since_plan: u32,
    idx: usize,
    phase: Phase,
    last_pos: Option<(f64, f64)>,
    /// Tics travelled, and the position/tic of the last stuck check.
    tics: u32,
    checkpoint: (f64, f64, u32),
    waited_for_mover: u32,
    tried_unstick: bool,
    /// Tics until the next planning attempt after a failure.
    retry_in: u32,
}

impl Travel {
    pub fn new(auto_lifts: bool, replan_every: Option<u32>) -> Self {
        Self {
            auto_lifts,
            replan_every,
            route: None,
            planned_for: None,
            since_plan: 0,
            idx: 0,
            phase: Phase::Walking,
            last_pos: None,
            tics: 0,
            checkpoint: (0.0, 0.0, 0),
            waited_for_mover: 0,
            tried_unstick: false,
            retry_in: 0,
        }
    }

    /// Forget the route and any stop; the next tick plans afresh.
    pub fn reset(&mut self) {
        *self = Self::new(self.auto_lifts, self.replan_every);
    }

    /// The route being followed, if one is planned.
    pub fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    /// The rest of the current route (for the minimap).
    pub fn remaining_points(&self) -> Vec<(f64, f64)> {
        self.route
            .as_ref()
            .map(|r| {
                r.waypoints
                    .iter()
                    .skip(self.idx)
                    .map(|w| (w.x.round(), w.y.round()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Short description of what travel is doing, for status reports.
    pub fn activity(&self) -> &'static str {
        match self.phase {
            Phase::Walking => "walking the route",
            Phase::Face { .. } => "turning",
            Phase::DoorUse { .. } | Phase::DoorWait { .. } => "opening a door",
            Phase::Unstick { .. } => "getting unstuck",
            Phase::LiftUse { .. }
            | Phase::LiftDown { .. }
            | Phase::LiftBoard { .. }
            | Phase::LiftRide { .. } => "riding a lift",
            Phase::Stopped => "stopped",
        }
    }

    /// Decide this tic's controls for travelling toward `goal`.
    pub fn tick(
        &mut self,
        engine: &mut Engine,
        nav: &mut Navigator,
        goal: &Goal,
        run: bool,
    ) -> (Controls, Vec<TravelEvent>) {
        let mut events = Vec::new();
        let s = engine.state();
        let moved = self
            .last_pos
            .map(|(x, y)| (s.x - x).hypot(s.y - y))
            .unwrap_or(0.0);
        self.last_pos = Some((s.x, s.y));
        self.tics += 1;
        self.since_plan += 1;

        if moved > TELEPORT_JUMP && self.phase != Phase::Stopped {
            let expected = self
                .current()
                .is_some_and(|w| matches!(w.kind, WaypointKind::Teleport { .. }));
            if expected {
                self.idx += 1;
                events.push(TravelEvent::Teleported);
            } else {
                events.push(TravelEvent::TeleportedUnexpectedly);
                self.route = None;
            }
            self.checkpoint = (s.x, s.y, self.tics);
        }

        let wants_plan = self.route.is_none()
            || self.planned_for.as_ref() != Some(goal)
            || (self.phase == Phase::Walking
                && self.replan_every.is_some_and(|n| self.since_plan >= n));
        if wants_plan && self.phase != Phase::Stopped {
            if self.retry_in > 0 {
                self.retry_in -= 1;
                return (Controls::default(), events);
            }
            match nav.route(engine, goal) {
                Ok(route) => {
                    self.route = Some(route);
                    self.planned_for = Some(goal.clone());
                    self.since_plan = 0;
                    self.idx = 0;
                    if !matches!(self.phase, Phase::Walking) {
                        self.phase = Phase::Walking;
                    }
                    self.checkpoint = (s.x, s.y, self.tics);
                }
                Err(e) => {
                    self.route = None;
                    self.retry_in = 35;
                    events.push(TravelEvent::NoRoute(e));
                    return (Controls::default(), events);
                }
            }
        }

        let controls = self.decide(engine, &s, run, &mut events);
        (controls, events)
    }

    fn current(&self) -> Option<nav::Waypoint> {
        self.route
            .as_ref()
            .and_then(|r| r.waypoints.get(self.idx).copied())
    }

    fn arrived_event(&self, switch: bool) -> TravelEvent {
        let route = self.route.as_ref();
        TravelEvent::Arrived {
            goal: route.map(|r| r.goal.clone()).unwrap_or_default(),
            hint: route.and_then(|r| r.arrival_hint.clone()),
            switch,
        }
    }

    fn decide(
        &mut self,
        engine: &mut Engine,
        s: &State,
        run: bool,
        events: &mut Vec<TravelEvent>,
    ) -> Controls {
        let hold = Controls::default();
        match self.phase {
            Phase::Stopped => hold,
            Phase::Face { x, y, then, budget } => {
                let b = nav::bearing_to(s, x, y);
                if b.abs() >= 1.0 && budget > 0 {
                    self.phase = Phase::Face {
                        x,
                        y,
                        then,
                        budget: budget - 1,
                    };
                    return Controls { turn: b, ..hold };
                }
                match then {
                    Then::Arrive => {
                        self.phase = Phase::Stopped;
                        events.push(self.arrived_event(true));
                        hold
                    }
                    Then::OpenDoor(sector) => {
                        self.phase = Phase::DoorUse { sector };
                        self.decide(engine, s, run, events)
                    }
                    Then::Lift(sector) if self.auto_lifts => {
                        self.phase = Phase::LiftUse { sector };
                        self.decide(engine, s, run, events)
                    }
                    Then::Lift(_) => {
                        self.phase = Phase::Stopped;
                        events.push(TravelEvent::AtLift);
                        hold
                    }
                }
            }
            Phase::DoorUse { sector } => {
                self.phase = Phase::DoorWait { sector, waited: 0 };
                Controls { use_: true, ..hold }
            }
            Phase::DoorWait { sector, waited } => {
                if door_open(engine, sector) {
                    events.push(TravelEvent::DoorOpened);
                    self.idx += 1;
                    self.phase = Phase::Walking;
                    self.checkpoint = (s.x, s.y, self.tics);
                    return self.decide(engine, s, run, events);
                }
                if waited >= DOOR_WAIT {
                    self.phase = Phase::Stopped;
                    events.push(TravelEvent::DoorStuck);
                    return hold;
                }
                self.phase = Phase::DoorWait {
                    sector,
                    waited: waited + 1,
                };
                hold
            }
            Phase::Unstick { tic, side } => {
                if tic >= UNSTICK_BACK + UNSTICK_SIDE {
                    self.phase = Phase::Walking;
                    self.checkpoint = (s.x, s.y, self.tics);
                    return self.decide(engine, s, run, events);
                }
                self.phase = Phase::Unstick { tic: tic + 1, side };
                if tic < UNSTICK_BACK {
                    Controls {
                        movement: Dir::Neg,
                        run,
                        ..hold
                    }
                } else {
                    Controls {
                        strafe: side,
                        run,
                        ..hold
                    }
                }
            }
            Phase::LiftUse { sector } => {
                self.phase = Phase::LiftDown { sector, waited: 0 };
                Controls { use_: true, ..hold }
            }
            Phase::LiftDown { sector, waited } => {
                let down = engine
                    .sectors()
                    .get(sector)
                    .is_some_and(|l| l.floor - s.z <= 24.0);
                if down || waited >= LIFT_DOWN_WAIT {
                    self.phase = Phase::LiftBoard { sector, waited: 0 };
                    return self.decide(engine, s, run, events);
                }
                self.phase = Phase::LiftDown {
                    sector,
                    waited: waited + 1,
                };
                hold
            }
            Phase::LiftBoard { sector, waited } => {
                if engine.point_sector(s.x, s.y) == Some(sector) || waited >= LIFT_BOARD_WAIT {
                    self.phase = Phase::LiftRide { sector, waited: 0 };
                    return hold;
                }
                self.phase = Phase::LiftBoard {
                    sector,
                    waited: waited + 1,
                };
                // Walk toward the waypoint after the lift: it lies across it.
                let next = self
                    .route
                    .as_ref()
                    .and_then(|r| r.waypoints.get(self.idx + 1).copied());
                match next {
                    Some(w) => {
                        let b = nav::bearing_to(s, w.x, w.y);
                        Controls {
                            movement: if b.abs() <= TURN_IN_PLACE {
                                Dir::Pos
                            } else {
                                Dir::None
                            },
                            turn: b,
                            ..hold
                        }
                    }
                    None => Controls {
                        movement: Dir::Pos,
                        ..hold
                    },
                }
            }
            Phase::LiftRide { sector, waited } => {
                let moving = engine.sectors().get(sector).is_some_and(|l| l.moving != 0);
                // Ride until the lift has gone up and stopped (or give up).
                if (!moving && waited > 35) || waited >= LIFT_RIDE_WAIT {
                    events.push(TravelEvent::RodeLift);
                    self.route = None; // re-plan from the top
                    self.phase = Phase::Walking;
                    return hold;
                }
                self.phase = Phase::LiftRide {
                    sector,
                    waited: waited + 1,
                };
                hold
            }
            Phase::Walking => self.walk(engine, s, run, events),
        }
    }

    fn walk(
        &mut self,
        engine: &mut Engine,
        s: &State,
        run: bool,
        events: &mut Vec<TravelEvent>,
    ) -> Controls {
        let hold = Controls::default();
        // Skip past waypoints already reached; stop at doors, lifts and the goal.
        let wp = loop {
            let Some(wp) = self.current() else {
                self.phase = Phase::Stopped;
                events.push(self.arrived_event(false));
                return hold;
            };
            let dist = (wp.x - s.x).hypot(wp.y - s.y);
            match wp.kind {
                WaypointKind::Walk if dist < ARRIVE => self.idx += 1,
                WaypointKind::Door { sector, fx, fy } if dist < ARRIVE_STOP => {
                    self.phase = Phase::Face {
                        x: fx,
                        y: fy,
                        then: Then::OpenDoor(sector),
                        budget: FACE_BUDGET,
                    };
                    return self.decide(engine, s, run, events);
                }
                WaypointKind::Lift { sector, fx, fy } if dist < ARRIVE_STOP => {
                    self.phase = Phase::Face {
                        x: fx,
                        y: fy,
                        then: Then::Lift(sector),
                        budget: FACE_BUDGET,
                    };
                    return self.decide(engine, s, run, events);
                }
                WaypointKind::Goal { fx, fy, face } if dist < ARRIVE_STOP => {
                    if face {
                        self.phase = Phase::Face {
                            x: fx,
                            y: fy,
                            then: Then::Arrive,
                            budget: FACE_BUDGET,
                        };
                        return self.decide(engine, s, run, events);
                    }
                    self.phase = Phase::Stopped;
                    events.push(self.arrived_event(false));
                    return hold;
                }
                _ => break wp,
            }
        };

        // Stuck check.
        if self.tics - self.checkpoint.2 >= STUCK_TICS {
            let progress = (s.x - self.checkpoint.0).hypot(s.y - self.checkpoint.1);
            if progress < STUCK_DISTANCE {
                if self.waited_for_mover < MOVER_WAIT && mover_between(engine, s, wp.x, wp.y) {
                    // A floor, lift or door on the way that's still moving isn't
                    // an obstacle; wait for it.
                    if self.waited_for_mover == 0 {
                        events.push(TravelEvent::WaitingForMover);
                    }
                    self.waited_for_mover += self.tics - self.checkpoint.2;
                } else if !self.tried_unstick {
                    // Probably snagged on a corner: back off and sidestep toward the waypoint.
                    self.tried_unstick = true;
                    let side = if nav::bearing_to(s, wp.x, wp.y) >= 0.0 {
                        Dir::Pos
                    } else {
                        Dir::Neg
                    };
                    self.phase = Phase::Unstick { tic: 0, side };
                    return self.decide(engine, s, run, events);
                } else {
                    self.phase = Phase::Stopped;
                    events.push(TravelEvent::Blocked);
                    return hold;
                }
            }
            self.checkpoint = (s.x, s.y, self.tics);
        }

        // Steer smoothly; on sharp corners turn in place rather than arc into a wall.
        let b = nav::bearing_to(s, wp.x, wp.y);
        Controls {
            movement: if b.abs() <= TURN_IN_PLACE {
                Dir::Pos
            } else {
                Dir::None
            },
            turn: b,
            run,
            ..hold
        }
    }
}

fn door_open(engine: &Engine, sector: usize) -> bool {
    engine
        .sectors()
        .get(sector)
        .is_some_and(|s| s.ceiling - s.floor >= 56.0)
}

/// Is a sector between the player and (x, y) currently moving?
fn mover_between(engine: &Engine, s: &State, x: f64, y: f64) -> bool {
    let sectors = engine.sectors();
    (0..=4).any(|i| {
        let t = f64::from(i) / 4.0;
        engine
            .point_sector(s.x + (x - s.x) * t, s.y + (y - s.y) * t)
            .and_then(|sec| sectors.get(sec))
            .is_some_and(|sec| sec.moving != 0)
    })
}
