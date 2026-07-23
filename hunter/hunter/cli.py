"""CLI entry point: python3 -m hunter <command>."""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from .ingest import ingest_findings
from .store import Store
from .types import Config

VERDICT_STATUSES = ("queued", "rejected", "wontfix", "note", "merged")


def _fmt(rows: list[dict], cols: list[str]) -> None:
    if not rows:
        print("(none)")
        return
    widths = [max(len(c), *(len(str(r.get(c, ""))) for r in rows)) for c in cols]
    print("  ".join(c.ljust(w) for c, w in zip(cols, widths)))
    for r in rows:
        print("  ".join(str(r.get(c, "") if r.get(c) is not None else "").ljust(w)
                        for c, w in zip(cols, widths)))


def _repo_or_die(store: Store, name: str) -> dict:
    repo = store.get_repo(name)
    if repo is None:
        sys.exit(f"error: unknown repo {name!r} (see: python3 -m hunter repos)")
    return repo


# -- command handlers -----------------------------------------------------

def cmd_init(store: Store, cfg: Config, args) -> None:
    print(f"db ready at {cfg.db_path}")


def cmd_add_repo(store: Store, cfg: Config, args) -> None:
    path = cfg.work_root / "repos" / args.name
    rid = store.add_repo(args.name, args.url, str(path), args.branch)
    print(f"repo #{rid} {args.name} -> {path}")


def cmd_repos(store: Store, cfg: Config, args) -> None:
    _fmt(store.list_repos(),
         ["id", "name", "url", "default_branch", "last_hunt_sha", "enabled"])


def cmd_findings(store: Store, cfg: Config, args) -> None:
    repo_id = _repo_or_die(store, args.repo)["id"] if args.repo else None
    _fmt(store.list_findings(status=args.status, repo_id=repo_id),
         ["id", "repo_id", "status", "severity", "confidence", "bug_class",
          "file", "line", "summary"])


def cmd_verdict(store: Store, cfg: Config, args) -> None:
    if args.status in ("rejected", "wontfix") and not args.reason:
        sys.exit(f"error: --reason is required for verdict '{args.status}'"
                 " (it feeds the suppression corpus)")
    if store.get_finding(args.fid) is None:
        sys.exit(f"error: no finding #{args.fid}")
    store.set_status(args.fid, args.status, verdict_reason=args.reason)
    store.log_event("verdict", f"finding #{args.fid} -> {args.status}"
                    + (f": {args.reason}" if args.reason else ""),
                    finding_id=args.fid)
    print(f"finding #{args.fid} -> {args.status}")


def cmd_ingest(store: Store, cfg: Config, args) -> None:
    repo = _repo_or_die(store, args.repo)
    counts = ingest_findings(store, repo["id"], Path(args.file))
    print(json.dumps(counts))


def cmd_jobs(store: Store, cfg: Config, args) -> None:
    _fmt(store.list_jobs(),
         ["id", "kind", "repo_id", "finding_id", "state", "tokens_new",
          "calls", "exit_code", "killed_reason"])


def cmd_events(store: Store, cfg: Config, args) -> None:
    for e in reversed(store.recent_events()):
        print(f"{e['at']}  [{e['kind']}]  {e['message']}")


def cmd_hunt(store: Store, cfg: Config, args) -> None:
    from .scheduler import run_hunt  # built separately; may not exist yet
    repo = _repo_or_die(store, args.repo)
    print(json.dumps(run_hunt(store, cfg, repo, force=args.force), default=str))


def cmd_fix(store: Store, cfg: Config, args) -> None:
    from .scheduler import run_fix
    finding = store.get_finding(args.fid)
    if finding is None:
        sys.exit(f"error: no finding #{args.fid}")
    print(json.dumps(run_fix(store, cfg, finding), default=str))


def cmd_cycle(store: Store, cfg: Config, args) -> None:
    from .scheduler import run_cycle
    force_repo = _repo_or_die(store, args.repo)["name"] if args.repo else None
    print(json.dumps(run_cycle(store, cfg, force_repo=force_repo), default=str))


def cmd_serve(store: Store, cfg: Config, args) -> None:
    from .server import serve
    if args.port:
        cfg.serve_port = args.port
    serve(cfg)


# -- parser ---------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="hunter", description="Idle-Token Bug Hunter")
    ap.add_argument("--config", type=Path, default=None,
                    help="path to config.json (default: hunter/config.json)")
    sub = ap.add_subparsers(dest="cmd", required=True)

    sub.add_parser("init", help="create/upgrade the database").set_defaults(fn=cmd_init)

    p = sub.add_parser("add-repo", help="register a repo")
    p.add_argument("name")
    p.add_argument("url")
    p.add_argument("--branch", default="main")
    p.set_defaults(fn=cmd_add_repo)

    sub.add_parser("repos", help="list repos").set_defaults(fn=cmd_repos)

    p = sub.add_parser("findings", help="list findings")
    p.add_argument("--status", default=None)
    p.add_argument("--repo", default=None)
    p.set_defaults(fn=cmd_findings)

    p = sub.add_parser("verdict", help="triage a finding")
    p.add_argument("fid", type=int)
    p.add_argument("status", choices=VERDICT_STATUSES)
    p.add_argument("--reason", default=None)
    p.set_defaults(fn=cmd_verdict)

    p = sub.add_parser("ingest", help="ingest a findings.json")
    p.add_argument("file")
    p.add_argument("--repo", required=True)
    p.set_defaults(fn=cmd_ingest)

    sub.add_parser("jobs", help="list jobs").set_defaults(fn=cmd_jobs)
    sub.add_parser("events", help="recent events").set_defaults(fn=cmd_events)

    p = sub.add_parser("hunt", help="run a hunt for one repo")
    p.add_argument("repo")
    p.add_argument("--force", action="store_true")
    p.set_defaults(fn=cmd_hunt)

    p = sub.add_parser("fix", help="run a fix for one finding")
    p.add_argument("fid", type=int)
    p.set_defaults(fn=cmd_fix)

    p = sub.add_parser("cycle", help="run one scheduler cycle")
    p.add_argument("--repo", default=None)
    p.set_defaults(fn=cmd_cycle)

    p = sub.add_parser("serve", help="run the triage UI")
    p.add_argument("--port", type=int, default=None)
    p.set_defaults(fn=cmd_serve)

    return ap


def main(argv: list[str] | None = None) -> None:
    args = build_parser().parse_args(argv)
    cfg = Config.load(args.config)
    store = Store(cfg)
    try:
        args.fn(store, cfg, args)
    finally:
        store.db.close()
