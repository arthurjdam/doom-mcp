//! ASCII map centred on the player. Either egocentric (the direction the
//! player faces is up) or north-up (axis-aligned walls stay straight).

use std::fmt::Write as _;

use crate::engine::Engine;
use crate::engine::ffi::{Line, ML_BLOCKING, ML_MAPPED, ML_SECRET};
use crate::world::observe;

/// Smallest gap the player (56 units tall) fits through.
const PLAYER_HEIGHT: f64 = 56.0;
/// Highest step the player can climb.
const MAX_STEP: f64 = 24.0;

const EXIT_SPECIALS: &[i32] = &[11, 51, 52, 124, 197, 198];
const LOCKED_DOOR_SPECIALS: &[i32] = &[26, 27, 28, 32, 33, 34, 99, 133, 134, 135, 136, 137];
const DOOR_SPECIALS: &[i32] = &[1, 31, 46, 117, 118];
const LIFT_SPECIALS: &[i32] = &[10, 21, 62, 88, 120, 121, 122, 123];

/// Higher wins when two features land on the same cell.
fn priority(c: char) -> u8 {
    match c {
        '^' => 9,
        'M' => 8,
        'E' => 7,
        'K' => 6,
        'D' => 5,
        'L' => 4,
        'S' => 4,
        '*' => 3,
        'o' => 3,
        '#' => 2,
        '%' => 2,
        '.' => 1,
        _ => 0,
    }
}

pub fn classify(l: &Line) -> Option<char> {
    let special = l.special;
    if EXIT_SPECIALS.contains(&special) {
        return Some('E');
    }
    if LOCKED_DOOR_SPECIALS.contains(&special) {
        return Some('K');
    }
    if DOOR_SPECIALS.contains(&special) {
        return Some('D');
    }
    if LIFT_SPECIALS.contains(&special) {
        return Some('L');
    }
    if l.two_sided == 0 {
        // A special on a solid wall is almost always a switch.
        return Some(if special != 0 { 'S' } else { '#' });
    }
    if l.flags & (ML_BLOCKING | ML_SECRET) != 0 {
        return Some('#');
    }
    let opening = l.front_ceiling.min(l.back_ceiling) - l.front_floor.max(l.back_floor);
    if opening < PLAYER_HEIGHT {
        return Some('#');
    }
    if (l.front_floor - l.back_floor).abs() > MAX_STEP {
        return Some('%');
    }
    None
}

pub struct MapOptions {
    /// Grid width and height in cells (odd, so the player sits in the middle).
    pub cells: usize,
    /// Map units per cell.
    pub cell_size: f64,
    /// Include walls the player hasn't seen yet and things not in sight.
    pub reveal: bool,
    /// Draw with north up instead of rotating so the player faces up.
    pub north_up: bool,
}

