#!/usr/bin/env python3
"""Discover shipped Jarvis surfaces that need macOS parity evidence."""

import json
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "docs/macos-parity-inventory.json"
ROUTE = re.compile(r"\b(?:router|app)\.(get|post|put|patch|delete)\(\s*(['\"])(.*?)\2")
COMMAND = re.compile(r"^    ([A-Z][A-Za-z0-9]+)(?:\s*\{|\s*\(|\s*,)", re.M)


def discover(root=ROOT):
    found = {}

    def add(kind, name, path):
        identifier = f"{kind}:{name}"
        if identifier in found:
            raise ValueError(f"duplicate capability ID: {identifier}")
        found[identifier] = str(path.relative_to(root))

    for path in sorted((root / "crates").glob("augmentagent-channel-*/Cargo.toml")):
        if path.parent.name != "augmentagent-channel-core":
            add("channel", path.parent.name.removeprefix("augmentagent-channel-"), path)

    main = root / "crates/augmentagent-cli/src/main.rs"
    section = main.read_text().split("enum Cmd {", 1)[1].split("\n}\n", 1)[0]
    for match in COMMAND.finditer(section):
        variant = match.group(1)
        name = re.sub(r"(?<!^)(?=[A-Z])", "-", variant).lower()
        add("cli", name, main)

    for directory in (root / "systemd", root / "scripts/systemd"):
        for path in sorted(directory.glob("augmentagent-*")):
            if path.suffix in (".service", ".timer"):
                add("unit", path.name, path)

    for directory in sorted((root / "sidecars").iterdir()):
        if not directory.is_dir():
            continue
        for filename in ("README.md", "package.json", "pyproject.toml", "go.mod"):
            path = directory / filename
            if path.is_file():
                add("sidecar", directory.name, path)
                break

    for prefix in ("install", "uninstall"):
        for suffix in ("sh", "py"):
            for path in sorted((root / "scripts").glob(f"{prefix}-*.{suffix}")):
                add("installer", path.name, path)

    for path in sorted((root / "src").rglob("*.ts")):
        for method, _quote, route in ROUTE.findall(path.read_text()):
            # The same path on different routers is a separate contract.
            module = path.relative_to(root / "src").with_suffix("").as_posix()
            add("route", f"{module}:{method.upper()}:{route}", path)

    for name in ("codex-tool-bridge.py", "codex-command-sandbox.py",
                 "codex-build-vm.py", "provider-supervisor.py"):
        path = root / "scripts" / name
        if path.is_file():
            add("tool", name, path)

    return found


def valid_evidence(evidence):
    if not isinstance(evidence, dict):
        return False
    required = ("commit", "macos_version", "architecture", "test_command", "result", "artifact")
    if any(not isinstance(evidence.get(key), str) or
           evidence[key].strip().lower() in ("", "todo", "tbd", "...") for key in required):
        return False
    return (re.fullmatch(r"[0-9a-f]{40}", evidence["commit"]) is not None
            and evidence["macos_version"].lower().startswith("macos ")
            and evidence["architecture"] in ("arm64", "x86_64")
            and evidence["result"] == "pass")


def validate(rows, found):
    errors = []
    catalog = {}
    for row in rows:
        identifier = row.get("id")
        if identifier in catalog:
            errors.append(f"duplicate inventory row: {identifier}")
        catalog[identifier] = row
    for identifier in sorted(found.keys() - catalog.keys()):
        errors.append(f"untracked capability: {identifier}")
    for identifier in sorted(catalog.keys() - found.keys()):
        errors.append(f"stale capability: {identifier}")
    for identifier in sorted(found.keys() & catalog.keys()):
        row = catalog[identifier]
        if row.get("source") != found[identifier]:
            errors.append(f"source changed: {identifier}")
        for key in ("linux_baseline", "macos_target", "prerequisites", "owner_issue", "test_plan", "status"):
            if not row.get(key):
                errors.append(f"{identifier} has no {key}")
        if row.get("status") not in ("unverified", "gap", "in_progress", "verified"):
            errors.append(f"invalid status: {identifier}")
        if row.get("status") == "verified" and not valid_evidence(row.get("evidence")):
            errors.append(f"verified without evidence: {identifier}")
    return errors


def main():
    found = discover()
    rows = json.loads(MANIFEST.read_text())
    errors = validate(rows, found)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"macOS parity inventory covers {len(found)} discovered capabilities; verified: "
          f"{sum(row['status'] == 'verified' for row in rows)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
