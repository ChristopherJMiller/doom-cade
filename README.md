# doom-cab

A single-purpose x86 machine that powers on directly into an arcade-style DOOM
experience: attract screen → a fixed 5-map run on one life → 3-letter initials
claim the score (after the run, arcade-style) → leaderboard → back to attract.
No shell, no desktop, no way out without a keyboard and physical access. The whole cabinet — kiosk OS, game
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

## Installing on real hardware

The repo builds a bootable installer ISO that installs the cabinet onto a
blank machine with **no network required** — the ISO carries the complete
cabinet system, this repo's source, and all flake inputs.

**Required pre-step:** set your SSH public key in `nix/hosts/cabinet.nix`
(`users.users.doom.openssh.authorizedKeys.keys`) *before* building the ISO.
The installed cabinet is key-only SSH with no console escape — without a key
baked in it is unreachable except by reinstalling. The installer prints a red
warning if the list is empty, but by then you have to rebuild the ISO anyway.

```sh
nix build .#iso          # ≈ 2.2 GB — it embeds the whole cabinet closure
```

Flash it to a USB stick — **double-check the device**: `lsblk` first, and be
sure `/dev/sdX` is the stick, not your disk, because `dd` will destroy
whatever it points at:

```sh
sudo dd if=result/iso/*.iso of=/dev/sdX bs=4M status=progress oflag=sync
```

Boot the target machine from the stick (UEFI). You land on a root console;
run:

```sh
doom-cade-install
```

The installer shows `lsblk` of the machine's disks, asks for the target
device (e.g. `/dev/nvme0n1`), then makes you re-type the device path and type
`YES` before wiping anything. It then partitions declaratively via
[disko](https://github.com/nix-community/disko) (1G ESP + btrfs with
`nix`/`persist` subvolumes; root is tmpfs) and runs the offline install —
expect a few minutes of flake evaluation followed by the store copy.

No ethernet at the cabinet? Run `nmtui` on the installer console to join
Wi-Fi (optional — the install itself is fully offline). Any connection you
set up is carried over to the installed cabinet automatically; to change
networks later, `ssh doom@<cabinet>` and run `nmtui` there.

**Changing Wi-Fi later (or recovering a cabinet that lost its network).**
Carry-over only covers day 1 — if the Wi-Fi password rotates, the cabinet
drops offline and SSH goes with it, so there is a USB recovery path
mirroring the WAD import: put either a `doom-cade-wifi.txt` (lines
`ssid=...` / `psk=...`, optional `hidden=1`) or a full NetworkManager
`*.nmconnection` keyfile (for enterprise/EAP setups) on a USB stick and
plug it in. It imports within a few seconds and reconnects;
`journalctl -u 'doom-cade-wifi-import@*'` shows what happened. The honest
tradeoff: anyone with physical USB access can repoint the cabinet's
network — consistent with the rest of the machine, where physical access
already means power, disk, and the WAD slot.

First boot after removing the stick goes straight into the kiosk: attract
screen, leaderboards, PRESS START — running the bundled Freedoom with the
UNVERIFIED IWAD banner until you plug in a USB stick containing `doom2.wad`
(next section). After the import, `journalctl -u 'doom-arcade-wad-import@*'`
prints the SHA-256 to pin as `services.doom-arcade.iwadSha256`.

The attract screen's top-right corner shows the scoreboard URL — the
cabinet's LAN IP on port 8080, refreshed as DHCP moves it — for anyone on
the office subnet to open on their phone. `http://doom-cab.local:8080` works
too where the network permits mDNS. Browsing is open; submitting scores
requires the cabinet's locally-minted token, so the board can't be forged
from a laptop.

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
polls this repo's `main` branch
([ChristopherJMiller/doom-cade](https://github.com/ChristopherJMiller/doom-cade),
configured in `nix/hosts/cabinet.nix`) and rebuilds itself when it changes.
Deploying a change *is* pushing to `main` — there is no other deploy step.
A reboot mid-run is acceptable; a reboot mid-attract is invisible.

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
| Start | `Enter` | Confirm / start run; **hold ~3 s in-game to end the run early** |
| (none) | — | Run is **always on** via `cl_run 1`; no button spent on it |

`Esc` is suppressed and the menu is unreachable; the attract app handles all
out-of-game interaction. Initials entry appears after the run with your score
on screen: joystick up/down cycles `A–Z` then `0–9`, Button 1 confirms and
advances, Button 2 backspaces, three characters auto-submit; 20 s idle
auto-submits padded with `A`.

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
but there is no completion or time bonus. The same partial credit applies to
early exits: hold Start ~3 seconds to end a run deliberately, or just walk
away — in-game heartbeats track progress, so a run abandoned mid-map is
detected within a few minutes and banked with whatever was earned (fastest
clear is the one board reserved for fully completed runs). Raw per-map stats
are stored
alongside every score so the formula can be recomputed retroactively; a
`scoring_version` bump starts a new season. Five boards cycle on the attract
screen: **High score**, **Deepest run**, **Fastest clear**, **Most kills**,
and **Secret hunter**.

## Repo layout

```
.
├── flake.nix                  # dev shell, packages (#iso), apps (#dev, #leaderboard, #vm)
├── nix/
│   ├── module.nix             # the services.doom-arcade NixOS module
│   ├── kiosk.nix              # cage, autologin, boot silencing, input lockdown
│   ├── wad-import.nix         # thumb-drive doom2.wad auto-import
│   ├── wifi-import.nix        # thumb-drive Wi-Fi config import / recovery
│   ├── disk-layout.nix        # disko layout: ESP + btrfs nix/persist, tmpfs root
│   ├── hosts/cabinet.nix      # the physical machine (set your SSH key here)
│   ├── hosts/vm.nix           # the #vm test machine
│   ├── hosts/installer.nix    # the bootable installer ISO (nix build .#iso)
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
