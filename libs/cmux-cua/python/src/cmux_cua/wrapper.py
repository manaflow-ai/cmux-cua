"""Subprocess wrapper for cmux-cua binary with stdio passthrough."""

import os
import sys
import subprocess
from pathlib import Path
from typing import Optional


def get_binary_path() -> Path:
    """Get the path to the bundled cmux-cua binary.

    Returns:
        Path to the cmux-cua executable.

    Raises:
        FileNotFoundError: If the binary is not found in the package.
    """
    # Binary is bundled in the package at: cmux_cua/bin/cmux-cua[.exe]
    package_dir = Path(__file__).parent

    if sys.platform == "win32":
        binary_name = "cmux-cua.exe"
    else:
        binary_name = "cmux-cua"

    binary_path = package_dir / "bin" / binary_name

    if not binary_path.exists():
        raise FileNotFoundError(
            f"cmux-cua binary not found at {binary_path}. "
            f"This package may not have been built correctly for {sys.platform}."
        )

    # Ensure binary is executable on Unix
    if sys.platform != "win32":
        os.chmod(binary_path, 0o755)

    return binary_path


def run_cmux_cua(args: Optional[list[str]] = None) -> int:
    """Execute cmux-cua binary with stdio passthrough.

    Args:
        args: Command-line arguments to pass to cmux-cua.
              If None, uses sys.argv[1:].

    Returns:
        Exit code from the cmux-cua process.
    """
    if args is None:
        args = sys.argv[1:]

    binary_path = get_binary_path()

    try:
        # Run with direct stdio inheritance - no buffering, no capturing
        result = subprocess.run(
            [str(binary_path), *args],
            stdin=sys.stdin,
            stdout=sys.stdout,
            stderr=sys.stderr,
        )
        return result.returncode
    except KeyboardInterrupt:
        # Standard SIGINT exit code
        return 130
    except Exception as e:
        print(f"Error executing cmux-cua: {e}", file=sys.stderr)
        return 1
