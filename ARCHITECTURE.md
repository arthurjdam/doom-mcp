# Architecture

doom-mcp lets an AI play DOOM over MCP. It has two modes that share one core:

- **Turn-based** (`--mode turn`, the default): the game is frozen between tool calls. The model
  controls every action (`act`) and the server reports exactly what happened. It is
  deterministic, which makes it the test bed for everything else.
- **Real-time** (`--mode realtime`): the game runs continuously at 35 tics per second. A fast,
  rule-based **pilot** plays moment to moment (moving, fighting, opening doors), and the model
  acts as **commander**: it gives standing orders and reacts to events.

```
              MCP client (Claude)
                     │ tools
         ┌───────────┴───────────┐
         │ mcp::turn │ mcp::realtime │      front-ends: tool definitions only
         └───────────┬───────────┘
                     │ jobs (closures run on the game thread)
               ┌─────┴─────┐
               │   game    │   the game thread: owns everything below,
               │  Session  │   drives the clock (stepped or 35 Hz)
               └─────┬─────┘
          ┌──────────┼──────────┐
       ┌──┴──┐   ┌───┴───┐  ┌───┴────┐
       │pilot│──▶│ world │  │ viewer │◀── frames, log, status, spectator messages
       └──┬──┘   └───┬───┘  └────────┘
          └────┬─────┘
           ┌───┴────┐
           │ engine │   safe wrapper around the C engine (one tic at a time)
           └───┬────┘
           csrc/dmcp.c + vendor/doomgeneric (C)
```

## Layers

Each layer only uses the layers below it.

| Module | Responsibility | Knows about |
|---|---|---|
| `engine` | Safe wrapper around doomgeneric: run one tic, hold keys and turn (`Controls`), read state, things, lines and sectors, and grab frames. Has no opinion about how to play. | the C shim |
| `world` | *Understanding* the game, as pure functions over engine state: observations and threat lists (`observe`), route planning with prerequisites (`nav`), and the ASCII map (`map`). | engine |
| `pilot` | *Reflexes*: given standing `Orders` and the current state, decide this tic's `Controls`. `travel` follows routes (doors, movers, unsticking); `combat` picks targets and weapons, aims, fires and dodges. Emits events. | engine, world |
| `game` | The game thread. `Session` owns the engine, navigator, pilot, plan, briefing and event log. `Game` is the cloneable handle the front-ends use; it runs jobs on the thread and, in real-time mode, ticks the pilot 35 times a second. | engine, world, pilot, viewer |
| `mcp` | The two MCP front-ends. They define tools, validate arguments, call into `game`, and format results. No game logic. | game, world, viewer |
| `viewer` | Local spectator page: paced video, minimap, log, plan, and the message box. Receives plain data; doesn't reach into the game. | engine (frames only) |

## The pilot and the commander (real-time mode)

```
 every tic (28.6 ms)                              every few seconds
 ┌───────────────────────────────┐               ┌──────────────────────────┐
 │ pilot.tick(orders, world)     │  events ───▶  │ model: wait_for_events   │
 │   combat: target? aim, fire   │               │   decide, then `command` │
 │   travel: follow route        │  ◀── orders   │   (goal, stance, focus…) │
 │ → Controls → engine.tic()     │               └──────────────────────────┘
 └───────────────────────────────┘
```

- **Orders** are standing instructions that last until changed: navigation goal, stance
  (`aggressive`, `balanced`, `cautious`, `hold_fire`), focus target, whether to move, and a
  one-shot `use`.
- **Events** go into the session's `EventLog` with a sequence number. Examples: a monster
  spotted, damage taken, a kill, a pickup, arrived, blocked, low health, died, level complete,
  and spectator messages. Important events wake up `wait_for_events` early.
- The **turn-based** `follow_route` uses the same `pilot::travel` logic, stepped tic by tic until
  something needs the model, so there is a single implementation of route following.

## Clocks

The engine's own clock is always virtual: the shim moves it to the next tic boundary and runs
exactly one tic per step (Doom's `singletics` mode), so one step is one tic and one frame. Only
*who decides when to step* differs:

- **Turn-based:** a tool call steps the game N tics, as fast as possible. The spectator page
  replays those frames at 35 fps, and the next game-advancing call waits for that replay to
  (nearly) finish.
- **Real-time:** the game thread loop runs jobs as they arrive and steps between them. With the
  spectator page open, **the video's playback clock is the master**: a tic runs whenever fewer
  than 3 frames are waiting to be shown. The game therefore goes exactly as fast as the video
  plays, never gets ahead, and bursts (a screen melt draws many frames at once) play out before
  it continues. With nobody watching, tics run every 1/35 s. Tics are never skipped, and never
  run back to back to catch up, since that would leave the video permanently behind.

## Tools

| Turn-based | Real-time |
|---|---|
| `new_game`, `act`, `observe`, `route`, `set_plan`, `get_map`, `press_keys` | `new_game`, `command`, `wait_for_events`, `observe`, `set_plan`, `get_map`, `press_keys` |

## Spectator view

The same in both modes. Frames are paced to 35 fps. In turn-based mode, game-advancing tools
wait for playback to catch up. In real-time mode the game already runs at playback speed.
Spectator messages are delivered at the top of the next tool result, and in real-time mode they
also wake `wait_for_events`.
