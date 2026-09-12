"""Tests for hunter.scheduler.anticipated_tokens.

A job's own cap_tokens badly underestimates the risk of a cold-cache
first call (observed up to 4.8x cap_tokens in production -- see
call). anticipated_tokens gives budget.decide() a realistic pre-start
reservation instead, based on this job kind's own history and whether
this specific (repo, kind) pair's prompt cache is likely still warm.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from hunter.scheduler import anticipated_tokens
from hunter.store import Store
from hunter.types import Config, now_ms


@pytest.fixture
def cfg(tmp_path: Path) -> Config:
    return Config(work_root=tmp_path, db_path=tmp_path / "test.db")


@pytest.fixture
def store(cfg: Config) -> Store:
    return Store(cfg)


def _finished_job(store: Store, repo_id: int, kind: str, tokens: int, finished_at: int) -> None:
    jid = store.create_job(kind, repo_id)
    store.update_job(jid, state="done", tokens_new=tokens, finished_at=finished_at)


def test_no_history_returns_zero(store: Store, cfg: Config) -> None:
    rid = store.add_repo("r", "https://r", "/r")
    assert anticipated_tokens(store, cfg, rid, "dep_update") == 0


def test_cold_uses_p90_not_p50(store: Store, cfg: Config) -> None:
    """No recent run of this (repo, kind) pair -> the prompt cache is
    likely cold -> anticipate the historical p90, not the (much lower)
    median a bimodal cost distribution would otherwise suggest."""
    rid = store.add_repo("r", "https://r", "/r")
    old = now_ms() - 2 * (cfg.cache_ttl_s * 1000)
    # 9 cheap warm-cache-style runs, 1 expensive cold-cache run.
    tokens = [1_000] * 9 + [700_000]
    for i, t in enumerate(tokens):
        _finished_job(store, rid, "dep_update", t, old - i * 1000)
    anticipated = anticipated_tokens(store, cfg, rid, "dep_update")
    assert anticipated == 700_000, "cold (no recent run) must anticipate the p90, not the median"


def test_warm_uses_median_not_p90(store: Store, cfg: Config) -> None:
    """A recent finish of this exact (repo, kind) pair within the cache
    TTL -> the prompt cache is likely still warm -> anticipate the
    (much cheaper) historical median instead of the worst case."""
    rid = store.add_repo("r", "https://r", "/r")
    old = now_ms() - 2 * (cfg.cache_ttl_s * 1000)
    tokens = [1_000] * 9 + [700_000]
    for i, t in enumerate(tokens):
        _finished_job(store, rid, "dep_update", t, old - i * 1000)
    # A recent finish of the SAME (repo, kind) pair, well inside the TTL.
    _finished_job(store, rid, "dep_update", 1_500, now_ms() - 60_000)
    anticipated = anticipated_tokens(store, cfg, rid, "dep_update")
    assert anticipated == 1_000, "warm (recent run) must anticipate the median, not the p90"


def test_warmth_is_scoped_to_this_repo(store: Store, cfg: Config) -> None:
    """A recent run of the same kind on a DIFFERENT repo must not count
    as this repo's cache being warm -- prompt caches are per-repo
    context, not per-kind globally. (The 80:20 cheap:expensive ratio
    keeps the pooled p90 index comfortably inside the expensive tier
    even after repo B's one extra cheap datapoint shifts it by one.)"""
    rid_a = store.add_repo("a", "https://a", "/a")
    rid_b = store.add_repo("b", "https://b", "/b")
    old = now_ms() - 2 * (cfg.cache_ttl_s * 1000)
    for i, t in enumerate([1_000] * 80 + [700_000] * 20):
        _finished_job(store, rid_a, "dep_update", t, old - i * 1000)
    cold_estimate = anticipated_tokens(store, cfg, rid_a, "dep_update")
    assert cold_estimate == 700_000
    # Repo B finishes a job well inside the cache TTL -- irrelevant to A.
    _finished_job(store, rid_b, "dep_update", 1_500, now_ms() - 60_000)
    still_cold_estimate = anticipated_tokens(store, cfg, rid_a, "dep_update")
    assert still_cold_estimate == 700_000, (
        "an unrelated repo's recent run must not change this repo's own warm/cold classification"
    )


def test_kind_scoped_independently(store: Store, cfg: Config) -> None:
    """dep_update's history must not leak into hunt's anticipated cost."""
    rid = store.add_repo("r", "https://r", "/r")
    old = now_ms() - 2 * (cfg.cache_ttl_s * 1000)
    for i, t in enumerate([1_000] * 9 + [700_000]):
        _finished_job(store, rid, "dep_update", t, old - i * 1000)
    assert anticipated_tokens(store, cfg, rid, "hunt") == 0


def _denied_job(store: Store, repo_id: int, kind: str, finished_at: int) -> None:
    jid = store.create_job(kind, repo_id)
    store.update_job(jid, state="denied", notes="budget", finished_at=finished_at)


def test_denied_job_does_not_count_as_warm(store: Store, cfg: Config) -> None:
    """A budget-denied job never made an LLM call, so its prompt cache
    claim is fictional -- it must not downgrade the estimate from p90 to
    median the way a genuine recent run would."""
    rid = store.add_repo("r", "https://r", "/r")
    old = now_ms() - 2 * (cfg.cache_ttl_s * 1000)
    tokens = [1_000] * 9 + [700_000]
    for i, t in enumerate(tokens):
        _finished_job(store, rid, "dep_update", t, old - i * 1000)
    # This repo/kind pair was DENIED moments ago -- no worker ever ran.
    _denied_job(store, rid, "dep_update", now_ms() - 60_000)
    anticipated = anticipated_tokens(store, cfg, rid, "dep_update")
    assert anticipated == 700_000, (
        "a denied job must not be mistaken for a warm prompt cache -- it never ran"
    )
