#!/usr/bin/env python3
"""Offline guard for new URL hosts in application-owned source.

This inventories URL literals, not data flow: provider URLs remain user supplied,
and claim namespaces, comments and preview values are not network requests.
Never print a candidate URL, which could contain accidentally pasted credentials.
"""
from __future__ import annotations

import ipaddress
import json
import re
import sys
from pathlib import Path
from urllib.parse import urlsplit

URL = re.compile(r"(?:https?|wss?)://[^\s\"'<>`\\;]+", re.IGNORECASE)
EXTENSIONS = {".rs", ".ts", ".tsx", ".m", ".h"}
RESERVED = ("example.com", "example.net", "example.org")


def sources(root: Path):
    for folder in ("src", "src-tauri/src", "src-tauri/native"):
        for path in sorted((root / folder).rglob("*")):
            if path.is_file() and path.suffix in EXTENSIONS:
                test_file = ".test." in path.name or path.name.endswith("_tests.rs") or path.name == "test-setup.ts"
                if "fixtures" not in path.parts and "tests" not in path.parts and not test_file:
                    yield path
    for name in ("src-tauri/build.rs", "src-tauri/tauri.conf.json"):
        path = root / name
        if path.is_file():
            yield path


def local_or_fixture(host: str, local_hosts: list[str]) -> bool:
    if host == "localhost" or host in local_hosts or host.endswith((".invalid", ".test", ".example")):
        return True
    if any(host == base or host.endswith("." + base) for base in RESERVED):
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def check(root: Path) -> list[str]:
    policy = json.loads((root / "scripts/network-hosts.json").read_text(encoding="utf-8"))
    errors = []
    for path in sources(root):
        relative = path.relative_to(root).as_posix()
        source = path.read_text(encoding="utf-8")
        for match in URL.finditer(source):
            # Templated authorities are supplied by configuration, not literals.
            authority = match.group().split("://", 1)[1].split("/", 1)[0]
            if "{" in authority or "$" in authority:
                continue
            try:
                host = urlsplit(match.group()).hostname
            except ValueError:
                host = None
            if host and local_or_fixture(host, policy["local_hosts"]):
                continue
            entry = policy["hosts"].get(host)
            if entry and ("files" not in entry or relative in entry["files"]):
                continue
            line = source.count("\n", 0, match.start()) + 1
            errors.append(f"{relative}:{line}: unregistered hard-coded URL host")
    return errors


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    errors = check(root)
    for error in errors:
        print(error, file=sys.stderr)
    if not errors:
        print("Network host inventory matches application source.")
    return bool(errors)


if __name__ == "__main__":
    sys.exit(main())