/// `route` is the planned path (waypoints after the player's position), drawn as dots.
pub fn render(engine: &mut Engine, opts: &MapOptions, route: &[(f64, f64)]) -> String {
    let s = engine.state();
    let (status, _) = observe::status(&s);
    if s.in_level == 0 || matches!(status, "title" | "intermission" | "finale") {
        return "No map: not currently in a level.".into();
    }

    let n = opts.cells | 1;
    let half = (n / 2) as f64;
    let mut grid = vec![vec![' '; n]; n];
    let up = if opts.north_up { 90.0_f64 } else { s.angle };
    let (sin, cos) = up.to_radians().sin_cos();
    let (px, py) = (s.x, s.y);

    // World -> (row, col); forward is up, right is right.
    let to_cell = |x: f64, y: f64| -> Option<(usize, usize)> {
        let (dx, dy) = (x - px, y - py);
        let forward = dx * cos + dy * sin;
        let right = dx * sin - dy * cos;
        let col = (half + right / opts.cell_size).round();
        let row = (half - forward / opts.cell_size).round();
        (col >= 0.0 && row >= 0.0 && col < n as f64 && row < n as f64)
            .then_some((row as usize, col as usize))
    };
    let plot = |grid: &mut Vec<Vec<char>>, x: f64, y: f64, c: char| {
        if let Some((r, col)) = to_cell(x, y)
            && priority(c) >= priority(grid[r][col])
        {
            grid[r][col] = c;
        }
    };

    let reach = opts.cell_size * (half + 1.0) * std::f64::consts::SQRT_2;
    for l in engine.lines() {
        if !opts.reveal && l.flags & ML_MAPPED == 0 {
            continue;
        }
        let Some(c) = classify(&l) else { continue };
        let len = (l.x2 - l.x1).hypot(l.y2 - l.y1);
        // Cheap reject: both endpoints far away and the line too short to cross the view.
        let d1 = (l.x1 - px).hypot(l.y1 - py);
        let d2 = (l.x2 - px).hypot(l.y2 - py);
        if d1.min(d2) > reach + len {
            continue;
        }
        let steps = (len / (opts.cell_size / 3.0)).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            plot(
                &mut grid,
                l.x1 + (l.x2 - l.x1) * t,
                l.y1 + (l.y2 - l.y1) * t,
                c,
            );
        }
    }

    // The route, under everything else.
    let mut prev = (px, py);
    for &(x, y) in route {
        let len = (x - prev.0).hypot(y - prev.1);
        let steps = (len / (opts.cell_size / 2.0)).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            plot(
                &mut grid,
                prev.0 + (x - prev.0) * t,
                prev.1 + (y - prev.1) * t,
                '.',
            );
        }
        prev = (x, y);
    }

    let mut legend = Vec::new();
    let things = observe::things(engine, reach, 256, true);
    for th in &things {
        if !opts.reveal && !th.line_of_sight {
            continue;
        }
        let c = match th.category {
            "monster" => 'M',
            "barrel" | "obstacle" => 'o',
            "projectile" => continue,
            _ => '*',
        };
        let a = (s.angle - th.bearing).to_radians();
        let (x, y) = (
            px + th.distance as f64 * a.cos(),
            py + th.distance as f64 * a.sin(),
        );
        if to_cell(x, y).is_some() {
            plot(&mut grid, x, y, c);
            if c == 'o' {
                continue;
            }
            legend.push(format!(
                "  {c} [{}] {} {}u bearing {:+.0}°",
                th.id, th.name, th.distance, th.bearing
            ));
        }
    }

    let mid = n / 2;
    grid[mid][mid] = if opts.north_up {
        ['>', '^', '<', 'v'][((s.angle + 45.0).rem_euclid(360.0) / 90.0) as usize % 4]
    } else {
        '^'
    };

    let mut out = String::new();
    if opts.north_up {
        let _ = writeln!(
            out,
            "North-up map ({n}x{n} cells, {} units per cell). You are the arrow in the centre, facing {:.0}° (0 = east/right, 90 = north/up).",
            opts.cell_size, s.angle
        );
    } else {
        let _ = writeln!(
            out,
            "Egocentric map ({n}x{n} cells, {} units per cell). You are '^' in the centre; up = the direction you face ({:.0}°).",
            opts.cell_size, s.angle
        );
    }
    let border: String = std::iter::repeat_n('-', n).collect();
    let _ = writeln!(out, "+{border}+");
    for row in &grid {
        let line: String = row.iter().collect();
        let _ = writeln!(out, "|{line}|");
    }
    let _ = writeln!(out, "+{border}+");
    out.push_str(
        "Legend: # wall/impassable  % ledge (step >24 high)  D door  K locked door (needs key)  \
         L lift  S switch  E EXIT  M monster  * pickup  o obstacle (barrel, pillar...)  . your route\n",
    );
    if !opts.reveal {
        out.push_str("Only walls you have seen are drawn; look around to reveal more.\n");
    }
    if !legend.is_empty() {
        out.push_str("Things on map:\n");
        out.push_str(&legend.join("\n"));
        out.push('\n');
    }
    out
}
