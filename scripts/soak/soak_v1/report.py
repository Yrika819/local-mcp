"""Report writer (Soak Harness V1, Slice 3/4).

Writes ``soak-report-<seed>-<sha>-<platform>.json`` and a Markdown companion.
Reports contain no session ids, no user home paths, and no payload content.
"""

from __future__ import annotations

import json
import os


def write_report(run_dir: str, report: dict) -> tuple:
    platform = report.get("platform", "unknown").replace("/", "-")
    seed = report.get("seed", "noseed")
    sha = (report.get("commit_sha") or "nocommit")[:12]
    json_name = f"soak-report-{seed}-{sha}-{platform}.json"
    json_path = os.path.join(run_dir, json_name)
    with open(json_path, "w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2, sort_keys=True, default=str)
        handle.write("\n")
    md_path = os.path.join(run_dir, json_name.replace(".json", ".md"))
    with open(md_path, "w", encoding="utf-8") as handle:
        handle.write(_markdown(report))
    return json_path, md_path


def _markdown(report: dict) -> str:
    lines = []
    lines.append("# Soak Harness V1 report\n")
    lines.append(f"- commit: `{report.get('commit_sha')}`")
    lines.append(f"- platform: `{report.get('platform')}` / runner: `{report.get('runner')}`")
    lines.append(f"- seed: `{report.get('seed')}`; duration: {report.get('duration_s')} s")
    lines.append(f"- verdict: **{report.get('verdict')}**")
    lines.append("")
    metrics = report.get("metrics", {})
    lines.append("## Ledger metrics (§5)\n")
    lines.append("| Metric | Value |")
    lines.append("| --- | --- |")
    for key in sorted(metrics):
        lines.append(f"| {key} | {metrics[key]} |")
    lines.append("")
    lines.append("## Resource measurement\n")
    lines.append(f"```json\n{json.dumps(report.get('resources', {}), indent=2, default=str)}\n```")
    lines.append("")
    lat = report.get("latency", {})
    lines.append("## Latency\n")
    lines.append(f"```json\n{json.dumps(lat, indent=2, default=str)}\n```")
    lines.append("")
    lines.append("## Scenarios\n")
    lines.append("```json\n" + json.dumps(report.get("scenarios", {}), indent=2, default=str) + "\n```")
    lines.append("")
    lines.append("## Failure injection\n")
    lines.append("```json\n" + json.dumps(report.get("failure_injection", []), indent=2, default=str) + "\n```")
    lines.append("")
    lines.append("## NOT RUN ledger\n")
    for entry in report.get("not_run", []):
        lines.append(f"- {entry}")
    lines.append("")
    lines.append("## Limitations\n")
    for entry in report.get("limitations", []):
        lines.append(f"- {entry}")
    lines.append("")
    if report.get("violations"):
        lines.append("## Violations (run FAIL)\n")
        for entry in report["violations"]:
            lines.append(f"- {entry}")
        lines.append("")
    return "\n".join(lines)


def redact_report(report: dict, forbidden: list) -> dict:
    """Defense in depth: strip forbidden strings from the report tree."""
    if not forbidden:
        return report
    serialized = json.dumps(report, default=str)
    for secret in forbidden:
        if secret and secret in serialized:
            serialized = serialized.replace(secret, "[REDACTED]")
    try:
        return json.loads(serialized)
    except json.JSONDecodeError:
        return report
