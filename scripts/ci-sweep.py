#!/usr/bin/env python3
"""Prune target/ down to the build units the current CI job actually used.

Two subcommands, run in this order inside one job:

  prepare   right after target/ is restored, before any cargo command:
            ages the atime of every restored file to 2000-01-01 (mtime kept
            to the nanosecond, since cargo judges freshness by mtime) and
            writes the stamp time.
  sweep     after the last cargo command: keeps every unit that has a file
            under .fingerprint/<unit> whose atime is at or after the stamp,
            and deletes every other unit's files from .fingerprint/, build/,
            deps/ and the profile directory. --dry-run only reports.

Why atime: cargo reads every unit's fingerprint on each build, fresh or not,
so "read after the stamp" means "part of this build". Deletion goes by the
16-hex-digit unit hash in each name, the same rule as cargo-sweep.

Why aging: APFS (macOS) only updates atime on a read when the stored atime
is not newer than mtime, and extraction leaves it newer. Linux (relatime)
works either way. Windows runners ship with last-access updates DISABLED;
the workflow switches them on before the restore.

Why not cargo-sweep itself: on the windows runner it swept 2.5 GiB of
in-use units after a dependency bump, while this rule, reading atime with
os.stat, kept exactly the units that build used. The cause was not
identified. cargo-sweep is also marked unmaintained, and it has no
upstream release binaries.
"""

import argparse
import os
import pathlib
import shutil
import sys
import time

TARGET = pathlib.Path("target")
STAMP = pathlib.Path(".ci-sweep-stamp")
AGED_ATIME_NS = 946684800 * 10**9  # 2000-01-01T00:00:00Z
PROFILE_SUBDIRS = (".fingerprint", "build", "deps", "native")


def unit_hash(name: str) -> str | None:
    """`({prefix}-)?{name}-{16 hex}(.{ext})?` -> the hash, else None."""
    stem = name.split(".", 1)[0]
    _, dash, tail = stem.rpartition("-")
    if not dash or len(tail) != 16:
        return None
    try:
        int(tail, 16)
    except ValueError:
        return None
    return tail


def prepare() -> int:
    start, aged, links, errors = time.time(), 0, 0, 0
    if TARGET.is_dir():
        for root, _dirs, files in os.walk(TARGET):
            for name in files:
                path = os.path.join(root, name)
                if os.path.islink(path):
                    links += 1
                    continue
                try:
                    st = os.stat(path)
                    os.utime(path, ns=(AGED_ATIME_NS, st.st_mtime_ns))
                    aged += 1
                except OSError as e:
                    errors += 1
                    print(f"skip {path}: {e}")
    # Written after the aging, so nothing aged can count as read.
    STAMP.write_text(repr(time.time()))
    print(
        f"aged atime of {aged} files in {time.time() - start:.1f}s "
        f"({links} symlinks skipped, {errors} errors); stamp written"
    )
    return 0


def newest_atime(unit_dir: pathlib.Path) -> float:
    newest = 0.0
    for f in unit_dir.iterdir():
        if f.is_file() and not f.is_symlink():
            newest = max(newest, os.stat(f).st_atime)
    return newest


def size_of(path: pathlib.Path) -> int:
    if path.is_symlink() or path.is_file():
        return path.lstat().st_size
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_size
            except OSError:
                pass
    return total


def remove(path: pathlib.Path) -> None:
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()


def sweep(dry_run: bool) -> int:
    if not STAMP.is_file():
        print(f"no {STAMP}: `prepare` did not run, refusing to sweep", file=sys.stderr)
        return 1
    stamp = float(STAMP.read_text())
    total_bytes, total_removed = 0, 0

    for fp_dir in sorted(TARGET.rglob(".fingerprint")):
        profile = fp_dir.parent
        keep, units = set(), 0
        for unit in fp_dir.iterdir():
            h = unit_hash(unit.name)
            if h is None or not unit.is_dir():
                continue
            units += 1
            if newest_atime(unit) >= stamp:
                keep.add(h)

        removed, removed_bytes = 0, 0
        scan = [profile] + [profile / d for d in PROFILE_SUBDIRS]
        for directory in scan:
            if not directory.is_dir():
                continue
            for entry in directory.iterdir():
                if entry.name in PROFILE_SUBDIRS:
                    continue
                h = unit_hash(entry.name)
                if h is None or h in keep:
                    continue
                removed_bytes += size_of(entry)
                removed += 1
                if not dry_run:
                    remove(entry)

        print(f"{profile}: {units} units, kept {len(keep)}, removed {removed} entries ({removed_bytes / 2**20:.1f} MiB)")
        total_bytes += removed_bytes
        total_removed += removed

    verb = "would remove" if dry_run else "removed"
    print(f"total: {verb} {total_removed} entries, {total_bytes / 2**20:.1f} MiB")
    if not dry_run:
        STAMP.unlink()
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("prepare")
    sweep_cmd = sub.add_parser("sweep")
    sweep_cmd.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    if args.command == "prepare":
        return prepare()
    return sweep(args.dry_run)


if __name__ == "__main__":
    sys.exit(main())
