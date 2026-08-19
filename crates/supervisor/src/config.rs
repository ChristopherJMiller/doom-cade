//! Supervisor configuration, read once at startup from the environment.
//!
//! Every knob has a sane default, so a bare `arcade-supervisor` on a
//! production cabinet needs no environment at all. The env contract (shared
//! with the NixOS module and the dev harness):
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `ARCADE_GZDOOM` | gzdoom binary path | `gzdoom` (from `$PATH`) |
//! | `ARCADE_IWAD` | IWAD path | `/var/lib/doom-arcade/iwad/doom2.wad` |
//! | `ARCADE_PK3` | telemetry pk3 path | *(none — `-file` omitted)* |
//! | `ARCADE_CONFIG_TEMPLATE` | pristine `gzdoom.ini` template | *(none — empty ini seeded)* |
//! | `ARCADE_RUNTIME_DIR` | tmpfs runtime dir | `/run/doom-arcade` |
//! | `ARCADE_SPOOL_DB` | spool SQLite path | `/var/lib/doom-arcade/spool.sqlite` |
//! | `ARCADE_LEADERBOARD_URL` | leaderboard base URL | `http://127.0.0.1:8080` |
//! | `ARCADE_TOKEN_FILE` | bearer-token file for `POST /v1/runs` | *(none — no auth header)* |
//! | `ARCADE_CABINET_ID` | cabinet identifier on submissions | `cab-1` |
//! | `ARCADE_ATTRACT_BIN` | attract binary path | `arcade-attract` (from `$PATH`) |
//! | `ARCADE_IWAD_UNVERIFIED` | `1` → attract shows the UNVERIFIED IWAD banner | off |
//! | `ARCADE_IWAD_SHA256` | IWAD hash recorded on submissions | `unknown` |
//! | `ARCADE_IDLE_TIMEOUT` | walk-away window, seconds | `180` |
//! | `ARCADE_STALL_TIMEOUT` | telemetry-stall window, seconds | `300` |
//! | `ARCADE_DEV` | `1` → windowed 1280×720 for gzdoom and attract | off |

use std::path::PathBuf;
use std::time::Duration;

/// Default IWAD location (SPEC §4.2).
pub const DEFAULT_IWAD: &str = "/var/lib/doom-arcade/iwad/doom2.wad";
/// Default tmpfs runtime directory (SPEC §8.2).
pub const DEFAULT_RUNTIME_DIR: &str = "/run/doom-arcade";
/// Default spool database path (SPEC §8.3).
pub const DEFAULT_SPOOL_DB: &str = "/var/lib/doom-arcade/spool.sqlite";
/// Default leaderboard base URL.
pub const DEFAULT_LEADERBOARD_URL: &str = "http://127.0.0.1:8080";
/// Default cabinet identifier.
pub const DEFAULT_CABINET_ID: &str = "cab-1";
/// Default attract binary name.
pub const DEFAULT_ATTRACT_BIN: &str = "arcade-attract";
/// Placeholder recorded when `ARCADE_IWAD_SHA256` is not provided.
pub const UNKNOWN_IWAD_SHA256: &str = "unknown";
/// Default walk-away window, in seconds (SPEC §8.4): how long gameplay may
/// stay completely static before the run is abandoned.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 180;
/// Default telemetry-stall window, in seconds: how long the telemetry
/// stream may stay silent before the run is abandoned.
pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 300;

/// Fully-resolved supervisor configuration. Cheap to clone; cloned into
/// each main-loop iteration task.
#[derive(Debug, Clone)]
pub struct Config {
    /// gzdoom binary (path or `$PATH` name).
    pub gzdoom_bin: PathBuf,
    /// IWAD path. Its readability is re-checked at every spawn (the "iwad
    /// guard") so a missing WAD sends the loop back to attract instead of
    /// crash-looping into gzdoom.
    pub iwad: PathBuf,
    /// `arcade-telemetry.pk3` path. `None` omits `-file` entirely — the
    /// run will produce no telemetry and end as `abandoned`, which is the
    /// honest outcome. Logged loudly at spawn.
    pub pk3: Option<PathBuf>,
    /// Pristine `gzdoom.ini` template copied into the session dir before
    /// every run. `None` seeds an empty ini (gzdoom fills in defaults).
    pub config_template: Option<PathBuf>,
    /// Runtime (tmpfs) dir holding `session/` and `events.fifo`.
    pub runtime_dir: PathBuf,
    /// Spool SQLite database path. `:memory:` is accepted (tests).
    pub spool_db: PathBuf,
    /// Leaderboard base URL, e.g. `http://127.0.0.1:8080`. Trailing
    /// slashes are tolerated.
    pub leaderboard_url: String,
    /// File containing the bearer token for `POST /v1/runs` (trimmed on
    /// read). `None` → requests carry no `Authorization` header.
    pub token_file: Option<PathBuf>,
    /// Cabinet identifier recorded on every submission.
    pub cabinet_id: String,
    /// Attract binary (path or `$PATH` name).
    pub attract_bin: PathBuf,
    /// Passed through to attract as `ARCADE_IWAD_UNVERIFIED=1` so it shows
    /// the "UNVERIFIED IWAD" banner (SPEC §4.2).
    pub iwad_unverified: bool,
    /// SHA-256 of the IWAD, recorded on submissions (season key).
    pub iwad_sha256: String,
    /// Walk-away window: when neither the player position nor the scoring
    /// counters have changed for this long, the run is abandoned with its
    /// partial score banked.
    pub idle_timeout: Duration,
    /// Telemetry-stall window: when no telemetry event at all arrives for
    /// this long, the run is abandoned. Longer than the ~2 s heartbeat
    /// cadence by design — intermissions pause heartbeats.
    pub stall_timeout: Duration,
    /// Explicit visitor-facing leaderboard URL, passed through to attract
    /// as `ARCADE_PUBLIC_URL` for the idle-screen display. When unset,
    /// attract derives one itself (LAN IP swapped into a loopback URL).
    pub public_url: Option<String>,
    /// Dev mode: gzdoom gets `-width 1280 -height 720 +vid_fullscreen 0`,
    /// attract gets `ARCADE_WINDOWED=1`.
    pub dev: bool,
}

