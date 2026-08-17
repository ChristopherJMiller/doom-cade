# arcade-telemetry.pk3 — empirical findings (SPEC §13.1–13.3)

Target engine: **GZDoom 4.14.2** (pinned via nixpkgs; `nix eval --raw
nixpkgs#gzdoom.version` → `4.14.2`). All findings below were established by
running that exact build headlessly (Xvfb + llvmpipe, Freedoom Phase 2
0.13.0 as IWAD, this `pk3/` directory passed to `-file`) on 2026-08-17.
Freedoom note: the nixpkgs-unstable snapshot in use has **no `freedoom`
attribute**; it was fetched from the `nixos-26.05` channel instead
(`nix build 'https://channels.nixos.org/nixos-26.05/nixexprs.tar.xz#freedoom'`).

## §13.1 — transport: does Console.Printf reach `-logfile`, and does a FIFO work?

**Yes, verified live.** `+logfile <path>` accepts a **named FIFO** (`mkfifo`
+ `cat fifo > capture &`): GZDoom opens and writes it without complaint, and
`ARCADE_EVT` lines emitted via `Console.Printf` arrived through the pipe
**in real time during play** (observed run_start / level_enter /
player_died / level_complete / run_complete while the game was running).
Caveats for the supervisor:

- The log stream is stdio-buffered on the engine side; engine chatter can sit
  in the buffer for a while, but every `Console.Printf` from the handler
  appeared promptly in testing. Everything flushes on clean exit.
