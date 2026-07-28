"""Tests for hunter.util — shared subprocess wrapper."""

from hunter.util import run_cmd


def test_run_cmd_success():
    rc, out = run_cmd(["echo", "hello"])
    assert rc == 0
    assert out == "hello"


def test_run_cmd_failure():
    rc, _out = run_cmd(["false"])
    assert rc != 0


def test_run_cmd_timeout():
    rc, out = run_cmd(["sleep", "10"], timeout=1)
    assert rc == 124
    assert "timeout" in out


def test_run_cmd_missing_binary():
    rc, _out = run_cmd(["__nonexistent_binary_xyz__"])
    assert rc == 127


def test_run_cmd_with_cwd(tmp_path):
    rc, out = run_cmd(["pwd"], cwd=str(tmp_path))
    assert rc == 0
    assert str(tmp_path) in out


def test_run_cmd_strips_output():
    rc, out = run_cmd(["echo", "  spaces  "])
    assert rc == 0
    assert out == "spaces"


def test_run_cmd_merges_stderr():
    rc, out = run_cmd(["sh", "-c", "echo err >&2"])
    assert rc == 0
    assert "err" in out
