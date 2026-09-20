#!/usr/bin/env python3
"""Measure a labelled CCat decision run without sending any provider requests."""
import argparse
import json
import sys
from collections import Counter
from pathlib import Path

OUTCOMES = {"allow", "review", "block"}


def load(path: Path):
    rows = []
    for number, line in enumerate(path.read_text().splitlines(), 1):
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise ValueError(f"line {number}: invalid JSON") from error
        if set(row) != {"case_id", "policy_id", "expected", "predicted"}:
            raise ValueError(f"line {number}: fields must be case_id, policy_id, expected, predicted")
        if not isinstance(row["case_id"], str) or not isinstance(row["policy_id"], str):
            raise ValueError(f"line {number}: case_id and policy_id must be strings")
        if row["expected"] not in OUTCOMES or row["predicted"] not in OUTCOMES:
            raise ValueError(f"line {number}: expected and predicted must be allow, review, or block")
        rows.append(row)
    return rows


def report(rows):
    matrix = Counter((row["expected"], row["predicted"]) for row in rows)
    false_allows = sum(count for (expected, predicted), count in matrix.items() if expected != "allow" and predicted == "allow")
    false_blocks = sum(count for (expected, predicted), count in matrix.items() if expected == "allow" and predicted == "block")
    correct = sum(count for (expected, predicted), count in matrix.items() if expected == predicted)
    return {
        "examples": len(rows),
        "correct": correct,
        "accuracy": correct / len(rows) if rows else 0.0,
        "false_allows": false_allows,
        "false_blocks": false_blocks,
        "confusion_matrix": {f"{expected}->{predicted}": count for (expected, predicted), count in sorted(matrix.items())},
    }


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--min-examples", type=int, default=100)
    parser.add_argument("--max-false-allows", type=int, default=0)
    args = parser.parse_args(argv)
    try:
        rows = load(args.input)
        result = report(rows)
    except (OSError, ValueError) as error:
        print(f"CCat calibration invalid: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True))
    if result["examples"] < args.min_examples:
        print(f"CCat calibration needs at least {args.min_examples} labelled examples.", file=sys.stderr)
        return 1
    if result["false_allows"] > args.max_false_allows:
        print("CCat calibration exceeds the permitted false-allow count.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
