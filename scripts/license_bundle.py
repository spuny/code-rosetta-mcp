#!/usr/bin/env python3
"""Collect dependency license texts for the resolved build into an artifact directory."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    parser.add_argument("--target")
    args = parser.parse_args()
    target = args.target
    if not target:
        version = subprocess.check_output(["rustc", "-vV"], text=True)
        target = next(line.split(": ", 1)[1] for line in version.splitlines() if line.startswith("host: "))
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--format-version", "1",
        "--filter-platform", target,
    ], text=True))
    resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
    args.output.mkdir(parents=True, exist_ok=True)
    summaries = []
    missing = []
    for package in metadata["packages"]:
        if package["id"] not in resolved or package["id"] in metadata["workspace_members"]:
            continue
        root = Path(package["manifest_path"]).parent
        texts = [p for p in root.rglob("*") if p.is_file() and p.name.lower().startswith(("license", "licence", "copying", "notice", "copyright", "unlicense"))]
        if package.get("license_file"):
            texts.append(root / package["license_file"])
        texts = sorted(set(texts))
        supplements = Path("LICENSES/dependencies") / f"{package['name']}-{package['version']}"
        supplemental_texts = sorted(supplements.glob("*")) if supplements.exists() else []
        if not texts and not supplemental_texts:
            missing.append(f"{package['name']} {package['version']}")
            continue
        destination = args.output / f"{package['name']}-{package['version']}"
        destination.mkdir(exist_ok=True)
        for text in texts:
            relative = text.relative_to(root)
            output = destination / relative
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(text, output)
        for text in supplemental_texts:
            shutil.copyfile(text, destination / text.name)
        if supplemental_texts:
            shutil.copyfile(root / "Cargo.toml", destination / "Cargo.toml")
        summaries.append(f"{package['name']} {package['version']}: {package.get('license') or 'see license files'}")
    if missing:
        raise SystemExit("Missing dependency license texts; review before distribution: " + ", ".join(missing))
    (args.output / "INDEX.txt").write_text("\n".join(sorted(summaries)) + "\n")
    print(f"Bundled license texts for {len(summaries)} dependencies in {args.output}")


if __name__ == "__main__":
    main()
