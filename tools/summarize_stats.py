#!/usr/bin/env python3
"""Summarize Fjern's opt-in RDP/VNC interval diagnostics."""
import argparse
import re
from pathlib import Path


LINE = re.compile(r"^(RDP|VNC) stats: (.*)$")
FIELD = re.compile(r"([a-z/-]+)=([^ ]+)")


def parse(text):
    rows = []
    for line in text.splitlines():
        match = LINE.match(line)
        if not match:
            continue
        values = dict(FIELD.findall(match.group(2)))
        needed = ("updates/s", "paint-attempts/s", "rss-mib")
        if not all(key in values for key in needed):
            raise ValueError("incomplete Fjern stats line")
        try:
            row = {key: float(values[key]) for key in needed}
            for key in (
                "published/s", "replaced/s", "published-row-changes/s",
                "picked/s", "paint-new/s", "paint-row-changes/s",
                "paint-max-ms", "snapshot-wait-max-ms", "input-queue-max-ms",
                "event-max-ms",
            ):
                if key in values:
                    row[key] = float(values[key])
        except ValueError as error:
            raise ValueError("invalid number in Fjern stats line") from error
        rows.append((match.group(1), row))
    if not rows:
        raise ValueError("no Fjern stats lines found")
    if len({kind for kind, _ in rows}) != 1:
        raise ValueError("log mixes RDP and VNC sessions")
    return rows


def summarize(rows):
    values = [row for _, row in rows]
    count = len(values)
    report = {
        "protocol": rows[0][0],
        "intervals": count,
        "mean_updates_per_second": sum(row["updates/s"] for row in values) / count,
        "mean_paint_attempts_per_second": sum(row["paint-attempts/s"] for row in values) / count,
        "peak_rss_mib": max(row["rss-mib"] for row in values),
    }
    for field, label in (
        ("published/s", "mean_published_per_second"),
        ("replaced/s", "mean_replaced_per_second"),
        ("published-row-changes/s", "mean_published_row_changes_per_second"),
        ("picked/s", "mean_picked_per_second"),
        ("paint-new/s", "mean_paint_new_per_second"),
        ("paint-row-changes/s", "mean_paint_row_changes_per_second"),
    ):
        present = [row[field] for row in values if field in row]
        if present:
            report[label] = sum(present) / len(present)
    for field, label in (
        ("paint-max-ms", "max_paint_ms"),
        ("snapshot-wait-max-ms", "max_snapshot_wait_ms"),
        ("input-queue-max-ms", "max_input_queue_ms"),
        ("event-max-ms", "max_event_batch_ms"),
    ):
        present = [row[field] for row in values if field in row]
        if present:
            report[label] = max(present)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    args = parser.parse_args()
    report = summarize(parse(args.log.read_text()))
    for key, value in report.items():
        print(f"{key}: {value}")


if __name__ == "__main__":
    main()
