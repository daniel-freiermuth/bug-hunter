"""The Python and Rust migration systems must converge on one schema.

Two systems with opposite philosophies write the same SQLite file:

- Python is convergent. `Store.__init__` runs `schema.sql`
  (`CREATE TABLE IF NOT EXISTS`), then a hand-maintained `ADD COLUMN`
  list, then a condition-probed rebuild of `findings`. Re-running is a
  no-op; `schema.sql` is freely editable because it *is* the desired
  state.
- Rust is versioned. `sqlx::migrate!` replays `hunter-rs/migrations/`
  in order, exactly once each, checksummed in `_sqlx_migrations`.
  Editing an applied file makes the daemon refuse to start.

Nothing asserted they agree, and they drifted: migration 001 began as a
symlink to `schema.sql`, was frozen as a copy of the then-deployed
schema, and 002's comment — claiming the initial schema already had the
composite UNIQUE — silently became false. The next person read that
comment, wrote a migration on top of it, and shipped a database that
rejected two findings of different types sharing a fingerprint.

These tests compare the SCHEMAS the two systems produce, not the files
they are written in. Only `sqlite3` is needed to replay the Rust
migrations, so this stays on the Python side rather than dragging a
cargo toolchain into it.
"""

from __future__ import annotations

import re
import sqlite3
from pathlib import Path
from typing import Any

import pytest

from hunter.store import Store
from hunter.types import Config

MIGRATIONS = Path(__file__).resolve().parents[2] / "hunter-rs" / "migrations"

# `_sqlx_migrations` is Rust's own bookkeeping and has no Python
# counterpart by design.
IGNORED_TABLES = {"_sqlx_migrations"}

# `sqlite_master.sql` is the CREATE statement as typed, comments and
# all, and `repos`' comment explains why the keyword is there -- so a
# plain substring test reports AUTOINCREMENT for a table that no longer
# declares it. Strip comments first, then match the keyword itself.
_SQL_COMMENT = re.compile(r"--[^\n]*|/\*.*?\*/", re.DOTALL)


def _declares_autoincrement(create_sql: str) -> bool:
    return bool(re.search(r"\bAUTOINCREMENT\b", _SQL_COMMENT.sub(" ", create_sql), re.IGNORECASE))


def _snapshot(db_path: Path) -> dict[str, Any]:
    """Semantic schema: tables, columns, indexes and rowid allocation.

    Deliberately not a text diff of the DDL. The two systems arrive at
    the same schema by different routes — Python from one `CREATE
    TABLE`, Rust from a copy plus a table rebuild — so whitespace and
    column ordering differ without meaning anything. What must match is
    the set of columns with their types, nullability, defaults and
    primary keys, and the set of indexes with their uniqueness and
    covered columns.

    `PRAGMA table_info` is blind to AUTOINCREMENT — it describes
    `INTEGER PRIMARY KEY` and `INTEGER PRIMARY KEY AUTOINCREMENT`
    identically — so that one keyword is read out of the stored DDL
    instead. It is not cosmetic: without it SQLite reissues the highest
    freed rowid, and a `repos.id` reused while the previous repo's
    `repos/repo-<id>` clone is still on disk points the new repo at the
    old one's files. Both daemons share `work_root`, so only one of them
    having it is the same as neither having it.
    """
    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    try:
        out: dict[str, Any] = {}
        table_sql = {
            r[0]: r[1] or ""
            for r in db.execute(
                "SELECT name, sql FROM sqlite_master "
                "WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
            )
            if r[0] not in IGNORED_TABLES
        }
        tables = list(table_sql)
        for table in tables:
            cols = {
                r[1]: (r[2].upper(), bool(r[3]), r[4], bool(r[5]))
                for r in db.execute(f"PRAGMA table_info({table})")
            }
            indexes = {}
            for row in db.execute(f"PRAGMA index_list({table})"):
                name, unique = row[1], bool(row[2])
                covered = tuple(x[2] for x in db.execute(f"PRAGMA index_info({name})"))
                # Autoindex names are positional, so a UNIQUE constraint
                # declared in a different order would produce a different
                # name for the same guarantee. Key on the guarantee.
                key = name if not name.startswith("sqlite_autoindex") else f"<unique:{covered}>"
                indexes[key] = (unique, covered)
            out[table] = {
                "columns": cols,
                "indexes": indexes,
                "autoincrement": _declares_autoincrement(table_sql[table]),
            }
        return out
    finally:
        db.close()


