# DOOM Arcade Cabinet — Implementation Spec

**Target audience:** an implementing agent with repo write access.
**Status:** design frozen; open questions listed in §13 must be resolved empirically during implementation, not guessed at.

---

## 1. Goal

A single-purpose x86 desktop that powers on directly into an arcade-style DOOM experience: attract screen → 3-letter initials → a fixed 5-map run on one life → score submitted to a leaderboard → back to attract. No shell, no desktop, no way out without a keyboard and physical access. The entire machine is declared in a NixOS flake in a GitHub repo and self-updates from that repo.

**Non-goals:** multiplayer, save games, mod loading at runtime, a general-purpose emulator frontend, touchscreen support.

---

## 2. Architecture

```
                          ┌──────────────────────────────────────┐
  boot → autologin(doom) →│  cage (Wayland kiosk compositor)     │
                          │    └── arcade-supervisor (Rust)      │
                          │          ├── spawns arcade-attract   │  egui fullscreen:
                          │          │                           │  leaderboard + initials entry
                          │          └── spawns gzdoom           │  the actual run
                          └──────────────────────────────────────┘
                                     │ tails event log (JSONL)
                                     ▼
                          ┌──────────────────────┐    HTTP     ┌────────────────────┐
                          │ local spool (SQLite) │ ──────────► │ leaderboard service │
                          │ survives offline     │             │ axum + SQLite      │
                          └──────────────────────┘             └────────────────────┘
```

Four deliverables:

| # | Component | Language | Path in repo |
|---|-----------|----------|--------------|
| 1 | `arcade-telemetry.pk3` — GZDoom mod emitting run events + defining the map rotation | ZScript | `pk3/` |
| 2 | `arcade-supervisor` — session loop, event ingest, score submission, offline spool | Rust | `crates/supervisor/` |
| 3 | `arcade-attract` — fullscreen attract/leaderboard/initials UI | Rust + egui | `crates/attract/` |
| 4 | `leaderboard` — score API + web view | Rust + axum + SQLite | `crates/leaderboard/` |
| 5 | NixOS module + flake tying it together | Nix | `flake.nix`, `nix/` |

The supervisor and attract app are separate processes deliberately: GZDoom cannot render a live leaderboard (ZScript has no filesystem or network access), so the out-of-game UI must be its own window. `cage` stacks child windows fullscreen and focuses the newest, so alternating between them works without a window manager.

---

## 3. Repository layout

```
.
├── flake.nix
├── flake.lock
├── nix/
│   ├── module.nix           # the NixOS module (services.doom-arcade)
│   ├── kiosk.nix            # cage, autologin, boot silencing, input lockdown
│   ├── hosts/
│   │   └── cabinet.nix      # the physical machine: hardware-configuration, hostname, net
│   └── pkgs/
│       ├── telemetry-pk3.nix
│       └── overlay.nix
├── crates/
│   ├── supervisor/
│   ├── attract/
│   ├── leaderboard/
│   └── protocol/            # shared: event enum, score payload, serde types
├── pk3/
│   ├── zscript.txt
│   ├── zscript/arcade/handler.zs
│   ├── MAPINFO.txt
│   └── CVARINFO.txt
├── assets/
│   └── config/gzdoom.ini    # pristine config template, copied fresh every run
└── .github/workflows/ci.yml
```

**The IWAD is never committed.** Add `*.wad` to `.gitignore` and add a CI step that fails the build if any file over 1 MB with a WAD magic header (`IWAD`/`PWAD`) is present in the tree.

---

## 4. Game layer

### 4.1 Engine

GZDoom, pinned from nixpkgs. Chosen over dsda-doom because ZScript gives a live event stream and lets us define arcade rules (single life, fixed rotation) declaratively.

### 4.2 IWAD provisioning

`doom2.wad` is supplied out of band and is **not** in the repo or the Nix store.

- Expected location: `/var/lib/doom-arcade/iwad/doom2.wad`, mode `0444`, owner `doom`.
- Create the directory via `systemd.tmpfiles.rules`.
- A `doom-arcade-preflight.service` (ordered `Before=cage-tty1.service`, `RequiredBy` it) computes the SHA-256 and compares it against `services.doom-arcade.iwadSha256` declared in the host config.
- On mismatch or absence: log loudly, and fall back to the `freedoom` package from nixpkgs so the machine still boots into something playable. The attract screen must display a visible "UNVERIFIED IWAD" banner in this state so it is never silently shipped.

