# Experiment 2 result — 2026-07-23T14:02:10

status: COMPLETED

pre-opener 5h state: {'recorded_at': 1784801410327, 'used_fraction': 0.0, 'status': 'ok', 'resets_at': None}
opener: {'duration_s': 6.6, 'output_tail': 'ok', 'calls': 1, 'input': 2, 'output': 4, 'cacheRead': 0, 'cacheWrite': 31678}
post-opener 5h state: {'recorded_at': 1784801410327, 'used_fraction': 0.0, 'status': 'ok', 'resets_at': None}

Interpretation guide: success = post shows a fresh window (low used_fraction,
new resets_at ≈ opener time + 5h). Opener cost fields show what the cheapest
`omp -p` opener actually costs (session floor applies — compare against a
future direct-API opener).
