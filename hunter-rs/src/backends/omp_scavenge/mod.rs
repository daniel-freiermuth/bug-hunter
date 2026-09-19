//! omp-scavenge backend — spends idle Claude-subscription capacity by
//! scavenging omp's local usage mirror. Port of
//! `hunter/backends/omp_scavenge`/ per BACKEND-CONTRACT.md §2.
//!
//! Round 2: capacity math + facade (`decide/keep_fresh/status_html`).
//! Round 3: harness (`run_worker` / usage-delta sandwich / `Backend::run`).

pub mod capacity;
mod facade;
pub mod harness;

pub use capacity::default_agent_db;
pub use facade::OmpScavengeBackend;
