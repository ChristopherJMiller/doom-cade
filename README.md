# doom-cab

A single-purpose x86 machine that powers on directly into an arcade-style DOOM
experience: attract screen → 3-letter initials → a fixed 5-map run on one life →
score submitted to a leaderboard → back to attract. No shell, no desktop, no way
out without a keyboard and physical access. The whole cabinet — kiosk OS, game
supervisor, attract UI, telemetry mod, and leaderboard service — is declared in
a NixOS flake in this repo, and the machine self-updates from it.

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

## Quick start (dev loop on a laptop)

No cabinet hardware needed:

```sh
nix run .#dev           # full loop, windowed 1280×720, Freedoom,
                        # ephemeral seeded leaderboard on 127.0.0.1:8080
nix run .#leaderboard   # leaderboard service alone, seeded with fake runs —
                        # for iterating on the web view / attract screen
nix develop -c cargo test --workspace
```

`#dev` runs the same supervisor → attract → gzdoom loop the cabinet runs, just
windowed and against Freedoom, so the attract screen shows the UNVERIFIED IWAD
banner — that is expected.

## Testing the kiosk in a VM

```sh
nix run .#vm
```

What to expect: the VM boots straight into the attract screen with no visible
OS — no login prompt, no desktop, just leaderboards cycling and PRESS START.
The UNVERIFIED IWAD banner is shown because the VM runs Freedoom, not a real
`doom2.wad`. To get a shell inside, `ssh -p 2222 <host>` (SSH is the only way
in; there is deliberately no escape from the cabinet itself).

## Loading your DOOM II WAD

The IWAD is copyrighted and is **never** committed to this repo or copied into
the Nix store (CI enforces this). You provide your own `doom2.wad` out of band;
it lives at `/var/lib/doom-arcade/iwad/doom2.wad` on the cabinet.

**Option A — thumb drive (no laptop required).** Put `doom2.wad` on a FAT- or
ext-formatted USB stick, at the root of the stick or up to two directories
deep. Plug it into the cabinet. The WAD auto-imports within a few seconds; then
check the import log for the hash to pin:

```sh
journalctl -u 'doom-arcade-wad-import@*'
```

The log prints the SHA-256 of the imported WAD — set that value as
`services.doom-arcade.iwadSha256` in the host config and push.

**Option B — over SSH.** (Add `-p 2222` / `scp -P 2222` when targeting the
test VM.)

```sh
scp doom2.wad cabinet:/tmp/
ssh cabinet doom-arcade-import-wad /tmp/doom2.wad
```

Why the hash pin: variants of `doom2.wad` (1.9 retail, BFG Edition, Unity
re-release) differ in map data, so the cabinet verifies the SHA-256 of your
specific copy at boot and falls back to Freedoom — with a loud UNVERIFIED IWAD
banner — on any mismatch. The hash is also recorded on every score submission
as part of the season key, so swapping WADs starts a fresh leaderboard season
instead of corrupting the existing board (see SPEC §4.2, §6).

## Deploying the cabinet

Deployment is GitOps via [comin](https://github.com/nlewo/comin): the cabinet
polls this repo's `main` branch and rebuilds itself when it changes. Deploying
a change *is* pushing to `main` — there is no other deploy step. One-time
setup: replace the placeholder repo URL in `nix/hosts/cabinet.nix` with this
repo's actual URL so the cabinet knows what to poll. A reboot mid-run is
acceptable; a reboot mid-attract is invisible.

## Controls

USB encoder (Zero Delay / Xin-Mo class) in keyboard mode; bindings are baked
into the pristine `assets/config/gzdoom.ini` copied fresh for every run:

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
| (none) | — | Run is **always on** via `cl_run 1`; no button spent on it |

`Esc` is suppressed and the menu is unreachable; the attract app handles all
out-of-game interaction. Initials entry: joystick up/down cycles `A–Z` then
`0–9`, Button 1 confirms and advances, Button 2 backspaces, three characters
auto-submit.

## Scoring

DOOM has no native score, so the cabinet defines one (constants live in
`crates/protocol/src/scoring.rs`):

```
map_score  = kills * 10
           + secrets * 100
           + items * 5
           + completion_bonus (500 if map completed)
           + time_bonus (max(0, 600 - seconds_on_map) * 2, only if completed)

run_score  = sum(map_score for each map) + depth_bonus (200 * maps_completed)
```

Death ends the run; the partial map's kills, secrets, and items still count,
but there is no completion or time bonus. Raw per-map stats are stored
alongside every score so the formula can be recomputed retroactively; a
`scoring_version` bump starts a new season. Five boards cycle on the attract
screen: **High score**, **Deepest run**, **Fastest clear**, **Most kills**,
and **Secret hunter**.

## Repo layout

```
.
├── flake.nix                  # dev shell, packages, apps (#dev, #leaderboard, #vm)
├── nix/
│   ├── module.nix             # the services.doom-arcade NixOS module
│   ├── kiosk.nix              # cage, autologin, boot silencing, input lockdown
│   ├── hosts/cabinet.nix      # the physical machine (set the repo URL here)
│   └── pkgs/                  # telemetry-pk3 package + overlay
├── crates/
│   ├── protocol/              # shared wire types, event parser, scoring — source of truth
│   ├── supervisor/            # arcade-supervisor: session loop, spool, submission
│   ├── attract/               # arcade-attract: egui attract / initials UI
│   └── leaderboard/           # arcade-leaderboard: axum score API + web view
├── pk3/                       # arcade-telemetry.pk3 sources (ZScript, MAPINFO)
├── assets/config/gzdoom.ini   # pristine per-run config template
├── scripts/check-no-wad.sh    # CI guard: no WADs in the tree, ever
└── docs/SPEC.md               # the frozen design
```

[`docs/SPEC.md`](docs/SPEC.md) is the authority on all of the above; when this
README and the spec disagree, the spec wins.
