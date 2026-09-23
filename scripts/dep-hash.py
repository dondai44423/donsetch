#!/usr/bin/env python3
"""Print a 16-hex-digit hash of the dependency inputs, ignoring donsetch's own version.

CI keys its build caches on this, so a release commit (which only bumps the
version) keeps an exact cache hit, while a real dependency, feature or profile
change produces a new key.

Mirrors Swatinem/rust-cache's lockfile hashing: manifests are parsed, not
regex-edited, `[package].version` and path-dependency versions are zeroed, and
lockfile entries without a registry `source`/`checksum` (the workspace's own
crates) are dropped.

Files are read as UTF-8 bytes explicitly: a bare `read_text()` decodes with the
locale code page (cp1252 on the Windows runners), and the manifests contain
non-ASCII, so the same checkout hashed differently on Windows.

Only tracked files count (`git ls-files`), so a restored `target/` or other
untracked `Cargo.toml` can never leak into the key.
"""

import hashlib
import json
import subprocess
import sys
import tomllib

DEP_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")


def tracked(pattern: str) -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "-z", "--", pattern],
        check=True,
        capture_output=True,
    ).stdout
    return sorted(p for p in out.decode("utf-8").split("\0") if p)


def load(path: str) -> dict:
    with open(path, "rb") as f:
        return tomllib.load(f)


def strip_path_dep_versions(deps: dict) -> None:
    for dep in deps.values():
        if isinstance(dep, dict) and "path" in dep:
            dep["version"] = "0.0.0"
            dep["path"] = ""


def normalize_manifest(manifest: dict) -> dict:
    package = manifest.get("package")
    if isinstance(package, dict) and "version" in package:
        package["version"] = "0.0.0"
    tables = [manifest]
    tables += [t for t in manifest.get("target", {}).values() if isinstance(t, dict)]
    for table in tables:
        for section in DEP_SECTIONS:
            deps = table.get(section)
            if isinstance(deps, dict):
                strip_path_dep_versions(deps)
    return manifest


def canonical(value) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def main() -> int:
    h = hashlib.sha256()

    lock = load("Cargo.lock")
    registry_packages = [
        p for p in lock.get("package", []) if "source" in p or "checksum" in p
    ]
    h.update(b"Cargo.lock\0")
    h.update(canonical(registry_packages))

    for path in tracked("*Cargo.toml"):
        h.update(path.encode("utf-8") + b"\0")
        h.update(canonical(normalize_manifest(load(path))))

    print(h.hexdigest()[:16])
    return 0


if __name__ == "__main__":
    sys.exit(main())
