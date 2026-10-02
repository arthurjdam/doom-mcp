//! Safe-ish wrapper around the engine. An `Engine` must only ever be used
//! from the thread that created it: doomgeneric is one big pile of globals.

use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int};

use anyhow::{Result, bail};

use super::ffi::{self, Keys, Line, SCREEN_H, SCREEN_W, Sector, State, Thing};

/// One full turn in "mouse units" (see `dmcp_turn`): 65536 angleturn / 8.
const UNITS_PER_TURN: f64 = 8192.0;
/// Cap per tic so the engine's 16-bit angleturn never overflows.
const MAX_UNITS_PER_TIC: i32 = 2048;

pub const TICRATE: u32 = 35;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Dir {
    #[default]
    None,
    Pos,
    Neg,
}

#[derive(Debug, Clone, Default)]
pub struct Input {
    /// Pos = forward, Neg = backward.
    pub movement: Dir,
    /// Pos = right, Neg = left.
    pub strafe: Dir,
    /// Positive turns right (clockwise), negative turns left.
    pub turn_degrees: f64,
    pub fire: bool,
    pub use_: bool,
    pub run: bool,
    /// Weapon slot key 1-7.
    pub weapon: Option<u8>,
    pub tics: u32,
}

pub struct Engine {
    keys: Keys,
    /// Keys held during the previous step; released (queued) at its end.
    last_held: Vec<c_int>,
    messages: Vec<String>,
    exited: bool,
    /// Small stable ids for map objects, keyed by their engine pointer.
    /// Reset whenever a level is (re)loaded.
    ids: HashMap<i64, u32>,
    next_id: u32,
    ids_level: (i32, i32, i32),
}

impl Engine {
    pub fn start(args: &[String]) -> Result<Self> {
        // Doom keeps pointers into argv for its whole life, so leak them.
        let mut argv: Vec<*mut c_char> = args
            .iter()
            .map(|a| CString::new(a.as_str()).unwrap().into_raw())
            .collect();
        argv.push(std::ptr::null_mut());
        let argv = Box::leak(argv.into_boxed_slice());

        if unsafe { ffi::dmcp_create(args.len() as c_int, argv.as_mut_ptr()) } != 0 {
            bail!(
                "Doom failed to start (exit status {}); see stderr for the engine's error",
                unsafe { ffi::dmcp_exit_status() }
            );
        }
        let mut keys = Keys::default();
        unsafe { ffi::dmcp_keys(&mut keys) };
        Ok(Self {
            keys,
            last_held: Vec::new(),
            messages: Vec::new(),
            exited: false,
            ids: HashMap::new(),
            next_id: 1,
            ids_level: (0, 0, 0),
        })
    }

    pub fn check_alive(&self) -> Result<()> {
        if self.exited {
            bail!(
                "the Doom engine has exited (status {}). Restart the MCP server to play again",
                unsafe { ffi::dmcp_exit_status() }
            );
        }
        Ok(())
    }

    pub fn run_tic(&mut self) -> Result<()> {
        self.check_alive()?;
        if unsafe { ffi::dmcp_run_tic() } != 0 {
            self.exited = true;
            self.check_alive()?;
        }
        let mut buf = [0 as c_char; 128];
        if unsafe { ffi::dmcp_take_message(buf.as_mut_ptr(), buf.len() as c_int) } > 0 {
            let msg = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
            self.messages.push(msg.to_string_lossy().into_owned());
        }
        Ok(())
    }

    pub fn run_tics(&mut self, n: u32) -> Result<()> {
        for _ in 0..n {
            self.run_tic()?;
        }
        Ok(())
    }

    /// Hold the requested controls for `input.tics` tics.
    pub fn step(&mut self, input: &Input) -> Result<()> {
        let held = self.press(input)?;
        if let Some(slot) = input.weapon {
            unsafe { ffi::dmcp_key(1, (b'0' + slot) as c_int) };
        }

        let mut turn_left = (input.turn_degrees * UNITS_PER_TURN / 360.0).round() as i32;
        let tics = input.tics.max(1);
        for i in 0..tics {
            if turn_left != 0 {
                let units = turn_left.clamp(-MAX_UNITS_PER_TIC, MAX_UNITS_PER_TIC);
                unsafe { ffi::dmcp_turn(units) };
                turn_left -= units;
            }
            self.run_tic()?;
            if i == 0
                && let Some(slot) = input.weapon
            {
                unsafe { ffi::dmcp_key(0, (b'0' + slot) as c_int) };
            }
        }
        // Finish any turn longer than the step allowed for.
        while turn_left != 0 {
            let units = turn_left.clamp(-MAX_UNITS_PER_TIC, MAX_UNITS_PER_TIC);
            unsafe { ffi::dmcp_turn(units) };
            turn_left -= units;
            self.run_tic()?;
        }
        self.release(held);
        Ok(())
    }