def _rust_schema(db_path: Path, through: int | None = None) -> None:
    """Replay `hunter-rs/migrations/*.sql` into `db_path`.

    Same order sqlx uses (lexicographic by filename) and the same order
    `build.rs` replays for the sqlx compile-time checks.
    """
    files = sorted(MIGRATIONS.glob("*.sql"))
    assert files, f"no migrations found under {MIGRATIONS}"
    if through is not None:
        files = files[:through]
    db = sqlite3.connect(db_path)
    try:
        for sql_file in files:
            db.executescript(sql_file.read_text())
        db.commit()
    finally:
        db.close()


def _assert_same(expected: dict[str, Any], actual: dict[str, Any], what: str) -> None:
    assert set(expected) == set(actual), (
        f"{what}: table sets differ — "
        f"only python: {sorted(set(expected) - set(actual))}, "
        f"only rust: {sorted(set(actual) - set(expected))}"
    )
    for table in sorted(expected):
        exp_c, act_c = expected[table]["columns"], actual[table]["columns"]
        assert set(exp_c) == set(act_c), (
            f"{what}: {table} columns differ — "
            f"only python: {sorted(set(exp_c) - set(act_c))}, "
            f"only rust: {sorted(set(act_c) - set(exp_c))}"
        )
        for col in sorted(exp_c):
            assert exp_c[col] == act_c[col], (
                f"{what}: {table}.{col} differs — "
                f"python {exp_c[col]} vs rust {act_c[col]} "
                "(type, notnull, default, pk)"
            )
        assert expected[table]["indexes"] == actual[table]["indexes"], (
            f"{what}: {table} indexes differ — "
            f"python {expected[table]['indexes']} vs rust {actual[table]['indexes']}"
        )
        assert expected[table]["autoincrement"] == actual[table]["autoincrement"], (
            f"{what}: {table} AUTOINCREMENT differs — "
            f"python {expected[table]['autoincrement']} vs "
            f"rust {actual[table]['autoincrement']}; the two daemons would "
            "allocate rowids differently for the same table"
        )


def test_migrations_dir_exists() -> None:
    """A moved or renamed migrations directory must fail loudly here,
    not silently turn the parity tests into no-ops."""
    assert MIGRATIONS.is_dir(), f"expected Rust migrations at {MIGRATIONS}"
    assert min(p.name for p in MIGRATIONS.glob("*.sql")).startswith("001"), (
        "migrations must start at 001; sqlx orders them by filename"
    )


def test_fresh_databases_match(tmp_path: Path) -> None:
    """A new install gets the same schema from either system."""
    py_db = tmp_path / "python.db"
    Store(Config(work_root=tmp_path, db_path=py_db))

    rs_db = tmp_path / "rust.db"
    _rust_schema(rs_db)

    _assert_same(_snapshot(py_db), _snapshot(rs_db), "fresh install")


def test_upgraded_databases_match(tmp_path: Path) -> None:
    """An existing install converges too — the case that actually broke.

    Migration 001 is a frozen copy of the schema as deployed, so it is
    exactly the starting point a real upgrade begins from. Bring that
    forward both ways and the results must agree: Python by
    `schema.sql` plus its ALTERs and rebuild probe, Rust by migrations
    002 onward.

    This is the direction that failed before. 002 could only ADD the
    composite index, leaving the legacy single-column UNIQUE in place,
    so an upgraded database silently rejected a `test_gap` and a `bug`
    sharing a fingerprint while a fresh one accepted them.
    """
    py_db = tmp_path / "python_upgraded.db"
    _rust_schema(py_db, through=1)  # the deployed schema, pre-upgrade
    Store(Config(work_root=tmp_path, db_path=py_db))  # Python brings it forward

    rs_db = tmp_path / "rust_upgraded.db"
    _rust_schema(rs_db)  # Rust brings the same start forward

    _assert_same(_snapshot(py_db), _snapshot(rs_db), "upgraded install")