> Variants of `doom2.wad` are not interchangeable — the 1.9 retail release, the BFG Edition release, and the Unity re-release differ in map data and lump contents, and the BFG Edition altered MAP31. **Compute the hash of the specific copy being used and pin that**; do not hardcode a hash from any external list. The hash is also recorded on every score submission (§7.2) so a WAD swap starts a new season instead of corrupting the existing board.

### 4.3 The run: MAPINFO

Define a custom episode in `pk3/MAPINFO.txt` with a fixed 5-map rotation and explicit `next` chaining, so the run has a defined end rather than continuing into MAP06. Proposed rotation (tune after playtesting for pacing — target 12–18 minutes for a competent player):

`MAP01 → MAP02 → MAP03 → MAP07 → MAP08 → (end run)`

Requirements:
- `defaultmap` sets `nointermission` off (the intermission screen is fine and reads as arcade-appropriate).
- The final map's `next` points to a custom end sequence that emits the run-complete event and returns to title.
- Skill is locked: supervisor passes `-skill 3` (Hurt Me Plenty). Do not expose skill selection.
- No saves: pass `+set saveloadconfirmation 0` and remove save/load from the menu via `MENUDEF` if the menu is reachable at all. Preferably make the menu unreachable (§9).

### 4.4 One life

DOOM has no lives concept. Implement as: on `PlayerDied`, the handler emits a `player_died` event; the supervisor waits 3 seconds (death animation and gib satisfaction) then sends `SIGTERM` to gzdoom. Do not attempt to force a quit from inside ZScript — process control belongs to the supervisor.

### 4.5 Telemetry handler

`pk3/zscript/arcade/handler.zs` — a `StaticEventHandler` that emits one JSON object per line via `Console.Printf`.

Events and the fields each carries:

| Event | Hook | Payload |
|---|---|---|
| `run_start` | `NewGame` | `session`, `initials`, `skill`, `ts` |
| `level_enter` | `WorldLoaded` (skip if `e.IsSaveGame`) | `session`, `map`, `level_name`, `ts` |
| `level_complete` | `WorldUnloaded` | `session`, `map`, `kills`, `total_monsters`, `secrets`, `total_secrets`, `items`, `total_items`, `maptime_tics` |
| `player_died` | `PlayerDied` | `session`, `map`, `kills`, `secrets`, `maptime_tics` |
| `run_complete` | final-map unload / end sequence | `session`, `total_maptime_tics` |

Stat fields come from `LevelLocals`: `Level.killed_monsters`, `Level.total_monsters`, `Level.found_secrets`, `Level.total_secrets`, `Level.found_items`, `Level.total_items`, `Level.maptime` (tics; 35 tics = 1 second), `Level.MapName`, `Level.LevelName`. Verify each name against the GZDoom version pinned in the flake before relying on it — the ZScript API does shift between releases.

Line format, one per line, prefixed with a fixed sentinel so the supervisor can discriminate engine chatter from telemetry:

```
ARCADE_EVT {"v":1,"event":"level_complete","session":"...","map":"MAP01",...}
```

`session` and `initials` are read from user cvars declared in `CVARINFO.txt` (`arcade_session`, `arcade_initials`, both `user string`), set by the supervisor on the command line with `+set`. Escape both defensively — treat them as untrusted when building the JSON string.

### 4.6 Event transport

**Preferred:** the supervisor creates a named pipe and passes `+logfile /run/doom-arcade/events.fifo`, then reads lines from it. This isolates telemetry from stdout buffering and from the engine's own logging.

**Fallback:** capture gzdoom's stdout directly.

The supervisor must tolerate either: parse only lines beginning with `ARCADE_EVT `, ignore everything else, never crash on a malformed line. Confirm empirically which transport actually works on the pinned GZDoom build (§13).

---

## 5. Scoring

DOOM has no native score, so define one. Formula (constants live in a single Rust module, easily tuned):

