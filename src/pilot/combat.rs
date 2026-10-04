//! Fighting, one tic at a time: choose a target, pick a weapon, track the
//! target and shoot when it's in the crosshair, and keep moving (strafe,
//! back off from melee monsters) so as not to be an easy target.

use crate::engine::ffi::{MF_COUNTKILL, State, Thing};
use crate::engine::{Controls, Dir, Engine};
use crate::world::observe::{WEAPON_NAMES, thing_type};

use super::{Orders, Stance};

/// Only monsters this close are considered.
const SIGHT_RANGE: f64 = 2048.0;
/// Balanced stance: also engage monsters this close that haven't noticed us.
const BALANCED_RANGE: f64 = 1024.0;
/// Change strafe direction this often (tics).
const STRAFE_PERIOD: u32 = 30;
/// Ticks between weapon switches, so we don't flip-flop.
const WEAPON_COOLDOWN: u32 = 35;
/// Keep backing off from melee monsters closer than this.
const MELEE_DANGER: f64 = 160.0;
/// Too close for rockets: the blast would hurt us.
const ROCKET_MIN_RANGE: f64 = 256.0;
/// weapontype_t value meaning "no weapon change pending".
const WP_NOCHANGE: i32 = 10;

/// Monsters that only hurt up close: keep them at a distance.
const MELEE_MONSTERS: &[&str] = &["Demon", "Spectre", "Lost Soul"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// Engine pointer (stable while the monster exists) and type.
    ptr: i64,
    kind: i32,
    pub id: u32,
    pub name: &'static str,
}

