# Coding Standards

Authoritative principles for all projects in this workspace. The union
of conventions established across logcrab, recentip, signalk-chart-rs,
and hunter-rs — proven by production use, not theoretical preference.

## Values

**Correctness → Maintainability → Boringness → Defensiveness**

- **Correctness first.** Code that produces wrong results is worse than
  code that crashes. Verify behavior, not just compilation.
- **Maintainability second.** The next person (or AI) reading this code
  six months from now matters more than saving a line today.
- **Boringness third.** Prefer the well-understood, widely-used approach.
  Novel solutions need to justify their novelty.
- **Fail early, fail loudly.** Never silently degrade. If a precondition
  is missing, refuse the operation with a clear error — don't produce
  garbage output that burns resources downstream. An error that surfaces
  at the point of failure is cheaper than one that surfaces three layers
  later as mysterious wrong behavior.

Additional:
- **Full agency.** Own the code. Delete what isn't pulling its weight.
  Refuse unnecessary abstractions.
- **Do one job, do it well.** Each module, function, and type has a
  clear single responsibility.
- **PoC-first.** Prove the pipeline works end-to-end, then build
  incrementally. Never design in isolation.

---

## Safety

### No unsafe code
`unsafe_code = "forbid"` (or at minimum `"deny"`). Use safe wrappers
(`nix`, `fs4`, `socket2`) instead of raw FFI. Zero exceptions in
application code; document any library exception with a safety proof.

### No panics in production code
```toml
[lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
```
Tests may `#![allow(...)]` these. Production code propagates errors
via `Result`, uses `unwrap_or`/`unwrap_or_default` for infallible
defaults, or returns early. A panic is a bug, not an error handling
strategy.

### No unchecked indexing (libraries)
`indexing_slicing = "deny"` in library code. Use `.get()`, iterators,
or bounds-checked access. Application/GUI code may allow when indices
are structurally guaranteed.

---

## Linting

### Clippy configuration
```toml
[lints.rust]
unsafe_code = "forbid"
unused_must_use = "deny"

[lints.clippy]
all = { level = "deny", priority = -1 }
pedantic = { level = "warn", priority = -1 }

# Safety
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"

# Accepted pedantic overrides
too_many_arguments = "allow"
module_name_repetitions = "allow"
must_use_candidate = "allow"
missing_errors_doc = "allow"       # internal crates
missing_panics_doc = "allow"
cast_possible_truncation = "allow"
cast_sign_loss = "allow"
cast_precision_loss = "allow"
cast_possible_wrap = "allow"
```

Libraries (recentip) may escalate `pedantic` and `nursery` to `"deny"`.
Applications may keep them at `"warn"`.

### Formatting
Use `cargo fmt` defaults. No `rustfmt.toml`. Formatting is not a
style discussion — it's automated and enforced in CI.

---

## Error handling

### By project type
- **Libraries:** Custom `Error` enum with `thiserror`, `Result<T>`
  type alias, documented recovery per variant. Export the error type.
- **Applications/servers:** `anyhow::Result` with `.context()` for
  propagation. Domain error enums with `thiserror` at API boundaries
  (e.g. 400 vs 500 in HTTP handlers).

### Rules
- Propagate with context: `.with_context(|| format!("reading {}", path.display()))`
- Distinguish domain refusals from infrastructure errors at the type level
- Log errors at the handler/boundary where recovery decisions are made,
  not deep in library code
- `bail!()` for early returns with descriptive messages
- Never suppress errors silently — log at minimum

---

## Type safety

