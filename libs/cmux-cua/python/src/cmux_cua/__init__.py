"""Python wrapper for cmux-cua - cross-platform MCP server.

This package provides a thin Python wrapper around the cmux-cua Rust binary,
enabling pip-installable access to the MCP server for computer-use automation.
"""

__version__ = "0.7.1"

from .wrapper import run_cmux_cua, get_binary_path

__all__ = ["run_cmux_cua", "get_binary_path", "__version__"]