/// What combat reports besides its controls.
#[derive(Debug, Clone, PartialEq)]
pub enum CombatEvent {
    Engaging {
        id: u32,
        name: &'static str,
        distance: i32,
    },
    SwitchedWeapon(&'static str),
    OutOfAmmo,
}

#[derive(Default)]
pub struct Combat {
    target: Option<Target>,
    /// The last target announced, so losing and regaining sight of the same
    /// monster doesn't announce it again.
    announced: Option<i64>,
    strafe: Dir,
    strafe_timer: u32,
    weapon_cooldown: u32,
    out_of_ammo_reported: bool,
}

impl Combat {
    pub fn target(&self) -> Option<Target> {
        self.target
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Fight if there's something to fight; `None` means nothing to engage.
    pub fn tick(
        &mut self,
        engine: &mut Engine,
        orders: &Orders,
        s: &State,
    ) -> Option<(Controls, Vec<CombatEvent>)> {
        self.weapon_cooldown = self.weapon_cooldown.saturating_sub(1);
        if orders.stance == Stance::HoldFire {
            self.target = None;
            return None;
        }
        let things = engine.things(SIGHT_RANGE);
        let candidates: Vec<&Thing> = things
            .iter()
            .filter(|t| {
                let (_, category) = thing_type(t.type_);
                category == "monster"
                    && t.flags & MF_COUNTKILL != 0
                    && t.health > 0
                    && t.line_of_sight != 0
                    && match orders.stance {
                        Stance::Aggressive => true,
                        Stance::Balanced => t.targeting_player != 0 || t.distance < BALANCED_RANGE,
                        Stance::Cautious => t.targeting_player != 0,
                        Stance::HoldFire => false,
                    }
            })
            .collect();

        // Focus target first, then stick with the current one, then the most
        // threatening (aware of us, then closest).
        let pick = orders
            .focus
            .and_then(|id| candidates.iter().find(|t| engine.thing_id(t.id) == id))
            .or_else(|| {
                self.target.and_then(|cur| {
                    candidates
                        .iter()
                        .find(|t| t.id == cur.ptr && t.type_ == cur.kind)
                })
            })
            .or_else(|| {
                candidates.iter().min_by(|a, b| {
                    (a.targeting_player == 0)
                        .cmp(&(b.targeting_player == 0))
                        .then(a.distance.total_cmp(&b.distance))
                })
            })
            .copied()
            .copied();
        let Some(t) = pick else {
            self.target = None;
            return None;
        };

        let mut events = Vec::new();
        let target = Target {
            ptr: t.id,
            kind: t.type_,
            id: engine.thing_id(t.id),
            name: thing_type(t.type_).0,
        };
        if self.announced != Some(t.id) {
            self.announced = Some(t.id);
            events.push(CombatEvent::Engaging {
                id: target.id,
                name: target.name,
                distance: t.distance.round() as i32,
            });
        }
        self.target = Some(target);

        let mut c = Controls {
            run: true,
            turn: t.bearing,
            ..Default::default()
        };

        // Weapon.
        let wanted = choose_weapon(s, t.distance, orders.weapon);
        let switching = s.pendingweapon != WP_NOCHANGE;
        if slot_of(wanted) != slot_of(s.readyweapon as usize)
            && !switching
            && self.weapon_cooldown == 0
        {
            c.weapon = Some(slot_of(wanted));
            self.weapon_cooldown = WEAPON_COOLDOWN;
            events.push(CombatEvent::SwitchedWeapon(WEAPON_NAMES[wanted]));
        }
        let melee_only = matches!(s.readyweapon, 0 | 7);
        if melee_only && wanted == 0 && !self.out_of_ammo_reported {
            self.out_of_ammo_reported = true;
            events.push(CombatEvent::OutOfAmmo);
        } else if !melee_only {
            self.out_of_ammo_reported = false;
        }

        // Fire once the crosshair is on it: within the monster's angular size
        // (a ~20-unit radius), at least 2.5 degrees.
        let tolerance = (20.0 / t.distance.max(1.0))
            .atan()
            .to_degrees()
            .clamp(2.5, 10.0);
        let in_reach = !melee_only || t.distance < 80.0;
        c.fire = t.bearing.abs() <= tolerance && in_reach && !switching;

        // Movement: strafe from side to side; keep melee monsters and our own
        // rockets' blast at a distance; close in when we can only punch.
        self.strafe_timer += 1;
        if self.strafe == Dir::None || self.strafe_timer >= STRAFE_PERIOD {
            self.strafe = if self.strafe == Dir::Pos {
                Dir::Neg
            } else {
                Dir::Pos
            };
            self.strafe_timer = 0;
        }
        c.strafe = self.strafe;
        let melee_monster = MELEE_MONSTERS.contains(&target.name);
        if melee_only {
            c.movement = Dir::Pos;
            c.strafe = Dir::None;
        } else if (melee_monster && t.distance < MELEE_DANGER)
            || (s.readyweapon == 4 && t.distance < ROCKET_MIN_RANGE)
            || (orders.stance == Stance::Cautious && t.distance < 320.0)
        {
            c.movement = Dir::Neg;
        } else if orders.stance == Stance::Aggressive && t.distance > 512.0 {
            c.movement = Dir::Pos;
        }
        Some((c, events))
    }
}

/// Number key that selects weapon index `i` (fist/chainsaw share 1, shotguns share 3).
pub fn slot_of(weapon: usize) -> u8 {
    [1, 2, 3, 4, 5, 6, 7, 1, 3]
        .get(weapon)
        .copied()
        .unwrap_or(1)
}

/// Best weapon (index into WEAPON_NAMES) for a target at `distance`.
fn choose_weapon(s: &State, distance: f64, preferred_slot: Option<u8>) -> usize {
    let owned = |w: usize| s.weaponowned[w] != 0;
    let (bullets, shells, cells, rockets) = (s.ammo[0], s.ammo[1], s.ammo[2], s.ammo[3]);
    let usable = |w: usize| {
        owned(w)
            && match w {
                1 | 3 => bullets > 0,
                2 => shells > 0,
                8 => shells > 1,
                4 => rockets > 0 && distance > ROCKET_MIN_RANGE,
                5 => cells > 0,
                6 => cells >= 40,
                _ => true,
            }
    };
    if let Some(slot) = preferred_slot
        && let Some(w) = (0..9).filter(|&w| slot_of(w) == slot).find(|&w| usable(w))
    {
        return w;
    }
    // Automatic choice, best first. Never the BFG unless asked for.
    let order: &[(usize, f64)] = &[
        (5, f64::MAX), // plasma
        (8, 500.0),    // super shotgun, up close
        (2, 600.0),    // shotgun, mid range
        (3, f64::MAX), // chaingun
        (2, f64::MAX), // shotgun at any range
        (4, f64::MAX), // rockets (only beyond blast range, see `usable`)
        (1, f64::MAX), // pistol
        (7, f64::MAX), // chainsaw
    ];
    order
        .iter()
        .find(|&&(w, max)| distance <= max && usable(w))
        .map(|&(w, _)| w)
        .unwrap_or(0)
}
