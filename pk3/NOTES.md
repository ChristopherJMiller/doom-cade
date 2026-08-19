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
- ~~`Console.Printf` uses the default print level, which also paints the
  line into the on-screen notify area.~~ **Resolved 2026-08-18**: once the
  2 s progress heartbeat existed, plain Printf kept the top of the screen
  permanently covered in raw JSON (and buried the HUD overlay), so `Emit`
  now uses `Console.PrintfEx(PRINT_LOG, ...)` (`engine/base.zs:678`,
  `PRINT_LOG` = "only to logfile", `base.zs:141`). Re-tested live as §13.1
  demanded: an otherwise-identical scripted run captured the identical
  set of telemetry lines through the FIFO (19/19, all parseable), and
  screenshots show a clean notify area. PRINT_LOG output does reach the
  `+logfile` FIFO exactly like the default level did.

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

## Partial credit: `progress` heartbeat + hold-Start-to-quit (added 2026-08-18)

Both features live-verified on the same harness (Xvfb + llvmpipe + Freedoom
Phase 2 + FIFO logfile), with **real X11 Enter key events** injected via
xdotool/XTEST against the Xvfb display — so the full production path
(hardware-ish key event → `InputProcess` → net event → `WorldTick` counter →
emission) ran end to end, using a copy of the production
`assets/config/gzdoom.ini` (in which Enter is genuinely unbound: the
`[Doom.Bindings]` section replaces GZDoom's defaults wholesale).

### Architecture note: ui/play fence

`InputProcess` is **ui scope** (`virtual ui bool InputProcess(InputEvent e)`,
gzdoom.pk3 `zscript/events.zs:194`) and ui code cannot write play-side
fields. Key state therefore crosses the fence via
`EventHandler.SendNetworkEvent("arcade_start_down"/"arcade_start_up")`
(clearscope static) into `NetworkProcess` (play scope), which sets the
`startHeld` flag consumed by `WorldTick`. `InputEvent.Key_Enter == 0x1c`
confirmed in gzdoom.pk3 `zscript/engine/inputevents.zs:106` *and* live (an
XTEST Enter keydown/keyup drove the whole chain). `InputProcess` returns
false unconditionally — it observes, never consumes.

### What was verified live

- **Heartbeat cadence**: `progress` lines arrived every 2.00 s on the dot
  (`maptime_tics` 70, 140, 210, …), with correct MAP01 totals (18/3/32 —
  same numbers the level_complete tests produced) and sane `px`/`py`.
- **px/py track the player**: idle heartbeats repeated `px:-192,py:-192`;
  after holding +forward for 2 s the next heartbeats read `px:-53` then
  `px:687` — movement is visible to walk-away detection.
- **First-tic suppression**: no heartbeat at map entry (`maptime > 0` and
  modulo-70 gate); first one fires at tic 70.
- **Hold-to-quit timing**: warning appeared exactly 1.0 s after keydown;
  `run_quit` was emitted 2.98–3.00 s after keydown in both full-hold runs
  (`maptime_tics` = keydown-tic + 105 exactly). Emitted **once** — holding
  a further 2 s produced no duplicate (`runQuitEmitted` latch).
- **Release resets**: a 1.75 s hold (warning shown) then release produced
  no `run_quit`, and a subsequent full hold quit 3.0 s from its *own*
  keydown (had the counter not reset it would have fired at ~1.4 s).
- **On-screen warning**: red centered "KEEP HOLDING TO END RUN"
  (`Console.MidPrint(smallfont, ...)`) visible in screenshots while held;
  the empty-`MidPrint` sent on release blanks it immediately (verified in
  a screenshot 0.4 s after keyup) instead of lingering for `con_midtime`.
- **Parser round-trip**: every captured sentinel line (13 progress, 1
  run_quit, plus run_start/level_enter) fed through the real
  `protocol::parse_event_line` via a scratch Rust binary — 15/15 parsed
  into the expected variants, 0 sentinel lines rejected.

Sample lines (verbatim from the FIFO):

```
ARCADE_EVT {"v":1,"event":"progress","session":"holdquit-test-uuid","map":"MAP01","kills":0,"total_monsters":18,"secrets":0,"total_secrets":3,"items":0,"total_items":32,"maptime_tics":350,"px":74,"py":-192}
ARCADE_EVT {"v":1,"event":"run_quit","session":"holdquit-test-uuid","map":"MAP01","maptime_tics":755}
```

### Findings and known behaviour

- **MidPrint echoes into the logfile** (text between dashed separator
  lines) on every call. Re-arming it per tic flooded the telemetry FIFO
  with ~210 junk lines/s while Start was held (observed live), so the
  handler re-arms it only once per second of hold. The supervisor already
  ignores non-sentinel lines; this is about noise, not correctness.
- **Intermissions**: `WorldTick` does not run during intermission screens,
  so holding Start there does nothing — the hold counter neither advances
  nor fires. Known, accepted behaviour: quitting is only possible mid-map.
  `WorldLoaded` additionally resets the hold state on every fresh map
  entry, so a hold spanning an intermission must be released and
  re-pressed on the next map.
- **Menus**: `InputProcess` is bypassed while a menu is open, so releasing
  Start inside a menu is invisible (stale `startHeld` latch). The game is
  paused in single-player menus (no `WorldTick`, counter frozen); after
  closing the menu the player just releases and re-presses. Moot on the
  cabinet, where the menu is unreachable.
- **Heartbeats continue after `run_quit`** until the supervisor's SIGTERM
  lands (observed: two more progress lines in ~1 s). The supervisor should
  act on `run_quit` and ignore the stragglers.
- **`netevent` can forge the hold events** (`IsManual` is deliberately not
  filtered): it grants exactly the power of holding the Start button and
  the cabinet exposes no console; filtering it would only have cost the
  headless test seam.
- **Stock configs bind `Enter=invuse`** (GZDoom default). Irrelevant twice
  over: the cabinet config unbinds it, and `InputProcess` sees every raw
  key event before bindings anyway (verified with Enter unbound; the
  handler never consumes, so a binding would still fire too — `invuse` is
  a no-op in Doom with no inventory).
- **Null-pawn guard**: no `progress` is emitted when
  `players[consoleplayer].mo` is null (px/py are the event's purpose);
  hold-to-quit and heartbeats are both gated on `runStarted && !playerDead`.
- **Harness tip**: GL-rendered frames read back **black** through
  `xwd`/XGetImage; add `+set vid_rendermode 0` (software renderer) when a
  test needs screenshots. Key injection needs the gzdoom window focused
  (`xdotool windowfocus`) and the pointer parked over it (no WM on Xvfb).

## Walk-up HUD overlay (added 2026-08-18)

`RenderOverlay` (ui scope) draws top-right at virtual 1280x800
(`DTA_VirtualWidth/Height` + `DTA_KeepRatio`, `NewSmallFont`): gold live
score, "MAP n/5 · <name>" (rotation position via a case-insensitive match
against the fixed map list; non-rotation maps show just their name), a
gray controls crib that collapses after 525 tics (15 s) of map time, and a
permanent "HOLD START — END RUN + BANK SCORE" line. The score is
`bankedScore` (accumulated play-side in WorldUnloaded — **keep in sync
with crates/protocol/src/scoring.rs**, constants mirrored as class consts)
plus the open map's provisional kills/secrets/items points read off
LevelLocals each frame. Display-only; supervisor/server recomputation
stays authoritative.

Verified live (scripted run: startup command buffer `+wait 750; kill
monsters; wait 350; special Exit_Normal 0` with sv_cheats — no console
toggling needed, so the game never pauses; screenshots via
`vid_rendermode 0` + xwd):

- Full overlay early on MAP01 (`SCORE 0`, `MAP 1/5 · Hydroelectric
  Plant`, controls block, hold-Start line), correctly right-aligned.
- Controls block gone after 15 s; hold-Start line slides up. Reappears on
  MAP02 entry (per-map, keyed off `level.maptime`).
- `kill monsters` (18 kills) → overlay reads `SCORE 180` (18×10) within a
  couple of seconds — live Level-counter mirroring works.
- After `Exit_Normal` into MAP02 the overlay read **`SCORE 2018`**, which
  is exactly `map_score(MAP01) + depth`: 18 kills×10 + 500 completion +
  (600 − 1100/35 s)×2 = 1138 time bonus + 200 depth, computed from the
  same run's `level_complete` line — banked math matches
  crates/protocol/src/scoring.rs to the point.
- Centered MidPrint quit warning and the overlay coexist (screenshot).
- **Zero render-path lines on the FIFO** across ~30k rendered frames
  (capture contained only telemetry, engine chatter, and the throttled
  MidPrint echoes). RenderOverlay must never Console.Printf.
- Non-ASCII glyphs "·" and "—" render fine in NewSmallFont.

ZScript gotchas hit (both verified by compile error, then fixed):

- A class-scope `static const` array is **not visible as an identifier**
  from method bodies on 4.14.2 ("Unknown identifier"); declare such
  arrays inside the functions that use them (gzdoom.pk3's own ui code
  does the same).
- Methods of a `play`-scope class default to play scope: a helper called
  from `RenderOverlay` must be marked `clearscope` (or `ui`) or it fails
  with "Can't call play function from ui context".

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
