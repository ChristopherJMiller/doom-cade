//! Shared protocol types for the DOOM arcade cabinet.
//!
//! This crate is the single source of truth for every type that crosses a
//! process or network boundary in the system (see `docs/SPEC.md`):
//!
//! - [`event`] — telemetry events emitted by the `arcade-telemetry.pk3`
//!   ZScript handler and parsed by the supervisor (SPEC §4.5).
//! - [`scoring`] — the scoring formula and its constants (SPEC §5).
//! - [`submit`] — the run-submission wire types accepted by
//!   `POST /v1/runs` on the leaderboard service (SPEC §7.2/§7.3).
//! - [`boards`] — leaderboard API types returned by `GET /v1/boards`
//!   and rendered by the attract app (SPEC §6/§7.3).
//!
//! Everything is re-exported at the crate root, so downstream crates can
//! simply write `use protocol::{Event, RunSubmission, BoardsResponse};`.

#![warn(missing_docs)]

pub mod boards;
pub mod event;
pub mod scoring;
pub mod submit;

pub use boards::*;
pub use event::*;
pub use scoring::*;
pub use submit::*;
