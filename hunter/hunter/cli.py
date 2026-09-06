"""CLI entry point: python3 -m hunter <command>.

Reduced to the two load-bearing entry points (systemd runs `daemon`;
`serve` is UI-only for local dev). Repo registration, notes, triage
verdicts, and manual job runs all moved into the web UI (:8377) as it
grew to cover them -- see hunter/ui. This file's job is now just: parse
argv, open the Store (which owns schema creation/migration), dispatch.
"""

from __future__ import annotations

import argparse
import logging
from pathlib import Path

from .store import Store
from .types import Config

_LOG_FORMAT = "%(asctime)s %(levelname)-5s %(name)s  %(message)s"


def cmd_serve(store: Store, cfg: Config, args: argparse.Namespace) -> None:
    from .server import serve

    if args.port:
        cfg.serve_port = args.port
    serve(cfg)


def cmd_daemon(store: Store, cfg: Config, args: argparse.Namespace) -> None:
    from .server import daemon

    if args.port:
        cfg.serve_port = args.port
    daemon(cfg)


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="hunter", description="Idle-Token Bug Hunter")
    ap.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="enable verbose (DEBUG) logging",
    )
    ap.add_argument(
        "--config",
        type=Path,
        default=None,
        help="path to config.json (default: hunter/config.json)",
    )
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("serve", help="run the triage UI only, no scheduler")
    p.add_argument("--port", type=int, default=None)
    p.set_defaults(fn=cmd_serve)

    p = sub.add_parser(
        "daemon",
        help="run forever: UI + budget-gated scheduler loop",
    )
    p.add_argument("--port", type=int, default=None)
    p.set_defaults(fn=cmd_daemon)

    return ap


def main(argv: list[str] | None = None) -> None:
    args = build_parser().parse_args(argv)

    # Logging: verbose → DEBUG, else INFO (both commands are long-running).
    level = logging.DEBUG if args.verbose else logging.INFO
    logging.basicConfig(format=_LOG_FORMAT, level=level)

    cfg = Config.load(args.config)
    store = Store(cfg)
    try:
        args.fn(store, cfg, args)
    finally:
        store.db.close()