```
map_score  = kills * 10
           + secrets * 100
           + items * 5
           + completion_bonus (500 if map completed)
           + time_bonus (max(0, 600 - seconds_on_map) * 2, only if completed)

run_score  = sum(map_score for each map) + depth_bonus (200 * maps_completed)
```

Death ends the run; the partial map's kills and secrets still count, but no completion or time bonus. Time bonus rewards moving without punishing exploration into negative territory.

Store the raw component stats alongside the computed score so the formula can be recomputed retroactively without replaying anything. Record `scoring_version` on every row; bump it when constants change, and let the leaderboard filter by it.

---

## 6. Leaderboard categories

A single number is a weak arcade board. Ship five, cycled on the attract screen:

1. **High score** — `run_score`, all runs
2. **Deepest run** — maps completed, tiebreak on score
3. **Fastest clear** — total time, completed runs only
4. **Most kills** — total kills in a run
5. **Secret hunter** — total secrets found in a run

All boards are scoped to `(iwad_sha256, scoring_version, map_rotation_id)`. Changing any of those starts a new season.

---

## 7. Leaderboard service

### 7.1 Deployment

`axum` + `sqlx` + SQLite, single binary. Runs as a systemd service, either on the cabinet itself (`127.0.0.1:8080`) or on the user's existing Kubernetes cluster. Make the endpoint a config option; the cabinet must work fully offline either way (§8.3).

### 7.2 Schema

```sql
CREATE TABLE runs (
  id              INTEGER PRIMARY KEY,
  session         TEXT NOT NULL UNIQUE,   -- UUID from supervisor; idempotency key
  initials        TEXT NOT NULL,          -- exactly 3 chars, [A-Z0-9]
  cabinet_id      TEXT NOT NULL,
  started_at      TEXT NOT NULL,          -- RFC3339
  ended_at        TEXT NOT NULL,
  end_reason      TEXT NOT NULL,          -- 'death' | 'complete' | 'abandoned'
  maps_completed  INTEGER NOT NULL,
  kills           INTEGER NOT NULL,
  secrets         INTEGER NOT NULL,
  items           INTEGER NOT NULL,
  total_tics      INTEGER NOT NULL,
  run_score       INTEGER NOT NULL,
  iwad_sha256     TEXT NOT NULL,
  scoring_version INTEGER NOT NULL,
  map_rotation_id TEXT NOT NULL
);

CREATE TABLE run_maps (
  run_id     INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  seq        INTEGER NOT NULL,
  map        TEXT NOT NULL,
  kills      INTEGER NOT NULL, total_monsters INTEGER NOT NULL,
  secrets    INTEGER NOT NULL, total_secrets  INTEGER NOT NULL,
  items      INTEGER NOT NULL, total_items    INTEGER NOT NULL,
  tics       INTEGER NOT NULL,
  completed  INTEGER NOT NULL,
  map_score  INTEGER NOT NULL,
  PRIMARY KEY (run_id, seq)
);

CREATE INDEX idx_season_score ON runs(iwad_sha256, scoring_version, map_rotation_id, run_score DESC);
```

### 7.3 API

| Method | Path | Notes |
|---|---|---|
| `POST` | `/v1/runs` | Submit a completed run. Body = full run + per-map array. **Idempotent on `session`** — re-submitting returns 200 with the existing record, never a duplicate. |
| `GET` | `/v1/boards/{category}` | `?limit=10&season=<iwad_sha>:<ver>:<rotation>`. Defaults to current season. |
| `GET` | `/v1/boards` | All five boards in one response — this is what the attract app polls. |
| `GET` | `/healthz` | Liveness. |
| `GET` | `/` | Minimal HTML leaderboard view for viewing off-cabinet. |

Auth: shared bearer token on `POST`, supplied via `EnvironmentFile` from a sops-nix secret. `GET` may be open on a home LAN. Rate-limit `POST` per cabinet regardless.

Server-side validation: recompute `run_score` from the submitted per-map stats and reject on mismatch; clamp implausible values (kills exceeding `total_monsters`, negative tics, initials not matching `^[A-Z0-9]{3}$`).

---

## 8. Supervisor

Single Rust binary, `tokio`. It is the only long-lived process under cage.

### 8.1 Main loop

