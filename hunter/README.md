# hunter — Idle-Token Bug Hunter

Self-hosted detail.dev-style bug hunter funded by spare Claude-subscription
capacity. Registers repos, hunts latent bugs with headless `omp -p` workers,
you triage in a local UI, fix workers ship draft PRs. Design + measured
grounding: ../IDEA.md, ../EXPERIMENT-*.md.

Python 3.14 stdlib only. State in `data/hunter.db` (SQLite).

## Run

```sh
cd hunter
python3 -m hunter init
python3 -m hunter add-repo NAME https://github.com/owner/repo [--branch main]
python3 -m hunter daemon         # run forever: UI (:8377) + scheduler loop
python3 -m hunter cycle          # one manual scheduling step
python3 -m hunter serve          # UI only, no scheduler
```

Other commands: `repos`, `findings [--status S]`, `verdict FID STATUS
[--reason ...]`, `ingest FILE --repo NAME`, `jobs`, `events`, `hunt REPO
[--force]`, `fix FID`.

Production shape: `hunter.service` (systemd user unit, installed) runs the
daemon permanently. It idles at zero token cost and wakes on a policy:
after a job → 60s (drain the queue); budget denied → sleep to the 5h reset;
idle → 15min. Every wake passes the budget gate before spending anything.

## How it decides

- **Budget** (`budget.py`): reads omp's local usage mirror
  (`~/.omp/agent/agent.db:usage_history`). Denies when the 5h window is near
  exhaustion or any 7-day limit crosses the interactive reserve line
  (default: scavenging stops at 75% weekly). Stale data → conservative caps.
- **Caps** (`runner.py`): workers are metered EXTERNALLY by tailing their
  session JSONL; SIGTERM at the token cap. omp may reuse a session file for
  a repeated cwd — metering filters by spawn timestamp.
- **Dedup**: fingerprint-unique findings; rejected/wontfix verdicts (with
  reasons) are injected into future hunt prompts as the suppression corpus;
  open findings are listed so the hunter must argue novelty.
- **Playbooks** (`playbooks/*.md`): hunt output is incremental (kill-safe);
  fixes are evidence-first with commit-per-step (kill = resumable).

## Config

`config.json`: per-kind token caps and wall clocks, weekly interactive
reserve, 5h deny threshold, UI port. Findings statuses: new → queued →
fixing → pr_open → merged, or rejected / wontfix / note.
