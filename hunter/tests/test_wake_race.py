"""Regression: _wake signal set during a cycle must not be discarded.

The daemon loop (server.py:454-503) clears _wake AFTER releasing
_cycle_lock (line 495), discarding any signal set by the UI override
handler (line 353) during the cycle.
"""

from __future__ import annotations

import threading

from hunter.server import _cycle_lock, _wake


def _daemon_post_cycle_sequence() -> bool:
    """Reproduce the exact post-cycle sequence from server.py:492-499.

    Returns whether the sleep loop would see the wake signal.
    """
    # Line 492: _cycle_lock.release() -- already released by caller
    # Line 495: _wake.clear()
    _wake.clear()
    # Line 499: while ... not _wake.is_set(): ...
    return _wake.is_set()


class TestWakeSignalRace:
    def setup_method(self) -> None:
        _wake.clear()
        if _cycle_lock.locked():
            _cycle_lock.release()

    def test_wake_set_during_cycle_survives_post_cycle(self) -> None:
        """Signal set during cycle must be visible to the sleep loop.

        Sequence:
          1. Daemon acquires lock, runs cycle        (line 456-459)
          2. UI handler calls _wake.set()             (line 353)
          3. Daemon releases lock                     (line 492)
          4. Daemon clears wake                       (line 495)  <-- BUG
          5. Sleep loop checks _wake.is_set()         (line 499)

        Step 4 discards the signal from step 2.
        """
        # Step 1: daemon acquires lock
        assert _cycle_lock.acquire(blocking=False)

        # Step 2: UI sets wake during cycle
        _wake.set()

        # Step 3: cycle finishes, releases lock
        _cycle_lock.release()

        # Steps 4-5: reproduce the daemon's post-cycle path
        wake_visible = _daemon_post_cycle_sequence()

        assert wake_visible, (
            "_wake signal set during cycle was cleared at line 495 "
            "before the sleep loop could see it -- override processing "
            "delayed by up to sleep_s (15-30 min)"
        )
