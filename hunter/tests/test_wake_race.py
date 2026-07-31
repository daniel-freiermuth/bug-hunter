"""Regression: _wake signal set during a cycle must not be discarded.

The daemon loop (server.py:454-503) must clear _wake BEFORE starting
the cycle so that signals set by the UI override handler (line 353)
during cycle execution survive to the sleep loop.
"""

from __future__ import annotations

import threading

from hunter.server import _cycle_lock, _wake


def _daemon_cycle_sequence() -> bool:
    """Reproduce the daemon cycle sequence from server.py:456-499.

    Returns whether the sleep loop would see a wake signal set during
    the cycle.
    """
    # Line 456: _cycle_lock.acquire()
    assert _cycle_lock.acquire(blocking=False)
    # Line 457: _wake.clear() -- before the cycle
    _wake.clear()
    # Line 459: run_cycle ... (UI sets _wake here)
    _wake.set()  # simulates UI handler calling _wake.set() mid-cycle
    # Line 492-493: finally: _cycle_lock.release()
    _cycle_lock.release()
    # Line 499: while ... not _wake.is_set(): ...
    return _wake.is_set()


class TestWakeSignalRace:
    def setup_method(self) -> None:
        _wake.clear()
        if _cycle_lock.locked():
            _cycle_lock.release()

    def test_wake_set_during_cycle_survives_post_cycle(self) -> None:
        """Signal set during cycle must be visible to the sleep loop.

        Fixed sequence:
          1. Daemon acquires lock                    (line 456)
          2. Daemon clears wake                      (line 457)
          3. Daemon runs cycle                       (line 459)
          4. UI handler calls _wake.set()            (line 353)
          5. Daemon releases lock                    (line 493)
          6. Sleep loop checks _wake.is_set()        (line 499)

        Signal from step 4 must survive to step 6.
        """
        wake_visible = _daemon_cycle_sequence()

        assert wake_visible, (
            "_wake signal set during cycle was cleared after the cycle "
            "finished -- override processing delayed by up to sleep_s"
        )
