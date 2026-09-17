#!/usr/bin/env python3

import argparse
import re
import sys
from pathlib import Path


VCPU_RESULT_PATTERN = re.compile(r"VCPU_PERF_RESULT\s+(?P<fields>.+)")
IVC_RESULT_PATTERN = re.compile(r"AXVISOR_IVC_BENCH_RESULT=(?P<status>\S+)\s*(?P<fields>.*)")
FIELD_PATTERN = re.compile(r"(?P<key>[A-Za-z_][A-Za-z0-9_]*)=(?P<value>\[[^\]]*\]|\S+)")


def parse_fields(text: str) -> dict[str, str]:
    return {match.group("key"): match.group("value") for match in FIELD_PATTERN.finditer(text)}


def markdown_row(name: str, fields: dict[str, str]) -> str:
    ordered = [
        key
        for key in (
            "status",
            "blocks_per_second",
            "baseline",
            "threshold",
            "cases",
            "testTime",
            "bytes",
            "chunks",
            "samples",
        )
        if key in fields
    ]
    ordered.extend(key for key in sorted(fields) if key not in ordered)
    details = "<br>".join(f"`{key}`: `{fields[key]}`" for key in ordered)
    return f"| {name} | {details} |"


def render_report(check_id: str, check_name: str, log_text: str) -> str:
    rows: list[str] = []
    for line in log_text.splitlines():
        line = line.removeprefix("[VM 1] ").strip()
        if match := VCPU_RESULT_PATTERN.search(line):
            rows.append(markdown_row("vCPU throughput", parse_fields(match.group("fields"))))
        if match := IVC_RESULT_PATTERN.search(line):
            fields = {"status": match.group("status")}
            fields.update(parse_fields(match.group("fields")))
            rows.append(markdown_row("AXIVC benchmark", fields))

    if not rows:
        raise ValueError("no supported performance result lines found")

    return "\n".join(
        [
            f"### {check_name}",
            "",
            f"`{check_id}`",
            "",
            "| Metric | Result |",
            "| --- | --- |",
            *rows,
            "",
        ]
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Render CI performance results")
    parser.add_argument("--check-id", required=True)
    parser.add_argument("--check-name", required=True)
    parser.add_argument("--log", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        log_text = args.log.read_text(encoding="utf-8", errors="replace")
        report = render_report(args.check_id, args.check_name, log_text)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(report, encoding="utf-8")
    except (OSError, ValueError) as error:
        print(f"performance report failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