    /// Press and hold the movement/fire/use/run keys `input` asks for.
    /// Returns the held keys, to pass to `release`.
    pub fn press(&mut self, input: &Input) -> Result<Vec<c_int>> {
        self.check_alive()?;
        let k = self.keys;
        let mut held = Vec::new();
        match input.movement {
            Dir::Pos => held.push(k.up),
            Dir::Neg => held.push(k.down),
            Dir::None => {}
        }
        match input.strafe {
            Dir::Pos => held.push(k.straferight),
            Dir::Neg => held.push(k.strafeleft),
            Dir::None => {}
        }
        if input.fire {
            held.push(k.fire);
        }
        if input.use_ {
            held.push(k.use_);
        }
        if input.run {
            held.push(k.speed);
        }

        // "Use" only triggers on a fresh press: if it was held last step too,
        // let one tic pass with it released so this press registers.
        if input.use_ && self.last_held.contains(&k.use_) {
            self.run_tic()?;
        }
        for &key in &held {
            unsafe { ffi::dmcp_key(1, key) };
        }
        Ok(held)
    }

    /// Queue the releases; they are processed at the start of the next tic.
    pub fn release(&mut self, held: Vec<c_int>) {
        for &key in &held {
            unsafe { ffi::dmcp_key(0, key) };
        }
        self.last_held = held;
    }

    /// Turn by `degrees` (positive = right) during the next tic. At most 90.
    pub fn turn_next_tic(&mut self, degrees: f64) {
        let units = (degrees * UNITS_PER_TURN / 360.0).round() as i32;
        let units = units.clamp(-MAX_UNITS_PER_TIC, MAX_UNITS_PER_TIC);
        if units != 0 {
            unsafe { ffi::dmcp_turn(units) };
        }
    }

    /// Tap "use" for one tic.
    pub fn tap_use(&mut self) -> Result<()> {
        let key = self.keys.use_;
        if self.last_held.contains(&key) {
            self.run_tic()?;
        }
        unsafe { ffi::dmcp_key(1, key) };
        self.run_tic()?;
        unsafe { ffi::dmcp_key(0, key) };
        self.last_held.clear();
        Ok(())
    }

    /// Tap each key in turn (press, 1 tic, release, `gap` tics).
    pub fn press_keys(&mut self, keys: &[c_int], gap: u32) -> Result<()> {
        self.check_alive()?;
        for &key in keys {
            unsafe { ffi::dmcp_key(1, key) };
            self.run_tic()?;
            unsafe { ffi::dmcp_key(0, key) };
            self.run_tics(gap.max(1))?;
        }
        Ok(())
    }

    pub fn new_game(&mut self, skill: i32, episode: i32, map: i32) -> Result<()> {
        self.check_alive()?;
        unsafe { ffi::dmcp_new_game(skill, episode, map) };
        // Let the level load and the screen wipe finish.
        self.run_tics(TICRATE)
    }

    pub fn key_bindings(&self) -> Keys {
        self.keys
    }

    pub fn take_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.messages)
    }

    pub fn state(&self) -> State {
        let mut s = State::default();
        unsafe { ffi::dmcp_state(&mut s) };
        s
    }

    /// Id for the object at engine address `ptr`, assigning a new one if needed.
    pub fn thing_id(&mut self, ptr: i64) -> u32 {
        let next = &mut self.next_id;
        *self.ids.entry(ptr).or_insert_with(|| {
            *next += 1;
            *next - 1
        })
    }

    fn sync_ids(&mut self) {
        let s = self.state();
        let (episode, map, leveltime) = self.ids_level;
        if (s.episode, s.map) != (episode, map) || s.leveltime < leveltime {
            self.ids.clear();
            self.next_id = 1;
        }
        self.ids_level = (s.episode, s.map, s.leveltime);
    }

    pub fn things(&mut self, radius: f64) -> Vec<Thing> {
        self.sync_ids();
        let mut out = vec![Thing::default(); 1024];
        let n = unsafe { ffi::dmcp_things(out.as_mut_ptr(), out.len() as c_int, radius) };
        out.truncate(n.max(0) as usize);
        out
    }

    /// All map objects, without assigning observation ids.
    pub fn things_snapshot(&self) -> Vec<Thing> {
        let mut out = vec![Thing::default(); 4096];
        let n = unsafe { ffi::dmcp_things(out.as_mut_ptr(), out.len() as c_int, 1e9) };
        out.truncate(n.max(0) as usize);
        out
    }

    pub fn lines(&self) -> Vec<Line> {
        let total = unsafe { ffi::dmcp_num_lines() }.max(0) as usize;
        let mut out = vec![Line::default(); total];
        let n = unsafe { ffi::dmcp_lines(out.as_mut_ptr(), total as c_int) };
        out.truncate(n.max(0) as usize);
        out
    }

    pub fn sectors(&self) -> Vec<Sector> {
        let total = unsafe { ffi::dmcp_num_sectors() }.max(0) as usize;
        let mut out = vec![Sector::default(); total];
        let n = unsafe { ffi::dmcp_sectors(out.as_mut_ptr(), total as c_int) };
        out.truncate(n.max(0) as usize);
        out
    }

    /// Index of the sector containing map point (x, y), if in a level.
    pub fn point_sector(&self, x: f64, y: f64) -> Option<usize> {
        let i = unsafe { ffi::dmcp_point_sector(x, y) };
        (i >= 0).then_some(i as usize)
    }

    /// Copy of the current 320x200 frame as packed RGB.
    pub fn frame_rgb(&self) -> Vec<u8> {
        let fb =
            unsafe { std::slice::from_raw_parts(ffi::dmcp_framebuffer(), SCREEN_W * SCREEN_H) };
        let mut rgb = Vec::with_capacity(fb.len() * 3);
        for &px in fb {
            rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8]);
        }
        rgb
    }
}