def test_python_upgrade_rebuilds_repos_without_reusing_ids(tmp_path: Path) -> None:
    """The rebuild behind that flag, asserted as behaviour.

    Schema equality says the keyword is there; this says what it buys
    and what the rebuild must not cost. `repos.id` is referenced by
    `findings.repo_id` and `jobs.repo_id`, so copying the rows into a
    new table has to carry the ids verbatim — a renumbering reparents
    those rows silently, and nothing downstream would notice.
    """
    db_path = tmp_path / "pre_autoincrement.db"
    _rust_schema(db_path, through=8)  # 001..008: repos.id is a bare rowid alias
    db = sqlite3.connect(db_path)
    try:
        create_sql = db.execute("SELECT sql FROM sqlite_master WHERE name = 'repos'").fetchone()[0]
        assert not _declares_autoincrement(create_sql), "seed must start without the keyword"
        # Non-contiguous ids: a straight copy preserves them, a rebuild
        # that renumbers would close the gap and look tidy doing it.
        for rid, name in ((1, "alpha"), (4, "beta"), (9, "gamma")):
            db.execute(
                "INSERT INTO repos (id, name, url, path, forge, default_branch,"
                " enabled, added_at) VALUES (?, ?, ?, ?, 'github', 'main', 1, 1)",
                (rid, name, f"https://example.invalid/{name}", f"/w/repos/repo-{rid}"),
            )
        db.execute(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence,"
            " summary, status, created_at, updated_at)"
            " VALUES (7, 'bug', 9, 'fp', 'high', 0.9, 's', 'new', 1, 1)"
        )
        db.execute("INSERT INTO jobs (id, kind, repo_id, state) VALUES (3, 'hunt', 4, 'done')")
        db.commit()
        before = db.execute("SELECT id, name FROM repos ORDER BY id").fetchall()
    finally:
        db.close()

    Store(Config(work_root=tmp_path, db_path=db_path))

    db = sqlite3.connect(db_path)
    try:
        create_sql = db.execute("SELECT sql FROM sqlite_master WHERE name = 'repos'").fetchone()[0]
        assert _declares_autoincrement(create_sql), "the upgrade must add AUTOINCREMENT"
        assert db.execute("SELECT id, name FROM repos ORDER BY id").fetchall() == before
        assert db.execute("PRAGMA foreign_key_check").fetchall() == [], (
            "the table swap orphaned a findings or jobs row"
        )
        assert db.execute("SELECT repo_id FROM findings WHERE id = 7").fetchone()[0] == 9
        assert db.execute("SELECT repo_id FROM jobs WHERE id = 3").fetchone()[0] == 4
        assert db.execute(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'repos_deleted_at'"
        ).fetchone(), "the DROP took migration 008's partial index with it"

        # The point of the keyword: free the highest id and the next
        # insert must climb past it, not step into the hole.
        db.execute("DELETE FROM findings WHERE repo_id = 9")
        db.execute("DELETE FROM repos WHERE id = 9")
        db.commit()
        reissued = db.execute(
            "INSERT INTO repos (name, url, path, forge, default_branch, enabled, added_at)"
            " VALUES ('delta', 'https://example.invalid/delta', '/w/repos/x', 'github',"
            " 'main', 1, 1)"
        ).lastrowid
        db.commit()
        assert reissued == 10, f"id {reissued} reuses a freed repo id"
    finally:
        db.close()

    # The live database already has the keyword; opening it again must
    # not rebuild anything.
    Store(Config(work_root=tmp_path, db_path=db_path))
    db = sqlite3.connect(db_path)
    try:
        assert (
            db.execute(
                "SELECT name FROM sqlite_master WHERE name LIKE '\\_repos%' ESCAPE '\\'"
            ).fetchall()
            == []
        ), "a second open left a rebuild table behind"
        assert db.execute("SELECT COUNT(*) FROM repos").fetchone()[0] == 3
    finally:
        db.close()