```
loop {
  1. purge and re-seed the runtime config dir from the read-only template
  2. spawn arcade-attract; wait for it to exit with either
       Initials(String)  → continue
       Timeout           → (idle; loop back to attract, which shows demo/attract reel)
  3. mint session UUID
  4. spawn gzdoom with the arg vector (§8.2)
  5. consume the event stream; maintain in-memory RunState
  6. on player_died: sleep 3s, SIGTERM gzdoom
     on run_complete: SIGTERM gzdoom
     on unexpected exit: mark end_reason='abandoned', keep partial stats
  7. compute run_score; write to local spool DB
  8. kick the submitter task (async, non-blocking)
}
```

The loop must never terminate. Panics in run handling are caught and logged; the loop restarts at step 1. A supervisor crash is caught by `Restart=always` on the systemd unit as a second line of defense.

### 8.2 GZDoom invocation

```
gzdoom
  -iwad   /var/lib/doom-arcade/iwad/doom2.wad
  -file   <nix-store>/share/arcade-telemetry.pk3
  -config /run/doom-arcade/session/gzdoom.ini
  -savedir /run/doom-arcade/session/saves
  -skill  3
  -warp   1
  +set arcade_session <uuid>
  +set arcade_initials <ABC>
  +set vid_fps 0
  +logfile /run/doom-arcade/events.fifo
```

`/run/doom-arcade/session/` is tmpfs, wiped and re-seeded from `assets/config/gzdoom.ini` before every run. This guarantees no player inherits another's rebinds, resolution changes, autosaves, or console state.

### 8.3 Offline spool

The local SQLite spool is the source of truth on the cabinet. Every run is written there first, then a background task submits pending runs with exponential backoff (cap ~5 min) and marks them submitted on 2xx. Because `POST /v1/runs` is idempotent on `session`, retries after ambiguous failures are safe. The cabinet is fully playable with the network down; scores land when it returns.

### 8.4 Watchdog

If gzdoom produces no event and no output for 20 minutes, treat the run as abandoned, kill it, and return to attract. Prevents a walked-away player from occupying the cabinet indefinitely.

---

## 9. Kiosk layer (NixOS)

`nix/kiosk.nix`:

```nix
{
  services.getty.autologinUser = "doom";
  users.users.doom = { isNormalUser = true; extraGroups = [ "video" "input" ]; };

  services.cage = {
    enable = true;
    user = "doom";
    program = "${pkgs.doom-arcade}/bin/arcade-supervisor";
  };
  systemd.services."cage-tty1".serviceConfig = {
    Restart = "always";
    RestartSec = 2;
  };

  boot.loader.timeout = 0;
  boot.kernelParams = [ "quiet" "loglevel=0" "vt.global_cursor_default=0" "systemd.show_status=0" ];
  boot.plymouth.enable = true;

  services.logind.extraConfig = ''
    HandlePowerKey=ignore
    HandleSuspendKey=ignore
    HandleLidSwitch=ignore
  '';
  systemd.targets."ctrl-alt-del".enable = false;

  # No way to escape to a shell from the cabinet itself
  services.openssh = { enable = true; settings.PasswordAuthentication = false; };
}
```

Additional requirements:

- **Impermanence.** Use the `impermanence` module with a tmpfs root; persist only `/var/lib/doom-arcade` (IWAD + spool), `/etc/ssh` host keys, and `/nix`. State cannot accumulate.
- **Self-update.** GitOps-style pull from the GitHub repo (comin), with an overnight-friendly cadence. A reboot mid-run is acceptable; a reboot mid-attract is invisible.
- **No TTY switching.** Ensure only tty1 is spawned.
- **Audio.** PipeWire, fixed volume, no mixer UI. Set a sane cap so the cabinet cannot be turned into a nuisance.

---

## 10. Controls

USB encoder (Zero Delay / Xin-Mo class) configured in **keyboard mode**, so the panel presents as a plain HID keyboard and needs no joystick axis mapping. Bindings baked into the pristine `gzdoom.ini`:

| Panel input | Key | DOOM action |
|---|---|---|
| Joystick up/down/left/right | Arrow keys | Forward / back / turn |
| Button 1 | `Ctrl` | Fire |
| Button 2 | `Space` | Use / open |
| Button 3 | `,` | Strafe left |
| Button 4 | `.` | Strafe right |
| Button 5 | `[` | Previous weapon |
| Button 6 | `]` | Next weapon |
| Start | `Enter` | Confirm / start run |
| (none) | — | Run is **always on** via `cl_run 1`; do not spend a button on it |

