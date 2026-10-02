# doom-mcp

An [MCP](https://modelcontextprotocol.io) server that lets an LLM play the original DOOM (1993).

It embeds the real id Software engine (via [doomgeneric](https://github.com/ozkl/doomgeneric))
into a Rust binary and exposes it as MCP tools over stdio. The game runs on a **virtual clock**,
so it is frozen between tool calls: the model can think as long as it wants, and every action is
deterministic.

## Quick start

```sh
cargo build --release
# Put a WAD next to Cargo.toml. The shareware doom1.wad (episode 1) is freely distributable.
./target/release/doom-mcp --wad doom1.wad   # speaks MCP on stdin/stdout
```

### Claude Code

This repo ships a `.mcp.json`, so starting `claude` in this directory picks the server up
automatically. To add it from anywhere else:

```sh
claude mcp add doom -- /path/to/doom-mcp/target/release/doom-mcp --wad /path/to/doom1.wad
```

Then ask: *"Start a new game of Doom and try to finish E1M1."*

### Claude Desktop / other clients

```json
{
  "mcpServers": {
    "doom": {
      "command": "/path/to/doom-mcp/target/release/doom-mcp",
      "args": ["--wad", "/path/to/doom1.wad"]
    }
  }
}
```

WAD lookup order: `--wad`, then `$DOOM_WAD`, then `doom1.wad`/`doom.wad`/`doom2.wad` in the
current directory or next to the binary. Extra engine flags go after `--`, e.g. `-- -fast`.

## Watching it play

The server also serves a live **spectator page** at <http://127.0.0.1:6660>. It opens in your
browser automatically the first time the model calls `new_game`. It shows:

- the game at a smooth 35 fps (every rendered frame, including the screen melt);
- a minimap of the level showing the player, the planned route and the current goal;
- the model's plan (`set_plan`);
- an action log showing what the model did, with the outcome (health, ammo, kills, damage);
- the model's own narration, if it passes `act`'s optional `comment`.

The game only advances while the model is acting, so the video plays each action back in real
time and holds on the last frame while the model is thinking. Flags: `--viewer-port PORT`,
`--no-open` (serve the page but don't open a browser), `--no-viewer`. If port 6660 is taken,
a free port is used; the URL is printed to stderr and included in the `new_game` result.

## Tools

| Tool | Game time passes? | What it does |
|---|---|---|
| `new_game` | yes (1s load) | Start at a skill/episode/map, skipping the menus. Returns the level briefing. |
| `act` | yes, `tics` | Hold movement/strafe/fire/use/run/weapon controls for N tics (35/s), with an exact relative `turn` or `aim_at` a thing. With `follow_route: true` the server walks the planned route instead (below). |
| `route` | no | Pick a navigation goal: `exit`, `secret_exit`, a key colour, `switch`, `explore`, a `thing` id, or a `point`. Returns the full route. |
| `set_plan` | no | Claude writes or replaces its plan for the level. The plan is echoed in every observation and shown to spectators. |
| `observe` | no | Current observation. |
| `get_map` | no | ASCII map: walls, ledges, doors, locked doors, lifts, switches, **exit**, monsters, pickups, obstacles, and the route as dots. Egocentric or `north_up`. |
| `press_keys` | yes, briefly | Raw key taps for menus, the automap, cheat codes. |

## Navigation and planning

Steering through a 3D maze one tool call at a time is the hard part for an LLM, so the server
plans routes itself (`src/nav.rs`) and leaves decisions and combat to the model.

- **Route planning over the real geometry.** The level is rasterised into 16-unit cells.
  Every move between cells is checked against the linedefs it crosses, using Doom's movement
  rules: steps of at most 24 units, at least 56 units of headroom, manual doors, locked doors
  and their keys, lifts, damaging floors, solid decorations, and teleporters. Paths keep away
  from walls and are smoothed into a few waypoints.
- **Prerequisites are worked out automatically.** If the goal is unreachable, the planner
  searches again assuming locked doors and triggered sectors could be opened. It takes the
  first obstacle on that path and routes to whatever removes it: the right key, or the
  switch or walk-over line whose tag moves that sector (including stair builders). Chains
  nest, e.g. E1M4 is *switch → blue key → yellow key → switch → exit*. Every observation
  explains it: "The exit is behind a locked red door, so this route goes to the red key first."
- **Level briefing** on every level start: how to reach the exit and through what, locked
  doors and keys, and monster/item/secret counts.
- **`follow_route`.** The server steers every tic along the route, opens ordinary doors, waits
  for moving floors and doors, works itself free when snagged on a corner, and stops early when
  a new monster comes into view, you take damage, or you arrive at a switch, lift or the goal.
- **Waypoint marker.** Screenshots show the next waypoint as a yellow diamond, or an arrow at
  the screen edge when it's out of view.

Tested on shareware episode 1: E1M1 and E1M2 complete with a simple scripted
follow-and-shoot loop, and every level except the boss map (E1M8) gets a route or a chained
plan to the exit.

Every observation contains:

- **Text summary**: status (`playing`/`dead`/`menu`/`intermission`/...), a hint for what to do
  next, health, armor, ammo, weapons, keys, position and facing, kills, items and secrets.
- **Events** from the action: pickups ("Picked up a shotgun."), damage taken and from where,
  kills, deaths, and `BLOCKED` when movement was obstructed.
- **Things**: nearby monsters, projectiles and pickups, each with an id, distance, *bearing*
  (degrees from the crosshair, + = right), whether it is on screen or merely in line of sight,
  monster health, and whether it is targeting you.
- **Screenshot**: PNG with a small crosshair, aspect-corrected to 4:3 (default 640x480).
- **`structuredContent`**: the same data as JSON.

## How it works

```
 MCP client ──stdio──▶ rmcp server (tokio) ──jobs──▶ doom thread ──FFI──▶ csrc/dmcp.c ──▶ doomgeneric (C)
```

- **Engine**: `vendor/doomgeneric` (GPL-2.0, upstream commit `dcb7a8d`) is compiled by `build.rs`
  with the `cc` crate at native 320x200. Upscaling and PNG encoding happen in Rust.
- **Shim** (`csrc/dmcp.c`): implements the doomgeneric platform layer (`DG_*`) and a small
  C API for stepping one tic, injecting events, and reading player state, map objects (with
  `P_CheckSight` line-of-sight checks) and linedefs.
- **Virtual time**: `DG_GetTicksMs` returns a counter that only advances when the engine sleeps.
  The engine "waits" for its next tic, time jumps forward, and a stepping loop runs exactly N tics.
- **Input**: key events are posted straight into Doom's event queue using its own key bindings.
  Turning uses mouse events, which map linearly onto the engine's turn units, so `turn: 37.5`
  turns exactly 37.5° (±0.05°).
- **Engine exits**: the engine is compiled with `-Dexit=dmcp_exit`, so `I_Error`/`I_Quit` unwind
  back into the shim with `longjmp` instead of killing the server. Later calls return a readable
  tool error.
- **stdout**: Doom `printf`s a lot. At startup, fd 1 is pointed at stderr and MCP gets a
  `dup` of the original stdout, so engine output can't corrupt the JSON-RPC stream.
- **Spectator view** (`src/viewer.rs`): the shim calls back into Rust on every frame. Frames
  are queued, re-paced to 35 fps, PNG-encoded and pushed to the browser over Server-Sent Events
  from a tiny HTTP server bound to 127.0.0.1.
- **MCP**: [`rmcp`](https://crates.io/crates/rmcp) 3.5 implements protocol revision
  `2026-07-28` (stateless, `server/discover`) and still accepts older clients that use the
  `initialize` handshake.

## Limitations

- One game per process: doomgeneric uses global state. If the game is quit from the menu,
  restart the server.
- No sound.
- The planner is heuristic. Lifts are approached and then handed back to the model; boss
  levels open their exit when the bosses die, which it can't predict; and a few kinds of
  trigger aren't modelled. When there's no known route it says so and suggests the `switch` or
  `explore` goals. Set `DOOM_MCP_NAV_DEBUG=1` to print the reachability cut to stderr.
- Save/load isn't exposed as a tool; the engine writes its config to `$TMPDIR/doom-mcp`.

## License

The engine in `vendor/doomgeneric` is GPL-2.0, which makes the combined binary GPL-2.0 too.
DOOM WADs are not included. The shareware `doom1.wad` may be freely distributed.