impl Config {
    /// Reads the configuration from the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Builds a configuration from an arbitrary lookup function.
    ///
    /// This is the testable core of [`Config::from_env`]: tests pass a
    /// closure over a map instead of mutating the (process-global, racy)
    /// environment. Empty values are treated as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| get(key).filter(|v| !v.is_empty());
        Config {
            gzdoom_bin: get("ARCADE_GZDOOM").map_or_else(|| "gzdoom".into(), PathBuf::from),
            iwad: get("ARCADE_IWAD").map_or_else(|| DEFAULT_IWAD.into(), PathBuf::from),
            pk3: get("ARCADE_PK3").map(PathBuf::from),
            config_template: get("ARCADE_CONFIG_TEMPLATE").map(PathBuf::from),
            runtime_dir: get("ARCADE_RUNTIME_DIR")
                .map_or_else(|| DEFAULT_RUNTIME_DIR.into(), PathBuf::from),
            spool_db: get("ARCADE_SPOOL_DB").map_or_else(|| DEFAULT_SPOOL_DB.into(), PathBuf::from),
            leaderboard_url: get("ARCADE_LEADERBOARD_URL")
                .unwrap_or_else(|| DEFAULT_LEADERBOARD_URL.to_owned()),
            token_file: get("ARCADE_TOKEN_FILE").map(PathBuf::from),
            cabinet_id: get("ARCADE_CABINET_ID").unwrap_or_else(|| DEFAULT_CABINET_ID.to_owned()),
            attract_bin: get("ARCADE_ATTRACT_BIN")
                .map_or_else(|| DEFAULT_ATTRACT_BIN.into(), PathBuf::from),
            iwad_unverified: parse_bool(get("ARCADE_IWAD_UNVERIFIED")),
            iwad_sha256: get("ARCADE_IWAD_SHA256")
                .unwrap_or_else(|| UNKNOWN_IWAD_SHA256.to_owned()),
            idle_timeout: parse_secs(get("ARCADE_IDLE_TIMEOUT"), DEFAULT_IDLE_TIMEOUT_SECS),
            stall_timeout: parse_secs(get("ARCADE_STALL_TIMEOUT"), DEFAULT_STALL_TIMEOUT_SECS),
            public_url: get("ARCADE_PUBLIC_URL"),
            dev: parse_bool(get("ARCADE_DEV")),
        }
    }
}

/// Boolean env parsing: the contract says `=1`, but `true`/`yes` (any
/// case) are also accepted. Anything else — including unset — is `false`.
fn parse_bool(value: Option<String>) -> bool {
    value.is_some_and(|v| {
        let v = v.trim().to_ascii_lowercase();
        v == "1" || v == "true" || v == "yes"
    })
}

