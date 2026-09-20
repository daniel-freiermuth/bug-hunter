//! Backend implementations. `omp_scavenge` is the only one; a BYOK backend
//! is the planned second consumer of the trait (future.md §8.3).

pub mod omp_scavenge;
