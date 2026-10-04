//! Route planning over the level's real geometry.
//!
//! The level is rasterised into a grid of 16-unit cells. Cells too close to a
//! solid wall (or a solid decoration) for the 16-unit-radius player are
//! blocked. Moving between neighbouring cells checks every two-sided linedef
//! the move crosses, using Doom's own movement rules: steps of at most 24
//! units, at least 56 units of headroom, doors that can be opened (with the
//! right key), and lifts that can be lowered.
//!
//! When the goal can't be reached, the planner works out why: a locked door
//! (go get the key first) or a sector moved by a switch or walk-over trigger
//! elsewhere (go trigger that first), and routes to that prerequisite instead.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use serde::Serialize;

use crate::engine::Engine;
use crate::engine::ffi::{Line, MF_COUNTKILL, MF_SOLID, ML_BLOCKING, ML_MAPPED, Sector, State};
use crate::world::observe::thing_type;

pub const CELL: f64 = 16.0;
/// How close a cell centre may be to a wall. A bit under the player's 16-unit
/// radius so grid rounding doesn't seal off tight (but passable) gaps.
const CLEARANCE: f64 = 14.0;
/// Cells this close to a wall cost extra, so routes keep off walls and corners.
const COMFORT: f64 = 40.0;
const MAX_STEP: f64 = 24.0;
const PLAYER_HEIGHT: f64 = 56.0;
/// Standing distance in front of a switch.
const SWITCH_STANDOFF: f64 = 32.0;
/// How many prerequisites (key -> switch -> ...) to chain.
const MAX_CHAIN: usize = 6;

