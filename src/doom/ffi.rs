//! Bindings to csrc/dmcp.c. Struct layouts mirror csrc/dmcp.h.

use std::ffi::{c_char, c_int};

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Keys {
    pub up: c_int,
    pub down: c_int,
    pub left: c_int,
    pub right: c_int,
    pub strafeleft: c_int,
    pub straferight: c_int,
    pub fire: c_int,
    pub use_: c_int,
    pub speed: c_int,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct State {
    pub gamemode: c_int,
    pub gamestate: c_int,
    pub menuactive: c_int,
    pub paused: c_int,
    pub demoplayback: c_int,
    pub automapactive: c_int,
    pub episode: c_int,
    pub map: c_int,
    pub skill: c_int,
    pub gametic: c_int,
    pub leveltime: c_int,
    pub totalkills: c_int,
    pub totalitems: c_int,
    pub totalsecrets: c_int,

    pub in_level: c_int,
    pub playerstate: c_int,
    pub health: c_int,
    pub armor: c_int,
    pub armortype: c_int,
    pub readyweapon: c_int,
    pub pendingweapon: c_int,
    pub weaponowned: [c_int; 9],
    pub ammo: [c_int; 4],
    pub maxammo: [c_int; 4],
    pub cards: [c_int; 6],
    pub powers: [c_int; 6],
    pub backpack: c_int,
    pub kills: c_int,
    pub items: c_int,
    pub secrets: c_int,
    pub damagecount: c_int,
    pub bonuscount: c_int,
    pub has_attacker: c_int,
    pub attacker_type: c_int,
    pub attacker_bearing: f64,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub angle: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Thing {
    pub id: i64,
    pub type_: c_int,
    pub flags: c_int,
    pub health: c_int,
    pub line_of_sight: c_int,
    pub targeting_player: c_int,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub distance: f64,
    pub bearing: f64,
    pub radius: f64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Line {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub flags: c_int,
    pub special: c_int,
    pub two_sided: c_int,
    pub front_floor: f64,
    pub front_ceiling: f64,
    pub back_floor: f64,
    pub back_ceiling: f64,
    pub front_sector: c_int,
    /// -1 when one-sided.
    pub back_sector: c_int,
    pub tag: c_int,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Sector {
    pub floor: f64,
    pub ceiling: f64,
    pub special: c_int,
    pub tag: c_int,
    /// A door, floor, lift or ceiling is currently moving in this sector.
    pub moving: c_int,
    pub floorpic: c_int,
}

// mobj_t flags (p_mobj.h)
pub const MF_SOLID: c_int = 0x2;
pub const MF_COUNTKILL: c_int = 0x400000;

// line_t flags (doomdata.h)
pub const ML_BLOCKING: c_int = 1;
pub const ML_SECRET: c_int = 32;
pub const ML_MAPPED: c_int = 256;

pub const SCREEN_W: usize = 320;
pub const SCREEN_H: usize = 200;

unsafe extern "C" {
    pub fn dmcp_create(argc: c_int, argv: *mut *mut c_char) -> c_int;
    pub fn dmcp_exit_status() -> c_int;
    pub fn dmcp_run_tic() -> c_int;
    pub fn dmcp_take_message(out: *mut c_char, len: c_int) -> c_int;
    pub fn dmcp_key(down: c_int, key: c_int);
    pub fn dmcp_turn(units: c_int);
    pub fn dmcp_keys(out: *mut Keys);
    pub fn dmcp_new_game(skill: c_int, episode: c_int, map: c_int) -> c_int;
    pub fn dmcp_state(out: *mut State);
    pub fn dmcp_things(out: *mut Thing, max: c_int, radius: f64) -> c_int;
    pub fn dmcp_num_lines() -> c_int;
    pub fn dmcp_lines(out: *mut Line, max: c_int) -> c_int;
    pub fn dmcp_framebuffer() -> *const u32;
    pub fn dmcp_num_sectors() -> c_int;
    pub fn dmcp_sectors(out: *mut Sector, max: c_int) -> c_int;
    pub fn dmcp_point_sector(x: f64, y: f64) -> c_int;
    pub fn dmcp_set_frame_callback(cb: extern "C" fn(*const u32));
}
