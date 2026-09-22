//! omp-scavenge backend — spends idle Claude-subscription capacity by
//! scavenging omp's local usage mirror. Port of
//! `hunter/backends/omp_scavenge`/ per BACKEND-CONTRACT.md §2.
//!
//! `capacity`: the two-ramp window math. `facade`: `decide`, `keep_fresh`
//! and `status_html`. `harness`: `run_worker`, the usage-delta sandwich
//! around it, and `Backend::run`.
pub mod capacity;
mod facade;
pub mod harness;
pub mod provider;

pub use capacity::default_agent_db;
pub use facade::OmpScavengeBackend;
pub use provider::LlmProvider;
// The status renderer and its inputs: a seam so the exact markup of
// BACKEND-CONTRACT.md §2.3 can be pinned by snapshot without a clock or
// a database. See tests/status_html_test.rs.
pub use facade::{NO_WINDOW_DATA, StatusInputs, render_status};