const MANUAL_DOORS: &[i32] = &[1, 31, 117, 118];
const LIFTS: &[i32] = &[10, 21, 62, 88, 120, 121, 122, 123];
/// Stair builders: raise the tagged sector and then neighbours with the same
/// floor texture, one after another.
const STAIRS: &[i32] = &[7, 8, 100, 127];
const SWITCH_EXITS: &[i32] = &[11];
const WALK_EXITS: &[i32] = &[52];
const SWITCH_SECRET_EXITS: &[i32] = &[51];
const WALK_SECRET_EXITS: &[i32] = &[124];
/// Player teleporters (walk across from the front side).
const TELEPORTS: &[i32] = &[39, 97];
/// Thing type index of a teleport destination (MT_TELEPORTMAN).
const MT_TELEPORTMAN: i32 = 41;
/// Sector specials that hurt to stand on (nukage, lava...).
const HAZARD_SECTORS: &[i32] = &[4, 5, 7, 11, 16];
/// Line specials activated by walking over them.
const WALK_SPECIALS: &[i32] = &[
    2, 3, 4, 5, 6, 8, 10, 12, 13, 16, 17, 19, 22, 25, 30, 35, 36, 37, 38, 39, 40, 44, 52, 53, 54,
    56, 57, 58, 59, 72, 73, 74, 75, 76, 77, 79, 80, 81, 82, 83, 84, 86, 87, 88, 89, 90, 91, 92, 93,
    94, 95, 96, 97, 98, 100, 104, 105, 106, 107, 108, 109, 110, 119, 120, 121, 124, 125, 126, 128,
    129, 130, 141,
];
/// Line specials activated by shooting them.
const GUN_SPECIALS: &[i32] = &[24, 46, 47];
/// Tagged specials that never open a way through: closing doors, crushers,
/// lights, teleporters, exits.
const NOT_OPENING: &[i32] = &[
    3, 6, 11, 12, 13, 16, 17, 25, 35, 39, 41, 42, 43, 44, 49, 50, 51, 52, 57, 73, 74, 75, 76, 77,
    79, 80, 81, 97, 104, 107, 110, 113, 116, 124, 125, 126, 138, 139, 141,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyColor {
    Blue,
    Yellow,
    Red,
}

impl KeyColor {
    pub fn name(self) -> &'static str {
        match self {
            Self::Blue => "blue",
            Self::Yellow => "yellow",
            Self::Red => "red",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Blue => 0,
            Self::Yellow => 1,
            Self::Red => 2,
        }
    }

    /// Either the keycard or the skull key of this colour opens its doors.
    pub fn held(self, s: &State) -> bool {
        s.cards[self.index()] != 0 || s.cards[self.index() + 3] != 0
    }

    fn of_door(special: i32) -> Option<Self> {
        match special {
            26 | 32 | 99 | 133 => Some(Self::Blue),
            27 | 34 | 136 | 137 => Some(Self::Yellow),
            28 | 33 | 134 | 135 => Some(Self::Red),
            _ => None,
        }
    }

    fn of_thing(name: &str) -> Option<Self> {
        if name.starts_with("Blue") {
            Some(Self::Blue)
        } else if name.starts_with("Yellow") {
            Some(Self::Yellow)
        } else if name.starts_with("Red") {
            Some(Self::Red)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Door {
    No,
    Manual,
    Locked(KeyColor),
}

/// Where the route is headed.
#[derive(Debug, Clone, PartialEq)]
pub enum Goal {
    Exit,
    SecretExit,
    Key(KeyColor),
    /// The nearest switch that hasn't been used yet.
    Switch,
    /// The nearest area not seen yet.
    Explore,
    /// A thing by its observation id.
    Thing(u32),
    Point(f64, f64),
    /// Whatever triggers the movement of this sector (internal prerequisite).
    Trigger(usize),
}

impl Goal {
    pub fn describe(&self) -> String {
        match self {
            Self::Exit => "the exit".into(),
            Self::SecretExit => "the secret exit".into(),
            Self::Key(k) => format!("the {} key", k.name()),
            Self::Switch => "the nearest unused switch".into(),
            Self::Explore => "the nearest unexplored area".into(),
            Self::Thing(id) => format!("thing #{id}"),
            Self::Point(x, y) => format!("({x:.0}, {y:.0})"),
            Self::Trigger(_) => "the switch or trigger that opens the way".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WaypointKind {
    Walk,
    /// Stand here facing (fx, fy) and press use to open the door.
    Door {
        sector: usize,
        fx: f64,
        fy: f64,
    },
    /// Stand here facing (fx, fy): press use to lower the lift, then ride it.
    Lift {
        sector: usize,
        fx: f64,
        fy: f64,
    },
    /// Walk across the teleporter here; you come out at (dx, dy).
    Teleport {
        dx: f64,
        dy: f64,
    },
    /// The destination; face (fx, fy) if it is a switch.
    Goal {
        fx: f64,
        fy: f64,
        face: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Waypoint {
    pub x: f64,
    pub y: f64,
    #[serde(flatten)]
    pub kind: WaypointKind,
}

impl WaypointKind {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Door { .. } => "door (open it with use)",
            Self::Lift { .. } => "lift (use it to lower it, step on, wait to ride up)",
            Self::Teleport { .. } => "teleporter (walk onto the pad)",
            Self::Goal { face: true, .. } => "destination (face it and press use)",
            Self::Goal { .. } => "destination",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Route {
    pub goal: String,
    /// Why the route goes somewhere other than the requested goal first.
    pub prerequisite: Option<String>,
    /// What to do on arrival.
    pub arrival_hint: Option<String>,
    pub waypoints: Vec<Waypoint>,
    pub length: f64,
    pub crosses_hazard: bool,
}

struct Target {
    x: f64,
    y: f64,
    /// Point to face on arrival (switches).
    face: Option<(f64, f64)>,
    hint: Option<String>,
    /// Sectors the player may stand in to reach it (empty = wherever is closest).
    sectors: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Cross {
    Plain,
    Door(usize),
    Lift(usize),
    /// Only passable after a trigger elsewhere moves this sector (relaxed search).
    Trigger(usize),
}

/// What a search may assume away.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Relax<'a> {
    locks: bool,
    triggers: bool,
    /// Key colours whose doors may not be assumed open (we're looking for that key).
    forbid: [bool; 3],
    /// Sectors whose trigger we're already trying to reach in this chain.
    forbid_sectors: &'a [usize],
}

impl Relax<'static> {
    const NONE: Self = Self {
        locks: false,
        triggers: false,
        forbid: [false; 3],
        forbid_sectors: &[],
    };
}

/// Where we are in a chain of prerequisites (exit <- key <- switch ...).
#[derive(Clone, Default)]
struct Chain {
    depth: usize,
    /// Keys being sought: their doors may not be assumed open.
    forbid: [bool; 3],
    /// Sectors whose trigger is being sought: they may not be assumed moved.
    forbid_sectors: Vec<usize>,
}

/// Static navigation data for one level.
pub struct Level {
    pub id: (i32, i32),
    ox: f64,
    oy: f64,
    w: usize,
    h: usize,
    cell_sector: Vec<u32>,
    wall: Vec<bool>,
    near_wall: Vec<bool>,
    /// Two-sided lines near each cell, for transition checks.
    cell_lines: Vec<Vec<u32>>,
    lines: Vec<Line>,
    door: Vec<Door>,
    lift: Vec<bool>,
    lift_low: Vec<f64>,
    rest_floor: Vec<f64>,
    hazard: Vec<bool>,
    /// Lines whose (tagged) special moves each sector in a way that may open a path.
    triggers: Vec<Vec<u32>>,
    /// Teleporter pads: (cell on the front side, line, destination cell).
    teleports: Vec<(usize, u32, usize)>,
}

fn is_wall(l: &Line) -> bool {
    l.two_sided == 0 || l.flags & ML_BLOCKING != 0
}

/// Distance from point p to segment ab.
fn seg_dist(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0)
    };
    (px - ax - t * dx).hypot(py - ay - t * dy)
}

/// Parameter along p->q where it crosses line l, if it does.
fn crossing(p: (f64, f64), q: (f64, f64), l: &Line) -> Option<f64> {
    let (rx, ry) = (q.0 - p.0, q.1 - p.1);
    let (sx, sy) = (l.x2 - l.x1, l.y2 - l.y1);
    let denom = rx * sy - ry * sx;
    if denom.abs() < 1e-9 {
        return None;
    }
    let (wx, wy) = (l.x1 - p.0, l.y1 - p.1);
    let t = (wx * sy - wy * sx) / denom;
    let u = (wx * ry - wy * rx) / denom;
    (t > 1e-9 && t <= 1.0 && (0.0..=1.0).contains(&u)).then_some(t)
}

/// True when p is on the line's front (right-hand) side.
fn on_front(p: (f64, f64), l: &Line) -> bool {
    (l.x2 - l.x1) * (p.1 - l.y1) - (l.y2 - l.y1) * (p.0 - l.x1) < 0.0
}

/// Per-query state that changes as the game runs.
struct Dynamic {
    sectors: Vec<Sector>,
    /// Current line specials (one-shot triggers clear theirs once used).
    specials: Vec<i32>,
    blocked: Vec<bool>,
    keys: [bool; 3],
}

impl Level {
    pub fn build(engine: &Engine, id: (i32, i32)) -> Self {
        let lines = engine.lines();
        let sectors = engine.sectors();
        let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for l in &lines {
            minx = minx.min(l.x1.min(l.x2));
            miny = miny.min(l.y1.min(l.y2));
            maxx = maxx.max(l.x1.max(l.x2));
            maxy = maxy.max(l.y1.max(l.y2));
        }
        let (ox, oy) = (minx - CELL, miny - CELL);
        let w = ((maxx - ox) / CELL).ceil() as usize + 2;
        let h = ((maxy - oy) / CELL).ceil() as usize + 2;

        let mut level = Self {
            id,
            ox,
            oy,
            w,
            h,
            cell_sector: vec![u32::MAX; w * h],
            wall: vec![false; w * h],
            near_wall: vec![false; w * h],
            cell_lines: vec![Vec::new(); w * h],
            lines,
            door: vec![Door::No; sectors.len()],
            lift: vec![false; sectors.len()],
            lift_low: sectors.iter().map(|s| s.floor).collect(),
            rest_floor: sectors.iter().map(|s| s.floor).collect(),
            hazard: sectors
                .iter()
                .map(|s| HAZARD_SECTORS.contains(&s.special))
                .collect(),
            triggers: vec![Vec::new(); sectors.len()],
            teleports: Vec::new(),
        };

        for c in 0..w * h {
            let (x, y) = level.center(c);
            if let Some(s) = engine.point_sector(x, y) {
                level.cell_sector[c] = s as u32;
            }
        }

        for i in 0..level.lines.len() {
            let l = level.lines[i];
            let mut cells = Vec::new();
            if is_wall(&l) {
                level.for_near(&l, CLEARANCE, |c| cells.push(c));
                for c in cells {
                    level.wall[c] = true;
                }
                let mut near = Vec::new();
                level.for_near(&l, COMFORT, |c| near.push(c));
                for c in near {
                    level.near_wall[c] = true;
                }
            } else {
                level.for_near(&l, CELL, |c| cells.push(c));
                for c in cells {
                    level.cell_lines[c].push(i as u32);
                }
            }
            if l.back_sector >= 0 {
                let (f, b) = (l.front_sector as usize, l.back_sector as usize);
                if l.tag == 0
                    && let Some(k) = KeyColor::of_door(l.special)
                {
                    level.door[b] = Door::Locked(k);
                } else if MANUAL_DOORS.contains(&l.special) && level.door[b] == Door::No {
                    level.door[b] = Door::Manual;
                }
                level.lift_low[f] = level.lift_low[f].min(sectors[b].floor);
                level.lift_low[b] = level.lift_low[b].min(sectors[f].floor);
            }
            if l.tag != 0 && l.special != 0 {
                for (si, s) in sectors.iter().enumerate() {
                    if s.tag != l.tag {
                        continue;
                    }
                    if LIFTS.contains(&l.special) {
                        level.lift[si] = true;
                    }
                    if !NOT_OPENING.contains(&l.special) {
                        level.triggers[si].push(i as u32);
                    }
                }
            }
        }
        level.build_stairs(&sectors);
        level.build_teleports(engine, &sectors);
        level
    }

    /// Mirror EV_BuildStairs: from each tagged sector, follow two-sided lines
    /// whose front is the current step to a back sector with the same floor
    /// texture. Every step in the chain is moved by the trigger.
    fn build_stairs(&mut self, sectors: &[Sector]) {
        for li in 0..self.lines.len() {
            let trigger = self.lines[li];
            if !STAIRS.contains(&trigger.special) || trigger.tag == 0 {
                continue;
            }
            for first in (0..sectors.len()).filter(|&s| sectors[s].tag == trigger.tag) {
                let texture = sectors[first].floorpic;
                let mut chain = vec![first];
                let mut cur = first;
                while let Some(next) = self.lines.iter().find_map(|l| {
                    let b = l.back_sector;
                    (l.front_sector as usize == cur
                        && b >= 0
                        && sectors[b as usize].floorpic == texture
                        && !chain.contains(&(b as usize)))
                    .then_some(b as usize)
                }) {
                    chain.push(next);
                    cur = next;
                }
                for s in chain {
                    if !self.triggers[s].contains(&(li as u32)) {
                        self.triggers[s].push(li as u32);
                    }
                }
            }
        }
    }

    fn build_teleports(&mut self, engine: &Engine, sectors: &[Sector]) {
        let dests: Vec<(f64, f64, usize)> = engine
            .things_snapshot()
            .iter()
            .filter(|t| t.type_ == MT_TELEPORTMAN)
            .filter_map(|t| engine.point_sector(t.x, t.y).map(|s| (t.x, t.y, s)))
            .collect();
        for (li, l) in self.lines.iter().enumerate() {
            if !TELEPORTS.contains(&l.special) || l.tag == 0 {
                continue;
            }
            let Some(&(dx, dy, _)) = dests.iter().find(|(_, _, s)| sectors[*s].tag == l.tag) else {
                continue;
            };
            let Some(dest) = self.cell_of(dx, dy).filter(|&c| !self.wall[c]) else {
                continue;
            };
            let mut pads = Vec::new();
            self.for_near(l, CELL, |c| pads.push(c));
            for c in pads {
                if !self.wall[c] && on_front(self.center(c), l) {
                    self.teleports.push((c, li as u32, dest));
                }
            }
        }
    }

    fn for_near(&self, l: &Line, r: f64, mut f: impl FnMut(usize)) {
        let x0 = ((l.x1.min(l.x2) - r - self.ox) / CELL).floor().max(0.0) as usize;
        let y0 = ((l.y1.min(l.y2) - r - self.oy) / CELL).floor().max(0.0) as usize;
        let x1 = (((l.x1.max(l.x2) + r - self.ox) / CELL).ceil() as usize).min(self.w - 1);
        let y1 = (((l.y1.max(l.y2) + r - self.oy) / CELL).ceil() as usize).min(self.h - 1);
        for cy in y0..=y1 {
            for cx in x0..=x1 {
                let c = cy * self.w + cx;
                let (px, py) = self.center(c);
                if seg_dist(px, py, l.x1, l.y1, l.x2, l.y2) <= r {
                    f(c);
                }
            }
        }
    }

    fn center(&self, c: usize) -> (f64, f64) {
        (
            self.ox + ((c % self.w) as f64 + 0.5) * CELL,
            self.oy + ((c / self.w) as f64 + 0.5) * CELL,
        )
    }

    fn cell_of(&self, x: f64, y: f64) -> Option<usize> {
        let cx = ((x - self.ox) / CELL).floor();
        let cy = ((y - self.oy) / CELL).floor();
        (cx >= 0.0 && cy >= 0.0 && (cx as usize) < self.w && (cy as usize) < self.h)
            .then(|| cy as usize * self.w + cx as usize)
    }

    fn dynamic(&self, engine: &mut Engine, s: &State) -> Dynamic {
        let mut blocked = self.wall.clone();
        for t in engine.things(1e9) {
            let (_, category) = thing_type(t.type_);
            // Monsters move; only static solid things are obstacles.
            if t.flags & MF_SOLID == 0 || t.flags & MF_COUNTKILL != 0 || category == "player" {
                continue;
            }
            let point = Line {
                x1: t.x,
                y1: t.y,
                x2: t.x,
                y2: t.y,
                ..Default::default()
            };
            self.for_near(&point, t.radius + CLEARANCE, |c| blocked[c] = true);
        }
        Dynamic {
            sectors: engine.sectors(),
            specials: engine.lines().iter().map(|l| l.special).collect(),
            blocked,
            keys: [KeyColor::Blue, KeyColor::Yellow, KeyColor::Red].map(|k| k.held(s)),
        }
    }

    /// Unused trigger lines that move sector `s`.
    fn live_triggers<'a>(&'a self, d: &'a Dynamic, s: usize) -> impl Iterator<Item = u32> + 'a {
        self.triggers[s]
            .iter()
            .copied()
            .filter(|&li| d.specials[li as usize] != 0)
    }

    /// Can the player move from sector `from` into sector `to`?
    fn enter(&self, d: &Dynamic, from: usize, to: usize, relax: Relax<'_>) -> Option<Cross> {
        let normal = self.enter_normally(d, from, to, relax);
        if normal.is_none() && relax.triggers {
            // Either side may be the one a trigger moves (a door that opens,
            // a floor that rises to meet a ledge...).
            for s in [to, from] {
                if !relax.forbid_sectors.contains(&s) && self.live_triggers(d, s).next().is_some() {
                    return Some(Cross::Trigger(s));
                }
            }
        }
        normal
    }

    fn enter_normally(
        &self,
        d: &Dynamic,
        from: usize,
        to: usize,
        relax: Relax<'_>,
    ) -> Option<Cross> {
        let (f, t) = (&d.sectors[from], &d.sectors[to]);
        let from_top = if self.lift[from] {
            f.floor.max(self.rest_floor[from])
        } else {
            f.floor
        };
        match self.door[to] {
            Door::Locked(k) if !d.keys[k.index()] && (!relax.locks || relax.forbid[k.index()]) => {
                return None;
            }
            Door::Manual | Door::Locked(_) => {
                if t.floor - from_top > MAX_STEP {
                    return None;
                }
                let open = t.ceiling - t.floor >= PLAYER_HEIGHT;
                return Some(if open || t.moving != 0 {
                    Cross::Plain
                } else {
                    Cross::Door(to)
                });
            }
            Door::No => {}
        }
        // Something is opening, lowering or carrying us right now; assume it'll
        // be passable once it stops.
        if t.moving != 0 || f.moving != 0 {
            return Some(Cross::Plain);
        }
        let to_low = if self.lift[to] {
            t.floor.min(self.lift_low[to])
        } else {
            t.floor
        };
        if to_low - from_top > MAX_STEP {
            return None;
        }
        let from_door = self.door[from] != Door::No;
        let ceiling = if from_door {
            t.ceiling
        } else {
            f.ceiling.min(t.ceiling)
        };
        let floor = to_low.max(if from_door { t.floor } else { f.floor });
        if ceiling - floor < PLAYER_HEIGHT && t.ceiling - t.floor < PLAYER_HEIGHT {
            return None;
        }
        if self.lift[to] && t.floor - from_top > MAX_STEP {
            return Some(Cross::Lift(to));
        }
        Some(Cross::Plain)
    }

    /// Check a move between neighbouring cells.
    fn edge(&self, d: &Dynamic, a: usize, b: usize, relax: Relax<'_>) -> Option<Cross> {
        if d.blocked[b] {
            return None;
        }
        let (pa, pb) = (self.center(a), self.center(b));
        let mut hits: Vec<(f64, u32)> = Vec::new();
        for &li in self.cell_lines[a].iter().chain(&self.cell_lines[b]) {
            if hits.iter().any(|&(_, x)| x == li) {
                continue;
            }
            if let Some(t) = crossing(pa, pb, &self.lines[li as usize]) {
                hits.push((t, li));
            }
        }
        hits.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut result = Cross::Plain;
        for (_, li) in hits {
            let l = &self.lines[li as usize];
            let front = on_front(pa, l);
            if front && TELEPORTS.contains(&d.specials[li as usize]) {
                return None; // handled as a teleport edge in search()
            }
            let (from, to) = if front {
                (l.front_sector as usize, l.back_sector as usize)
            } else {
                (l.back_sector as usize, l.front_sector as usize)
            };
            match self.enter(d, from, to, relax)? {
                Cross::Plain => {}
                // A trigger dependency outranks doors and lifts.
                other if !matches!(result, Cross::Trigger(_)) => result = other,
                _ => {}
            }
        }
        Some(result)
    }

    fn search(
        &self,
        d: &Dynamic,
        start: usize,
        goal_cells: &[usize],
        relax: Relax<'_>,
    ) -> Option<Vec<usize>> {
        let n = self.w * self.h;
        let mut is_goal = vec![false; n];
        for &g in goal_cells {
            is_goal[g] = true;
        }
        let goal_pts: Vec<(f64, f64)> = if goal_cells.len() <= 16 {
            goal_cells.iter().map(|&g| self.center(g)).collect()
        } else {
            Vec::new()
        };
        let heuristic = |c: usize| -> f32 {
            if goal_pts.is_empty() {
                return 0.0;
            }
            let (x, y) = self.center(c);
            goal_pts
                .iter()
                .map(|&(gx, gy)| (gx - x).hypot(gy - y))
                .fold(f64::MAX, f64::min) as f32
        };

        let mut dist = vec![f32::INFINITY; n];
        let mut prev = vec![u32::MAX; n];
        let mut heap = BinaryHeap::new();
        dist[start] = 0.0;
        heap.push(Reverse((heuristic(start).to_bits(), start as u32)));
        let w = self.w as isize;

        while let Some(Reverse((_, c))) = heap.pop() {
            let c = c as usize;
            for &(pad, li, dest) in &self.teleports {
                if pad == c && d.specials[li as usize] != 0 && !d.blocked[dest] {
                    let nd = dist[c] + 32.0;
                    if nd < dist[dest] {
                        dist[dest] = nd;
                        prev[dest] = c as u32;
                        heap.push(Reverse(((nd + heuristic(dest)).to_bits(), dest as u32)));
                    }
                }
            }
            if is_goal[c] {
                let mut path = vec![c];
                let mut cur = c;
                while prev[cur] != u32::MAX {
                    cur = prev[cur] as usize;
                    path.push(cur);
                }
                path.reverse();
                return Some(path);
            }
            let (cx, cy) = ((c % self.w) as isize, (c / self.w) as isize);
            for (dx, dy) in [
                (1, 0),
                (-1, 0),
                (0, 1),
                (0, -1),
                (1, 1),
                (1, -1),
                (-1, 1),
                (-1, -1),
            ] {
                let (nx, ny) = (cx + dx, cy + dy);
                if nx < 0 || ny < 0 || nx >= w || ny >= self.h as isize {
                    continue;
                }
                let nb = (ny * w + nx) as usize;
                if dx != 0 && dy != 0 {
                    // No cutting corners past blocked cells.
                    if d.blocked[(cy * w + nx) as usize] || d.blocked[(ny * w + cx) as usize] {
                        continue;
                    }
                }
                let Some(cross) = self.edge(d, c, nb, relax) else {
                    continue;
                };
                let mut cost = if dx != 0 && dy != 0 {
                    CELL * std::f64::consts::SQRT_2
                } else {
                    CELL
                };
                let s = self.cell_sector[nb];
                if s != u32::MAX && self.hazard[s as usize] {
                    cost *= 4.0;
                }
                if self.near_wall[nb] {
                    cost *= 1.6;
                }
                match cross {
                    Cross::Door(_) => cost += 64.0,
                    Cross::Lift(_) => cost += 256.0,
                    Cross::Trigger(_) => cost += 2048.0,
                    Cross::Plain => {}
                }
                let nd = dist[c] + cost as f32;
                if nd < dist[nb] {
                    dist[nb] = nd;
                    prev[nb] = c as u32;
                    heap.push(Reverse(((nd + heuristic(nb)).to_bits(), nb as u32)));
                }
            }
        }
        None
    }

    /// Nearest open cell to (x, y), within `radius` cells.
    fn nearest_open(&self, d: &Dynamic, x: f64, y: f64, radius: usize) -> Option<usize> {
        let c = self.cell_of(x, y)?;
        let (cx, cy) = ((c % self.w) as isize, (c / self.w) as isize);
        let r = radius as isize;
        let mut best: Option<(f64, usize)> = None;
        for dy in -r..=r {
            for dx in -r..=r {
                let (nx, ny) = (cx + dx, cy + dy);
                if nx < 0 || ny < 0 || nx >= self.w as isize || ny >= self.h as isize {
                    continue;
                }
                let nc = (ny * self.w as isize + nx) as usize;
                if d.blocked[nc] {
                    continue;
                }
                let (px, py) = self.center(nc);
                let dist = (px - x).hypot(py - y);
                if best.is_none_or(|(bd, _)| dist < bd) {
                    best = Some((dist, nc));
                }
            }
        }
        best.map(|(_, c)| c)
    }

    /// Open cells from which the player can reach target `t`: within use range
    /// (64 units) of it, and in one of its allowed sectors when it has any.
    fn goal_cells(&self, d: &Dynamic, t: &Target) -> Vec<usize> {
        if t.sectors.is_empty() {
            return self.nearest_open(d, t.x, t.y, 3).into_iter().collect();
        }
        let Some(c) = self.cell_of(t.x, t.y) else {
            return Vec::new();
        };
        let (cx, cy) = ((c % self.w) as isize, (c / self.w) as isize);
        let mut out = Vec::new();
        for dy in -4..=4isize {
            for dx in -4..=4isize {
                let (nx, ny) = (cx + dx, cy + dy);
                if nx < 0 || ny < 0 || nx >= self.w as isize || ny >= self.h as isize {
                    continue;
                }
                let nc = (ny * self.w as isize + nx) as usize;
                let (px, py) = self.center(nc);
                let reach = match t.face {
                    // Switches: within use range of the switch itself.
                    Some((fx, fy)) => (px - fx).hypot(py - fy) <= 60.0,
                    None => (px - t.x).hypot(py - t.y) <= 32.0,
                };
                if reach && !d.blocked[nc] && t.sectors.contains(&(self.cell_sector[nc] as usize)) {
                    out.push(nc);
                }
            }
        }
        out
    }

    /// Straight-line walk between two cells without doors, lifts or walls.
    fn walkable(&self, d: &Dynamic, a: usize, b: usize) -> bool {
        let (pa, pb) = (self.center(a), self.center(b));
        let len = (pb.0 - pa.0).hypot(pb.1 - pa.1);
        let steps = (len / 8.0).ceil().max(1.0) as usize;
        let mut prev = a;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let Some(c) = self.cell_of(pa.0 + (pb.0 - pa.0) * t, pa.1 + (pb.1 - pa.1) * t) else {
                return false;
            };
            if c == prev {
                continue;
            }
            if self.edge(d, prev, c, Relax::NONE) != Some(Cross::Plain) {
                return false;
            }
            prev = c;
        }
        true
    }

    /// Where to stand to activate line `l`, and what to say about it.
    fn line_target(&self, l: &Line, special: i32, what: &str) -> Target {
        let (mx, my) = ((l.x1 + l.x2) / 2.0, (l.y1 + l.y2) / 2.0);
        if WALK_SPECIALS.contains(&special) {
            let mut sectors = vec![l.front_sector as usize];
            if l.back_sector >= 0 {
                sectors.push(l.back_sector as usize);
            }
            return Target {
                x: mx,
                y: my,
                face: None,
                hint: Some(format!("Walk across this spot to trigger it: it {what}.")),
                sectors,
            };
        }
        let len = (l.x2 - l.x1).hypot(l.y2 - l.y1).max(1e-6);
        // Front side is to the right of v1 -> v2.
        let (nx, ny) = ((l.y2 - l.y1) / len, -(l.x2 - l.x1) / len);
        let hint = if GUN_SPECIALS.contains(&special) {
            format!("Shoot this wall (face it and fire): it {what}.")
        } else {
            format!("Press this switch (act use=true while facing it): it {what}.")
        };
        Target {
            x: mx + nx * SWITCH_STANDOFF,
            y: my + ny * SWITCH_STANDOFF,
            face: Some((mx, my)),
            hint: Some(hint),
            sectors: vec![l.front_sector as usize],
        }
    }

    fn targets(&self, engine: &mut Engine, d: &Dynamic, goal: &Goal) -> Vec<Target> {
        let mut out = Vec::new();
        let lines_with = |pred: &dyn Fn(usize, &Line) -> bool| -> Vec<usize> {
            self.lines
                .iter()
                .enumerate()
                .filter(|(i, l)| pred(*i, l))
                .map(|(i, _)| i)
                .collect()
        };
        match goal {
            Goal::Exit | Goal::SecretExit => {
                let (switch, walk) = if *goal == Goal::Exit {
                    (SWITCH_EXITS, WALK_EXITS)
                } else {
                    (SWITCH_SECRET_EXITS, WALK_SECRET_EXITS)
                };
                for i in lines_with(&|i, _| {
                    switch.contains(&d.specials[i]) || walk.contains(&d.specials[i])
                }) {
                    let mut t = self.line_target(&self.lines[i], d.specials[i], "ends the level");
                    t.hint = Some(if walk.contains(&d.specials[i]) {
                        "Walk across this line to finish the level.".into()
                    } else {
                        "This is the exit switch: act with use=true to finish the level.".into()
                    });
                    out.push(t);
                }
            }
            Goal::Switch => {
                for i in lines_with(&|i, l| {
                    let sp = d.specials[i];
                    l.two_sided == 0
                        && sp != 0
                        && !WALK_SPECIALS.contains(&sp)
                        && !SWITCH_EXITS.contains(&sp)
                        && !SWITCH_SECRET_EXITS.contains(&sp)
                }) {
                    out.push(self.line_target(
                        &self.lines[i],
                        d.specials[i],
                        "does something on this level",
                    ));
                }
            }
            Goal::Trigger(sector) => {
                for li in self.live_triggers(d, *sector) {
                    let l = &self.lines[li as usize];
                    out.push(self.line_target(l, d.specials[li as usize], "opens the way forward"));
                }
            }
            Goal::Key(color) => {
                for t in engine.things(1e9) {
                    let (name, category) = thing_type(t.type_);
                    if category == "key" && KeyColor::of_thing(name) == Some(*color) {
                        out.push(Target {
                            x: t.x,
                            y: t.y,
                            face: None,
                            hint: Some(format!("Walk over the {name} to pick it up.")),
                            sectors: engine.point_sector(t.x, t.y).into_iter().collect(),
                        });
                    }
                }
            }
            Goal::Thing(id) => {
                for t in engine.things(1e9) {
                    if engine.thing_id(t.id) == *id {
                        out.push(Target {
                            x: t.x,
                            y: t.y,
                            face: None,
                            hint: None,
                            sectors: Vec::new(),
                        });
                    }
                }
            }
            Goal::Point(x, y) => out.push(Target {
                x: *x,
                y: *y,
                face: None,
                hint: None,
                sectors: Vec::new(),
            }),
            Goal::Explore => {}
        }
        out
    }

    pub fn route(&self, engine: &mut Engine, goal: &Goal) -> Result<Route, String> {
        let s = engine.state();
        let d = self.dynamic(engine, &s);
        let mut chain = Chain::default();
        if let Goal::Key(k) = goal {
            chain.forbid[k.index()] = true;
        }
        self.route_with(engine, &d, &s, goal, &chain)
    }

    fn route_with(
        &self,
        engine: &mut Engine,
        d: &Dynamic,
        s: &State,
        goal: &Goal,
        chain: &Chain,
    ) -> Result<Route, String> {
        let start = self.nearest_open(d, s.x, s.y, 3).ok_or(
            "You're somewhere the route planner can't place you; move a little and try again.",
        )?;

        let targets = self.targets(engine, d, goal);
        let mut goal_cells = Vec::new();
        let mut goal_target = Vec::new();
        if *goal == Goal::Explore {
            for (c, lines) in self.cell_lines.iter().enumerate() {
                if !d.blocked[c]
                    && lines
                        .iter()
                        .any(|&li| self.lines[li as usize].flags & ML_MAPPED == 0)
                {
                    goal_cells.push(c);
                    goal_target.push(usize::MAX);
                }
            }
            if goal_cells.is_empty() {
                return Err("Everything reachable has been seen already.".into());
            }
        } else {
            if targets.is_empty() {
                return Err(match goal {
                    Goal::Exit => {
                        "This level has no exit switch or exit line. On boss levels the way \
                                   out opens once the bosses are dead."
                            .into()
                    }
                    _ => format!(
                        "Couldn't find {} on this level (it may have been used already).",
                        goal.describe()
                    ),
                });
            }
            for (i, t) in targets.iter().enumerate() {
                let cells = self.goal_cells(d, t);
                goal_target.extend(std::iter::repeat_n(i, cells.len()));
                goal_cells.extend(cells);
            }
        }

        if let Some(path) = self.search(d, start, &goal_cells, Relax::NONE) {
            let end = *path.last().unwrap();
            let target = goal_cells
                .iter()
                .position(|&c| c == end)
                .and_then(|i| targets.get(goal_target[i]));
            return Ok(self.waypoints(d, s, &path, target, goal));
        }

        // Unreachable as things stand: find the first obstacle on the most
        // plausible path and route to whatever removes it.
        let fallback = || {
            format!(
                "No known route to {}. It may need a lift, a teleporter, or a trigger the planner \
                 doesn't understand. Try goal \"switch\" or \"explore\".",
                goal.describe()
            )
        };
        if chain.depth >= MAX_CHAIN {
            return Err(fallback());
        }
        let relax = Relax {
            locks: true,
            triggers: true,
            forbid: chain.forbid,
            forbid_sectors: &chain.forbid_sectors,
        };
        let Some(path) = self.search(d, start, &goal_cells, relax) else {
            if std::env::var_os("DOOM_MCP_NAV_DEBUG").is_some() {
                eprintln!(
                    "relaxed search for {} failed; {} goal cells",
                    goal.describe(),
                    goal_cells.len()
                );
            }
            return Err(fallback());
        };
        for w in path.windows(2) {
            match self.edge(d, w[0], w[1], relax) {
                Some(Cross::Door(sec)) => {
                    let Door::Locked(k) = self.door[sec] else {
                        continue;
                    };
                    if d.keys[k.index()] {
                        continue;
                    }
                    let mut next = chain.clone();
                    next.depth += 1;
                    next.forbid[k.index()] = true;
                    let mut sub = self
                        .route_with(engine, d, s, &Goal::Key(k), &next)
                        .map_err(|e| {
                            format!(
                                "{} is behind a locked {} door, and: {e}",
                                capitalise(&goal.describe()),
                                k.name()
                            )
                        })?;
                    sub.prerequisite = Some(join_reasons(
                        format!(
                            "{} is behind a locked {} door, so this route goes to the {} key first.",
                            capitalise(&goal.describe()),
                            k.name(),
                            k.name()
                        ),
                        sub.prerequisite.take(),
                    ));
                    return Ok(sub);
                }
                Some(Cross::Trigger(sec)) => {
                    let mut next = chain.clone();
                    next.depth += 1;
                    next.forbid_sectors.push(sec);
                    let mut sub = self
                        .route_with(engine, d, s, &Goal::Trigger(sec), &next)
                        .map_err(|e| {
                        format!(
                            "The way to {} is opened by a switch or trigger elsewhere, and: {e}",
                            goal.describe()
                        )
                    })?;
                    sub.prerequisite = Some(join_reasons(
                        format!(
                            "The way to {} is closed until something is triggered (a switch or a \
                             walk-over line), so this route goes there first.",
                            goal.describe()
                        ),
                        sub.prerequisite.take(),
                    ));
                    return Ok(sub);
                }
                _ => {}
            }
        }
        Err(fallback())
    }

    fn waypoints(
        &self,
        d: &Dynamic,
        s: &State,
        path: &[usize],
        target: Option<&Target>,
        goal: &Goal,
    ) -> Route {
        // Doors, lifts and teleporters are anchors: you must stop at them.
        // (anchor index, kind, index to resume string-pulling from)
        let mut anchors: Vec<(usize, WaypointKind)> = Vec::new();
        let mut resume_after_teleport = Vec::new();
        for i in 1..path.len() {
            let ahead = self.center(path[(i + 2).min(path.len() - 1)]);
            let (a, b) = (path[i - 1], path[i]);
            let adjacent =
                (a % self.w).abs_diff(b % self.w) <= 1 && (a / self.w).abs_diff(b / self.w) <= 1;
            if !adjacent {
                // A teleport jump: aim just past the teleporter line.
                if let Some(&(_, li, _)) = self
                    .teleports
                    .iter()
                    .find(|&&(pad, _, dest)| pad == a && dest == b)
                {
                    let l = &self.lines[li as usize];
                    let (mx, my) = ((l.x1 + l.x2) / 2.0, (l.y1 + l.y2) / 2.0);
                    let len = (l.x2 - l.x1).hypot(l.y2 - l.y1).max(1e-6);
                    let (nx, ny) = ((l.y2 - l.y1) / len, -(l.x2 - l.x1) / len);
                    let (dx, dy) = self.center(b);
                    anchors.push((i - 1, WaypointKind::Teleport { dx, dy }));
                    resume_after_teleport.push((i - 1, (mx - nx * 16.0, my - ny * 16.0)));
                }
                continue;
            }
            match self.edge(d, a, b, Relax::NONE) {
                Some(Cross::Door(sector)) => anchors.push((
                    i - 1,
                    WaypointKind::Door {
                        sector,
                        fx: ahead.0,
                        fy: ahead.1,
                    },
                )),
                Some(Cross::Lift(sector)) => anchors.push((
                    i - 1,
                    WaypointKind::Lift {
                        sector,
                        fx: ahead.0,
                        fy: ahead.1,
                    },
                )),
                _ => {}
            }
        }
        let last = path.len() - 1;
        let (fx, fy, face) = match target.and_then(|t| t.face) {
            Some((fx, fy)) => (fx, fy, true),
            None => {
                let (x, y) = self.center(path[last]);
                (x, y, false)
            }
        };
        anchors.push((last, WaypointKind::Goal { fx, fy, face }));

        let mut waypoints = Vec::new();
        let mut from = 0;
        for (anchor, kind) in anchors {
            if let WaypointKind::Teleport { .. } = kind {
                // Walk to the pad, then across the line; resume from the destination.
                let mut i = from;
                while i < anchor {
                    let mut j = anchor;
                    while j > i + 1 && !self.walkable(d, path[i], path[j]) {
                        j -= 1;
                    }
                    let (x, y) = self.center(path[j]);
                    waypoints.push(Waypoint {
                        x,
                        y,
                        kind: WaypointKind::Walk,
                    });
                    i = j;
                }
                let &(_, (px, py)) = resume_after_teleport
                    .iter()
                    .find(|(a, _)| *a == anchor)
                    .unwrap();
                waypoints.push(Waypoint { x: px, y: py, kind });
                from = anchor + 1;
                continue;
            }
            // Greedy string-pulling: jump to the furthest directly walkable cell.
            let mut i = from;
            while i < anchor {
                let mut j = anchor;
                while j > i + 1 && !self.walkable(d, path[i], path[j]) {
                    j -= 1;
                }
                if j < anchor {
                    let (x, y) = self.center(path[j]);
                    waypoints.push(Waypoint {
                        x,
                        y,
                        kind: WaypointKind::Walk,
                    });
                }
                i = j;
            }
            let (x, y) = self.center(path[anchor]);
            if anchor == last
                && let Some(t) = target
                && t.face.is_none()
            {
                // Walk onto the target itself (keys, walk-over triggers).
                waypoints.push(Waypoint {
                    x: t.x,
                    y: t.y,
                    kind,
                });
            } else {
                waypoints.push(Waypoint { x, y, kind });
            }
            from = anchor;
        }
        if waypoints.len() > 1 {
            // Skip a first waypoint we're already standing on.
            let first = waypoints[0];
            if first.kind == WaypointKind::Walk && (first.x - s.x).hypot(first.y - s.y) < 16.0 {
                waypoints.remove(0);
            }
        }

        let mut length = 0.0;
        let mut prev = (s.x, s.y);
        for w in &waypoints {
            length += (w.x - prev.0).hypot(w.y - prev.1);
            prev = match w.kind {
                WaypointKind::Teleport { dx, dy } => (dx, dy),
                _ => (w.x, w.y),
            };
        }
        let crosses_hazard = path.iter().any(|&c| {
            let sec = self.cell_sector[c];
            sec != u32::MAX && self.hazard[sec as usize]
        });
        Route {
            goal: goal.describe(),
            prerequisite: None,
            arrival_hint: target.and_then(|t| t.hint.clone()),
            waypoints,
            length,
            crosses_hazard,
        }
    }

    /// Debugging aid: edges between "reachable from the player" and "can reach
    /// one of `goals`" that the planner refuses, i.e. the actual cut.
    pub fn debug_cut(&self, engine: &mut Engine, goal: &Goal) -> String {
        let s = engine.state();
        let d = self.dynamic(engine, &s);
        let all = Relax {
            locks: true,
            triggers: true,
            forbid: [false; 3],
            forbid_sectors: &[],
        };
        let n = self.w * self.h;
        let nbrs = |c: usize| {
            let (cx, cy) = ((c % self.w) as isize, (c / self.w) as isize);
            [(1, 0), (-1, 0), (0, 1), (0, -1)].into_iter().filter_map(
                move |(dx, dy): (isize, isize)| {
                    let (nx, ny) = (cx + dx, cy + dy);
                    (nx >= 0 && ny >= 0 && nx < self.w as isize && ny < self.h as isize)
                        .then(|| (ny * self.w as isize + nx) as usize)
                },
            )
        };
        let flood = |starts: Vec<usize>, forward: bool| {
            let mut seen = vec![false; n];
            let mut stack = starts;
            for &c in &stack {
                seen[c] = true;
            }
            while let Some(c) = stack.pop() {
                for nb in nbrs(c) {
                    if seen[nb] || d.blocked[nb] {
                        continue;
                    }
                    let ok = if forward {
                        self.edge(&d, c, nb, all)
                    } else {
                        self.edge(&d, nb, c, all)
                    };
                    if ok.is_some() {
                        seen[nb] = true;
                        stack.push(nb);
                    }
                }
            }
            seen
        };
        let Some(start) = self.nearest_open(&d, s.x, s.y, 3) else {
            return "no start".into();
        };
        let fwd = flood(vec![start], true);
        let goals: Vec<usize> = self
            .targets(engine, &d, goal)
            .iter()
            .flat_map(|t| self.goal_cells(&d, t))
            .collect();
        let back = flood(goals, false);
        let mut out = format!(
            "forward {} cells, backward {} cells, overlap {}\n",
            fwd.iter().filter(|&&x| x).count(),
            back.iter().filter(|&&x| x).count(),
            (0..n).filter(|&c| fwd[c] && back[c]).count()
        );
        let mut seen_lines = std::collections::BTreeSet::new();
        for c in (0..n).filter(|&c| fwd[c]) {
            for nb in nbrs(c) {
                if back[nb] && !fwd[nb] {
                    for &li in self.cell_lines[c].iter().chain(&self.cell_lines[nb]) {
                        if crossing(self.center(c), self.center(nb), &self.lines[li as usize])
                            .is_some()
                            && seen_lines.insert(li)
                        {
                            let l = &self.lines[li as usize];
                            let (fs, bs) = (l.front_sector as usize, l.back_sector as usize);
                            out += &format!(
                                "cut line {li} ({:.0},{:.0})-({:.0},{:.0}) sp {} tag {} | s{fs} f{:.0} c{:.0} lift {} | s{bs} f{:.0} c{:.0} lift {} low {:.0}\n",
                                l.x1,
                                l.y1,
                                l.x2,
                                l.y2,
                                l.special,
                                l.tag,
                                d.sectors[fs].floor,
                                d.sectors[fs].ceiling,
                                self.lift[fs],
                                d.sectors[bs].floor,
                                d.sectors[bs].ceiling,
                                self.lift[bs],
                                self.lift_low[bs],
                            );
                        }
                    }
                }
            }
        }
        out
    }

    /// Key colours of the locked doors on this level.
    pub fn locked_doors(&self) -> Vec<KeyColor> {
        let mut out = Vec::new();
        for d in &self.door {
            if let Door::Locked(k) = d
                && !out.contains(k)
            {
                out.push(*k);
            }
        }
        out
    }
}

fn join_reasons(reason: String, inner: Option<String>) -> String {
    match inner {
        Some(inner) => format!("{reason} {inner}"),
        None => reason,
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Bearing from the player to (x, y): positive = right of the crosshair.
pub fn bearing_to(s: &State, x: f64, y: f64) -> f64 {
    let world = (y - s.y).atan2(x - s.x).to_degrees();
    let mut b = s.angle - world;
    while b > 180.0 {
        b -= 360.0;
    }
    while b <= -180.0 {
        b += 360.0;
    }
    b
}

/// Where the route to the current goal goes next, as shown to the model.
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

/// Caches the level's navigation grid.
#[derive(Default)]
pub struct Navigator {
    level: Option<Level>,
}

impl Navigator {
    pub fn level(&mut self, engine: &Engine) -> Option<&Level> {
        let s = engine.state();
        if s.in_level == 0 || s.demoplayback != 0 {
            return None;
        }
        let id = (s.episode, s.map);
        if self.level.as_ref().is_none_or(|l| l.id != id) {
            self.level = Some(Level::build(engine, id));
        }
        self.level.as_ref()
    }

    pub fn route(&mut self, engine: &mut Engine, goal: &Goal) -> Result<Route, String> {
        match self.level(engine) {
            Some(level) => {
                let r = level.route(engine, goal);
                if r.is_err() && std::env::var_os("DOOM_MCP_NAV_DEBUG").is_some() {
                    eprintln!("CUT: {}", level.debug_cut(engine, goal));
                }
                r
            }
            None => Err("Not in a level.".into()),
        }
    }
}