/// Duration env parsing: a positive integer number of seconds. Anything
/// else — unset, zero, negative, or garbage — falls back to the default,
/// so a typo in a deployed env file degrades to the shipped behavior
/// instead of arming an instant (or never-firing) timeout.
fn parse_secs(value: Option<String>, default_secs: u64) -> Duration {
    let secs = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(default_secs);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg_from(pairs: &[(&str, &str)]) -> Config {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_when_env_is_empty() {
        let cfg = cfg_from(&[]);
        assert_eq!(cfg.gzdoom_bin, PathBuf::from("gzdoom"));
        assert_eq!(cfg.iwad, PathBuf::from(DEFAULT_IWAD));
        assert_eq!(cfg.pk3, None);
        assert_eq!(cfg.config_template, None);
        assert_eq!(cfg.runtime_dir, PathBuf::from(DEFAULT_RUNTIME_DIR));
        assert_eq!(cfg.spool_db, PathBuf::from(DEFAULT_SPOOL_DB));
        assert_eq!(cfg.leaderboard_url, DEFAULT_LEADERBOARD_URL);
        assert_eq!(cfg.token_file, None);
        assert_eq!(cfg.cabinet_id, DEFAULT_CABINET_ID);
        assert_eq!(cfg.attract_bin, PathBuf::from(DEFAULT_ATTRACT_BIN));
        assert!(!cfg.iwad_unverified);
        assert_eq!(cfg.iwad_sha256, UNKNOWN_IWAD_SHA256);
        assert_eq!(cfg.idle_timeout, Duration::from_secs(180));
        assert_eq!(cfg.stall_timeout, Duration::from_secs(300));
        assert!(!cfg.dev);
    }

    #[test]
    fn every_override_is_honored() {
        let cfg = cfg_from(&[
            ("ARCADE_GZDOOM", "/opt/gzdoom/bin/gzdoom"),
            ("ARCADE_IWAD", "/tmp/freedoom2.wad"),
            ("ARCADE_PK3", "/nix/store/x/arcade-telemetry.pk3"),
            ("ARCADE_CONFIG_TEMPLATE", "/etc/arcade/gzdoom.ini"),
            ("ARCADE_RUNTIME_DIR", "/tmp/arcade-run"),
            ("ARCADE_SPOOL_DB", "/tmp/spool.sqlite"),
            ("ARCADE_LEADERBOARD_URL", "http://boards.lan:9000"),
            ("ARCADE_TOKEN_FILE", "/run/secrets/token"),
            ("ARCADE_CABINET_ID", "cab-basement"),
            ("ARCADE_ATTRACT_BIN", "/opt/attract"),
            ("ARCADE_IWAD_UNVERIFIED", "1"),
            ("ARCADE_IWAD_SHA256", "deadbeef"),
            ("ARCADE_IDLE_TIMEOUT", "60"),
            ("ARCADE_STALL_TIMEOUT", "900"),
            ("ARCADE_DEV", "1"),
        ]);
        assert_eq!(cfg.gzdoom_bin, PathBuf::from("/opt/gzdoom/bin/gzdoom"));
        assert_eq!(cfg.iwad, PathBuf::from("/tmp/freedoom2.wad"));
        assert_eq!(
            cfg.pk3,
            Some(PathBuf::from("/nix/store/x/arcade-telemetry.pk3"))
        );
        assert_eq!(
            cfg.config_template,
            Some(PathBuf::from("/etc/arcade/gzdoom.ini"))
        );
        assert_eq!(cfg.runtime_dir, PathBuf::from("/tmp/arcade-run"));
        assert_eq!(cfg.spool_db, PathBuf::from("/tmp/spool.sqlite"));
        assert_eq!(cfg.leaderboard_url, "http://boards.lan:9000");
        assert_eq!(cfg.token_file, Some(PathBuf::from("/run/secrets/token")));
        assert_eq!(cfg.cabinet_id, "cab-basement");
        assert_eq!(cfg.attract_bin, PathBuf::from("/opt/attract"));
        assert!(cfg.iwad_unverified);
        assert_eq!(cfg.iwad_sha256, "deadbeef");
        assert_eq!(cfg.idle_timeout, Duration::from_secs(60));
        assert_eq!(cfg.stall_timeout, Duration::from_secs(900));
        assert!(cfg.dev);
    }

    #[test]
    fn timeout_parsing_accepts_positive_seconds() {
        let cfg = cfg_from(&[
            ("ARCADE_IDLE_TIMEOUT", " 45 "),
            ("ARCADE_STALL_TIMEOUT", "1"),
        ]);
        assert_eq!(cfg.idle_timeout, Duration::from_secs(45));
        assert_eq!(cfg.stall_timeout, Duration::from_secs(1));
    }

    #[test]
    fn timeout_parsing_falls_back_on_invalid_values() {
        for bad in ["0", "-5", "abc", "1.5", "60s", ""] {
            let cfg = cfg_from(&[("ARCADE_IDLE_TIMEOUT", bad), ("ARCADE_STALL_TIMEOUT", bad)]);
            assert_eq!(
                cfg.idle_timeout,
                Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
                "input: {bad:?}"
            );
            assert_eq!(
                cfg.stall_timeout,
                Duration::from_secs(DEFAULT_STALL_TIMEOUT_SECS),
                "input: {bad:?}"
            );
        }
    }

    #[test]
    fn bool_parsing() {
        for truthy in ["1", "true", "TRUE", "yes", " 1 "] {
            assert!(cfg_from(&[("ARCADE_DEV", truthy)]).dev, "input: {truthy:?}");
        }
        for falsy in ["0", "false", "no", "2", "on", ""] {
            assert!(!cfg_from(&[("ARCADE_DEV", falsy)]).dev, "input: {falsy:?}");
        }
    }

    #[test]
    fn empty_values_fall_back_to_defaults() {
        let cfg = cfg_from(&[("ARCADE_IWAD", ""), ("ARCADE_CABINET_ID", "")]);
        assert_eq!(cfg.iwad, PathBuf::from(DEFAULT_IWAD));
        assert_eq!(cfg.cabinet_id, DEFAULT_CABINET_ID);
    }
}