@pytest.mark.parametrize("types", [("bug", "test_gap"), ("refactor", "dep_update")])
def test_both_systems_allow_one_fingerprint_across_types(
    tmp_path: Path, types: tuple[str, str]
) -> None:
    """The behaviour the schemas exist to guarantee, asserted directly.

    Schema equality is a proxy; this is the property it is a proxy for.
    Both an upgraded and a fresh database must accept the same
    fingerprint under two different finding types — that is the whole
    point of `UNIQUE(type, fingerprint)`, and the bug that shipped was
    exactly this insert being rejected.
    """
    for label, build in (
        ("fresh", lambda p: Store(Config(work_root=tmp_path, db_path=p))),
        (
            "upgraded",
            lambda p: (_rust_schema(p, through=1), Store(Config(work_root=tmp_path, db_path=p))),
        ),
        ("rust", _rust_schema),
    ):
        db_path = tmp_path / f"{label}_{types[0]}.db"
        build(db_path)
        db = sqlite3.connect(db_path)
        try:
            db.execute(
                "INSERT INTO repos (name, url, path, forge, default_branch, added_at) "
                "VALUES ('r', 'u', 'p', 'github', 'main', 0)"
            )
            for finding_type in types:
                db.execute(
                    "INSERT INTO findings (type, repo_id, fingerprint, severity, "
                    "confidence, summary, created_at, updated_at) "
                    "VALUES (?, 1, 'SHARED-FP', 'low', 0.5, 's', 0, 0)",
                    (finding_type,),
                )
            db.commit()
        except sqlite3.IntegrityError as exc:  # pragma: no cover - failure path
            pytest.fail(f"{label} db rejected {types[1]} sharing a fingerprint: {exc}")
        finally:
            db.close()


def test_python_rebuild_preserves_every_column_value(tmp_path: Path) -> None:
    """The findings rebuild must carry data across, not just shape.

    Python's upgrade path rebuilds `findings` into a new table to drop the
    legacy single-column UNIQUE, copying rows with an explicit column
    list. That list is hand-maintained beside the table's own DDL, so a
    column can be *declared* on the new table and never *copied* — the
    rebuild then silently nulls it for every existing row.

    The schema comparisons above cannot see this: both sides end up with
    an identical column, one of them empty. So this asserts values, and
    does it generically — every nullable column gets a distinct marker
    and must come back unchanged, which covers columns added after this
    test was written.
    """
    db_path = tmp_path / "legacy.db"
    _rust_schema(db_path, through=1)  # deployed schema: legacy UNIQUE present

    db = sqlite3.connect(db_path)
    # The probe migration adds this before the rebuild runs, so a real
    # database reaching the rebuild always has it.
    db.execute("ALTER TABLE findings ADD COLUMN standard_section TEXT")
    cols = [r[1] for r in db.execute("PRAGMA table_info(findings)")]
    notnull = {r[1] for r in db.execute("PRAGMA table_info(findings)") if r[3]}

    # A distinct, recognisable value in every text column we may set.
    payload = {
        c: f"value-for-{c}"
        for c in cols
        if c not in notnull and c not in {"id", "line", "rung_achieved", "confidence"}
    }
    fixed = {
        "type": "standards",
        "repo_id": 1,
        "fingerprint": "FP",
        "severity": "high",
        "confidence": 0.9,
        "summary": "s",
        "status": "new",
        "created_at": 1,
        "updated_at": 1,
    }
    row = {**payload, **fixed}
    db.execute(
        "INSERT INTO repos (name, url, path, forge, default_branch, added_at) "
        "VALUES ('r', 'u', 'p', 'github', 'main', 0)"
    )
    # Column names come from PRAGMA table_info on a table this test just
    # created; every value is bound.
    db.execute(
        f"INSERT INTO findings ({', '.join(row)}) VALUES ({', '.join('?' * len(row))})",  # noqa: S608
        tuple(row.values()),
    )
    db.commit()
    assert [r[2] for r in db.execute("PRAGMA index_info(sqlite_autoindex_findings_1)")] == [
        "fingerprint"
    ], "precondition: the legacy single-column UNIQUE is what triggers the rebuild"
    db.close()

    Store(Config(work_root=tmp_path, db_path=db_path))  # performs the rebuild

    db = sqlite3.connect(db_path)
    db.row_factory = sqlite3.Row
    after = db.execute("SELECT * FROM findings").fetchone()
    lost = [c for c, v in payload.items() if after[c] != v]
    db.close()
    assert not lost, f"the rebuild dropped values for: {lost}"
