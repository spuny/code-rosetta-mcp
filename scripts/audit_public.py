#!/usr/bin/env python3
"""Reject known internal references and credential shapes in the proposed public tree."""
from pathlib import Path
import argparse
import re
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--private-pattern", action="append", default=[])
    args = parser.parse_args()
    files = subprocess.check_output([
        "git", "ls-files", "-z", "--cached", "--others", "--exclude-standard",
    ]).decode().split("\0")
    internal = re.compile(r"https?://[^\s/]*\.int\.|/(?:Users|home)/[^/\s]+/", re.I)
    private = [re.compile(pattern, re.I) for pattern in args.private_pattern]
    credentials = re.compile(
        r"glpat-[A-Za-z0-9_-]{10,}|gh[pousr]_[A-Za-z0-9]{20,}|"
        r"github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|"
        r"AKIA[0-9A-Z]{16}|-----BEGIN (?:RSA |OPENSSH |EC )?PRIVATE KEY-----"
    )
    failures = []
    reviewed_fixture = "crates/rosetta-parse/tests/fixtures/yaml/secrets.yaml"
    scanned = 0
    for name in sorted(set(files)):
        if not name or name == "scripts/audit_public.py":
            continue
        path = Path(name)
        if not path.is_file():
            continue
        if "rosetta-ui" in path.parts or "sourcebot" in path.parts or "sb_api" in path.parts:
            failures.append(f"{name}: excluded compatibility/UI path")
        try:
            text = path.read_text()
        except UnicodeDecodeError:
            failures.append(f"{name}: binary file requires publication review")
            continue
        scanned += 1
        for number, line in enumerate(text.splitlines(), 1):
            if internal.search(line) or any(pattern.search(line) for pattern in private):
                failures.append(f"{name}:{number}: internal reference (value withheld)")
            if credentials.search(line):
                if name == reviewed_fixture and "-----BEGIN PRIVATE KEY-----\n  abc\n  -----END PRIVATE KEY-----" in text:
                    continue
                failures.append(f"{name}:{number}: credential-shaped value (redacted)")
        if name.startswith("crates/") and re.search(r"sourcebot|saucebot", text, re.I):
            failures.append(f"{name}: compatibility/provenance reference requires review")
    if failures:
        raise SystemExit("\n".join(failures))
    print(f"Publication pattern audit passed: {scanned} text files; one reviewed synthetic key fixture.")
    print("This checks selected patterns and exclusions, not ownership or all possible secrets.")


if __name__ == "__main__":
    main()
