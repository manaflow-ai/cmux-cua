"""CLI entry point for cmux-cua Python wrapper.

This module is invoked when running:
    python -m cmux_cua [args...]
or via the installed script:
    cmux-cua [args...]
"""

import sys
from .wrapper import run_cmux_cua


def main() -> None:
    """Main entry point for the cmux-cua CLI."""
    exit_code = run_cmux_cua()
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
