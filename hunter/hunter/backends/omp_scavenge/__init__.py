"""OMP + Anthropic scavenging backend.

Bundles:
- Harness: spawns headless omp, meters its ledger JSONL, SIGTERMs at cap.
- Accounting: reads Anthropic window state from omp's agent.db, rolls
  forward expired cycles, computes unaccounted tokens.
- Policy: dual linear ramps (5h harvest window + 7d weekly cap) with
  configurable human headroom; prioritized work waives pacing but not
  exhaustion.

See hunter.backend.Backend for the protocol this implements.
"""

from .facade import OmpScavengeBackend

__all__ = ["OmpScavengeBackend"]