Menu access must be suppressed during a run — rebind or unbind `Esc` in the pristine config so a player cannot reach settings, quit, or save. The attract app handles all out-of-game interaction.

Initials entry in `arcade-attract`: joystick up/down cycles the character, Button 1 confirms and advances, Button 2 backspaces, three characters then auto-submits. Classic arcade behavior; charset `A–Z` then `0–9`.

---

## 11. Attract app

`arcade-attract`, egui via `eframe` (glow backend is sufficient; verify Wayland-under-cage behavior early). Fullscreen, no decorations.

States:

1. **Idle reel** — cycles the five leaderboards on ~8-second dwell, interleaved with a "PRESS START" prompt. Fetches `/v1/boards` on entry and every 60s; renders cached data with a subtle offline indicator on failure.
2. **Initials entry** — entered on Start. 20-second inactivity timeout returns to idle.
3. **Exit** — prints the chosen initials to stdout in a machine-readable form and exits 0; supervisor reads it.

Also displays the "UNVERIFIED IWAD" banner when preflight fell back to Freedoom.

Visual direction: match the DOOM status-bar palette — heavy reds, browns, and off-white — with a bitmap-style face. Do not attempt to extract or bundle fonts or graphics from the IWAD; use a freely licensed pixel font and original assets so the repo stays distributable.

---

## 12. Development workflow

- `nix run .#dev` — launches the full loop windowed at 1280×720 against Freedoom, with a local ephemeral leaderboard. Must work on a normal laptop with no cabinet hardware.
- `nix run .#leaderboard` — service alone, seeded with fake runs, for UI iteration.
- `cargo test -p protocol` — round-trip tests for the event parser, including malformed and hostile input (partial lines, non-JSON, injected sentinels, oversized fields).
- **Replay fixture:** commit a recorded JSONL event stream from a real run to `crates/supervisor/tests/fixtures/`. Scoring and state-machine tests run against it with no engine required — this is the main defense against regressions in run handling.
- `nixos-rebuild build-vm` for testing the kiosk layer without touching the physical machine.
- CI: `nix flake check`, `cargo clippy -- -D warnings`, `cargo test`, plus the no-WAD-committed check.

---

## 13. Open questions — resolve empirically, do not assume

1. **Does `Console.Printf` output reach `-logfile`, and is a named FIFO usable as that target on the pinned GZDoom build?** If not, fall back to stdout capture. Settle this before writing any downstream parsing — it determines the transport.
2. **Exact `LevelLocals` field names on the pinned GZDoom version.** Confirm each stat field compiles before building the handler out.
3. **Does `WorldUnloaded` fire on the final map's exit**, and with stats still populated? If not, emit `run_complete` from a `MAPINFO`-defined end sequence instead.
4. **egui/eframe under cage on Wayland** — confirm fullscreen, keyboard focus, and window stacking against gzdoom before committing to the two-process design.
5. **Encoder key ordering.** Cheap encoders vary in their factory key map; capture actual scancodes with `evtest` on the real hardware and derive the config from that rather than from a datasheet.

---

## 14. Milestones

| M | Deliverable | Done when |
|---|---|---|
| M1 | Telemetry spike | A hand-run gzdoom emits parseable `ARCADE_EVT` lines for enter/complete/died. §13.1–13.3 answered. |
| M2 | Supervisor + scoring | Headless loop plays a run end-to-end, computes a score, writes it to the local spool. Replay fixture tests pass. |
| M3 | Leaderboard service | API up, idempotent submission verified, HTML view renders all five boards. |
| M4 | Attract app | Full loop runs windowed on a dev machine: attract → initials → run → score → attract. |
| M5 | Kiosk | `nixos-rebuild build-vm` boots straight into attract with no visible OS. |
| M6 | Cabinet | Physical machine flashed, real IWAD provisioned and hash-pinned, encoder mapped, autoUpgrade verified against the repo. |

Ship M1–M4 before touching NixOS kiosk config. The game and scoring layer carries all the risk; the kiosk layer is well-trodden and can be assembled quickly once there is something worth booting into.
