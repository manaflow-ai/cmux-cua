"""Tests for the cmux-cua Python wrapper."""

import subprocess
import sys
from pathlib import Path
from unittest.mock import Mock, patch

import pytest


def test_get_binary_path():
    """Test that get_binary_path returns a valid path."""
    from cmux_cua.wrapper import get_binary_path

    # This will raise FileNotFoundError if binary doesn't exist
    # In CI, we need to build the package first for this to pass
    try:
        binary_path = get_binary_path()
        assert binary_path.exists()
        assert binary_path.name in ("cmux-cua", "cmux-cua.exe")
    except FileNotFoundError:
        # Expected in development without building
        pytest.skip("Binary not bundled yet (run build_wheel.py first)")


def test_run_cmux_cua_version(monkeypatch):
    """Test running cmux-cua --version through the wrapper."""
    from cmux_cua.wrapper import run_cmux_cua, get_binary_path

    try:
        binary_path = get_binary_path()
    except FileNotFoundError:
        pytest.skip("Binary not bundled yet")

    # Run with --version
    exit_code = run_cmux_cua(["--version"])
    assert exit_code == 0


def test_wrapper_preserves_exit_code():
    """Test that the wrapper preserves the binary's exit code."""
    from cmux_cua.wrapper import run_cmux_cua, get_binary_path

    try:
        binary_path = get_binary_path()
    except FileNotFoundError:
        pytest.skip("Binary not bundled yet")

    # Invalid command should return non-zero
    exit_code = run_cmux_cua(["--this-flag-does-not-exist"])
    assert exit_code != 0


@patch("cmux_cua.wrapper.subprocess.run")
@patch("cmux_cua.wrapper.get_binary_path")
def test_subprocess_args(mock_get_binary, mock_run):
    """Test that subprocess is called with correct arguments."""
    from cmux_cua.wrapper import run_cmux_cua

    mock_binary = Path("/fake/path/cmux-cua")
    mock_get_binary.return_value = mock_binary
    mock_run.return_value = Mock(returncode=0)

    run_cmux_cua(["mcp", "--help"])

    mock_run.assert_called_once()
    call_args = mock_run.call_args
    assert call_args[0][0] == [str(mock_binary), "mcp", "--help"]
    assert call_args[1]["stdin"] == sys.stdin
    assert call_args[1]["stdout"] == sys.stdout
    assert call_args[1]["stderr"] == sys.stderr


@patch("cmux_cua.wrapper.subprocess.run")
@patch("cmux_cua.wrapper.get_binary_path")
def test_keyboard_interrupt_handling(mock_get_binary, mock_run):
    """Test that KeyboardInterrupt returns exit code 130."""
    from cmux_cua.wrapper import run_cmux_cua

    mock_binary = Path("/fake/path/cmux-cua")
    mock_get_binary.return_value = mock_binary
    mock_run.side_effect = KeyboardInterrupt()

    exit_code = run_cmux_cua(["mcp"])
    assert exit_code == 130