### Domain types over primitives
Parse at the boundary, typed internally. If a string represents a
fixed set of values, it's an enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum FindingStatus { New, Queued, Fixing, PrOpen, ... }
```

A typo in a status string is a runtime bug. A typo in
`FindingStatus::PrOpne` is a compile error.

### Newtypes for domain IDs
When a bare `i64` could mean repo ID, finding ID, or job ID
interchangeably — consider newtypes. At minimum, use named parameters
(never two `i64` positional params with different semantics).

### Named-field structs for mutations
Never pass multiple `Option<&str>` parameters with different semantics
(the `pr_url`-as-`verdict_reason` bug class). Use structs with named
fields or specific purpose-named functions.

### No `serde_json::Value` in internal data flow
External API responses are parsed into typed structs at the boundary.
Internal code operates on domain types. `serde_json::Value` is only
for genuinely unknown schemas (third-party APIs with no stable contract)
and JSON-text embedding in prompts.

### TypeScript strictness
When a project has a TypeScript frontend:
```json
{
  "strict": true,
  "noUncheckedIndexedAccess": true,
  "noUnusedLocals": true,
  "noUnusedParameters": true,
  "noFallthroughCasesInSwitch": true
}
```
ESLint: `@typescript-eslint/no-explicit-any: "error"`.

---

## Database / SQL

### Compile-time checked queries only
All SQL uses `sqlx::query!` / `sqlx::query_as!` macros, validated
against the schema at compile time. No `QueryBuilder`, no runtime
string construction, no `sqlx::query(` without `!`.

One documented exception: reading an external database whose schema
isn't available at compile time (e.g. omp's `agent.db`).

### Encapsulated data access
The database pool is private to the store module. All SQL goes
through typed store methods. Code outside the store cannot construct
or execute queries. Enforced by:
1. `fn pool()` is private (not `pub`)
2. CI lint: `grep` guards verify no pool leakage
3. Zero `QueryBuilder` in the codebase

### Specific methods over generic updates
No `update_job(id, &BigOptionalStruct)` with 15 Optional fields.
Instead: `complete_job(id, state, ...)`, `orphan_job(id, ...)` —
each a single compile-time checked query where every parameter is
required for that operation.

---

## Testing

### Philosophy
- **Deterministic:** no flaky tests. Use simulation (turmoil) over
  mocking. Fresh state per test (temp dirs, fresh DBs).
- **Behavioral:** test what the code does, not how it's structured.
  Assert on domain types, not string representations.
- **Coverage:** every changed line should be covered. Use `llvm-cov`
  for measurement (libraries).

### Patterns
- Integration tests in `tests/` directory
- Fresh isolated state per test: `fresh_db()`, `tempdir()`
- `cargo nextest` for parallel execution and stress testing (libraries)
- Compliance traceability where applicable (`covers!()` macro)

---

## Dependencies

### Policy
- Minimal runtime deps. Every dependency is a maintenance liability
  and an attack surface.
- Feature-gate optional deps (`optional = true`)
- Pin CI tool versions to exact URLs/hashes, not mutable installers
- Common choices: `tokio`, `serde`, `anyhow`/`thiserror`, `tracing`,
  `axum` (server), `sqlx` (database), `clap` (CLI)

---

## Performance

- Profile before optimizing. Use `profiling::scope!()` or equivalent
  for hot paths.
- Pre-size collections: `Vec::with_capacity(n)` when size is known
- No needless allocations or copies
- LTO + `codegen-units = 1` for release builds (libraries and
  performance-critical applications)
- GPU shaders for per-frame per-entity math (graphics projects)
- Batch cross-boundary calls (JS↔WASM, network, IPC)

---

## CI / Enforcement

Every project enforces at minimum:
1. `cargo fmt --check`
2. `cargo clippy` with deny-level lints
3. `cargo test`

Additional per project type:
- **Frontend:** `svelte-check`, `eslint` (strict), `vitest`
- **Libraries:** `cargo nextest`, `llvm-cov`, doc tests
- **Servers:** SQL encapsulation grep guards, parity tests
- **Git:** fast-forward only merges, no force-push to default branch

---

## Documentation

- Module-level doc comments explaining conventions and non-obvious
  decisions
- API contracts as separate markdown docs for cross-module interfaces
- Architecture decision records (ADRs) for significant choices,
  with rationale and rejected alternatives
- Code comments for *why*, not *what*