- Regular-file `+logfile` also works (used for most compile checks here).
- stdout capture (the SPEC's fallback) also carries the same text on Linux,
  but is block-buffered when redirected to a file — under SIGKILL the tail
  is lost. Preferred transport = the FIFO, as the SPEC hoped.
- `Console.Printf` uses the default print level, which also paints the line
  into the on-screen notify area. If that proves ugly on the cabinet,
  `Console.PrintfEx(PRINT_LOG, ...)` exists on 4.14.2 (`engine/base.zs:678`)
  as a candidate — but whether PRINT_LOG output reaches the logfile/stdout
  identically has NOT been tested; switch only with a re-test.

## §13.2 — exact ZScript API names on 4.14.2

All verified two ways: by reading `gzdoom.pk3`'s own zscript sources from
the pinned build, and by compiling + running this mod against it.

- `LevelLocals` (global `level`): `MapName`, `LevelName`, `maptime`,
  `totaltime`, `killed_monsters`, `total_monsters`, `found_secrets`,
  `total_secrets`, `found_items`, `total_items` — all present
  (`zscript/doombase.zs`), all emitted correctly in live events.
- `StaticEventHandler` is `play` scope; `WorldLoaded` / `WorldUnloaded` /
  `WorldThingDied` take `WorldEvent` with `IsSaveGame` / `IsReopen`
  (`zscript/events.zs`). `WorldEvent.NextMap` also exists (unused).
- 4.14.2 **does** have `NewGame()` and `PlayerDied(PlayerEvent)` virtuals on
  event handlers, contrary to the design's suspicion — but the frozen
  design's first-WorldLoaded / WorldThingDied approach was kept: it needs
  nothing version-specific and runs with LevelLocals live.
- `CVar.GetCVar(Name, PlayerInfo)`, `G_SkillPropertyInt(SKILLP_ACSReturn)`,
  `String.Format`/`AppendFormat`/`ByteAt`/`Mid` all present and working.
- **`SKILLP_ACSReturn` is 0-based**: launching with `-skill 3` (HMP) makes
  it read **2**. The handler emits `+1` so `run_start.skill` matches the
  1-based `-skill` convention the supervisor uses (verified live: payload
  shows `"skill":2` before the shift; shifted build compiles clean).
- **Lexer landmine (cost an hour, do not regress):** when the ZScript
  tokenizer looks for the end of a string literal, only `\"` is honored as
  an escape pair — a literal `"\\\\"` swallows its own closing quote and
  the string runs on to the next bare quote in the file, producing
  `Unexpected character: \ (ASCII 92)` / `Unexpected identifier` errors far
  from the real cause. (Bisected: `"a\\b"` OK, `"\""` OK, `"\\\""` OK,
  `"\\\\"` **broken**.) Consequently `handler.zs` contains **no backslash
  characters inside string literals at all**; every backslash in the JSON
  output is produced via `AppendFormat`/`Format` with `%c` and code 92.
- `String.Length()` is unsigned; compare against a signed copy or the
  compiler warns (fixed in `JsonEscape`).

## §13.3 — does WorldUnloaded fire on the final map's exit?

**Yes, verified live.** With `MAPINFO` giving MAP08 `next = "EndTitle"`
(`endtitle` confirmed as a keyword in the 4.14.2 binary), a *real* exit on
MAP08 (`special Exit_Normal 0` from the console, sv_cheats) fired
`WorldUnloaded` **with stats intact**: the log shows `level_complete` for
MAP08 with correct monster/secret/item totals and `maptime_tics`, followed
by `run_complete` whose `total_maptime_tics` (4579) exactly equals the sum
of the three per-map times of that test run (2031 + 1260 + 1288). The
engine then went to the end-of-game screen and back to the title with the
neutered main menu (NEW GAME / OPTIONS / QUIT GAME — no save/load: MENUDEF
override verified on screen). No end-sequence fallback needed.

Caveat kept in MAPINFO comments anyway: the `nextmap` **console command**
refuses to advance off MAP08 ("no next map!") because its next is an end
sequence, not a map — only real exits (exit lines/switches,
`Exit_Normal`) follow it. Fine for production, mildly annoying for manual
testing.

## Live-run event log (verbatim from the FIFO)

```
ARCADE_EVT {"v":1,"event":"run_start","session":"3f1c2a9e-test-uuid","initials":"A\"B\" ","skill":2,"ts":0}
ARCADE_EVT {"v":1,"event":"level_enter","session":"3f1c2a9e-test-uuid","map":"MAP01","level_name":"Hydroelectric Plant","ts":0}
ARCADE_EVT {"v":1,"event":"player_died","session":"3f1c2a9e-test-uuid","map":"MAP01","kills":0,"secrets":0,"maptime_tics":3304}
ARCADE_EVT {"v":1,"event":"level_complete","session":"run2-test","map":"MAP01","kills":0,"total_monsters":18,"secrets":0,"total_secrets":3,"items":0,"total_items":32,"maptime_tics":2031}
ARCADE_EVT {"v":1,"event":"level_enter","session":"run2-test","map":"MAP02","level_name":"Filtration Tunnels","ts":0}
ARCADE_EVT {"v":1,"event":"level_complete","session":"run2-test","map":"MAP08","kills":0,"total_monsters":88,"secrets":0,"total_secrets":3,"items":0,"total_items":32,"maptime_tics":1288}
ARCADE_EVT {"v":1,"event":"run_complete","session":"run2-test","total_maptime_tics":4579}
```

(`skill":2` predates the +1 shift; two separate sessions shown. Every line
above parses under `protocol::parse_event_line` by inspection.)

## Additional findings for downstream components

- **`ts` (= `level.totaltime`) is unreliable at WorldLoaded time**: it read
  0 on MAP02 entry after ~58 s on MAP01, and 1260 on MAP08 entry after two
  maps. It appears to be updated at intermission end, not level start. The
  protocol crate documents `ts` as a unix timestamp; ZScript has no wall
  clock, and the parent directive fixed `ts` = `level.totaltime`. The
  supervisor should timestamp events on receipt and treat `ts` as
  advisory. Per-map `maptime_tics` (what scoring uses) is accurate.
- **`+set` mangles hostile cvar values**: passing initials `A"B\` on the
  command line stored `A"B" ` — the engine's own command tokenizer, not the
  pk3, rewrote it. Production initials are `[A-Z0-9]{3}` so this never
  triggers; JsonEscape exists for values set maliciously *in-engine* (e.g.
  a console `set arcade_initials` with quotes — live-verified that an
  embedded `"` comes out as a correct `\"` in the JSON).
- **JsonEscape live coverage**: quote branch verified live; backslash and
  control-char branches share the identical `%c` code path but were not
  observed live (the +set mangling above ate the test backslash).
- **Autosave fires on map entry** ("Game saved." on MAP08 entry) even with
  `-noautoload`. The pristine `assets/config/gzdoom.ini` should set
  `disableautosave=1` (and `autosavecount=0`) — belongs to the supervisor /
  assets component, noted here because it also re-arms death-reload via
  autosave.
- **Death behaviour**: after `player_died`, the post-death map reload
  arrives flagged `IsSaveGame` (autosave load) and the handler additionally
  latches `playerDead`, so no spurious `level_enter`/`level_complete` was
  emitted after death in testing (the supervisor's 3 s SIGTERM makes this
  moot in production).
- **Quit-while-alive**: whether `WorldUnloaded` fires on engine exit
  mid-map (watchdog SIGTERM with a live player) is **untested** — the one
  observed quit-from-map happened with `playerDead` latched, which
  suppresses emission anyway. If it does fire, the supervisor would see a
  trailing `level_complete` for an uncompleted map on abandoned runs;
  its state machine should ignore events after it decides a run is over.
- **MAP03 → MAP07 chain**: not traversed live (test session ended);
  MAP01→MAP02 and MAP08→EndTitle chains verified live, MAP03/MAP07 blocks
  parse under `G_ParseMapInfo` in every run, and `map07special` is included
  for Dead Simple's tag-666/667 scripting.
- **Not tested at all**: behaviour under `cage`/Wayland (all testing was
  Xvfb/X11), sound enabled, the real doom2.wad IWAD, and the
  FIFO-under-supervisor lifecycle (open/close ordering, reader restarts).
