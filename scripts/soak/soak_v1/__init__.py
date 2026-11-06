"""Soak Harness V1 — request/response completeness harness for local-mcp MCP stdio.

Design of record: ``docs/SOAK_HARNESS_V1_DESIGN.md`` (PR #26).
This package is Python 3.11+ standard library only. It never imports product
code and never uses the operator's session or state directory.
"""

__all__ = ["ledger", "driver", "resources", "scenarios", "failure_injection", "report", "harness"]
