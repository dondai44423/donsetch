#!/usr/bin/env python3
"""battle.py : DonSheet battle harness.

Runs every PDF in bench/battle-corpus/ (and tests/pdf-corpus/) through:
  1. donsetch extract (the pipeline under test)
  2. pymupdf4llm (the baseline everyone knows)

For each doc records: time, output chars, notes, and a garbage score
(control/PUA chars ratio). Writes per-doc markdown to
bench/results/donsetch/<name>.md and bench/results/pymupdf/<name>.md for
side-by-side review, plus bench/results/report.md (the summary table).

Usage:
  python3 bench/battle.py [--pattern REGEX] [--skip-ocr] [--both|--donsetch-only]

Env:
  DONSETCH_BIN     binary path (default target/release/donsetch)
  DONSHEET_OCR     forwarded (set to "off" via --skip-ocr for speed)
  BATTLE_MAX_PAGES soft page cap via --max-chars fallback (default: full doc)
"""
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("DONSETCH_BIN", str(ROOT / "target/release/donsetch"))
CORPORA = [ROOT / "bench/battle-corpus", ROOT / "tests/pdf-corpus"]
OUT = ROOT / "bench/results"


def garbage_ratio(text: str) -> float:
    if not text:
        return 0.0
    garbage = 0
    for ch in text:
        cp = ord(ch)
        if cp in (0xFFFD,) or (0xE000 <= cp <= 0xF8FF) or (cp < 0x20 and ch not in "\n\t"):
            garbage += 1
    return garbage / len(text)


def donsetch_extract(pdf: Path, skip_ocr: bool) -> dict:
    env = dict(os.environ)
    if skip_ocr:
        env["DONSHEET_OCR"] = "off"
    t0 = time.monotonic()
    proc = subprocess.run(
        [BIN, "extract", "--input", str(pdf), "--max", "8000000"],
        capture_output=True, text=True, timeout=900, env=env,
    )
    dt = time.monotonic() - t0
    md = proc.stdout
    notes = re.findall(r"^\*\[pdf: (.+?)\]\*$", md, re.M)
    body_len = len(md) - (md.index("\nhttp", 0) if "\nhttp" in md else 0)
    return {
        "time": dt,
        "chars": len(md),
        "garbage": garbage_ratio(md),
        "notes": notes[:3],
        "panic": "panicked at" in proc.stderr or proc.returncode not in (0,),  # rough
        "md": md,
        "rc": proc.returncode,
    }


def pymupdf_extract(pdf: Path) -> dict | None:
    try:
        import pymupdf4llm
    except ImportError:
        return None
    try:
        t0 = time.monotonic()
        md = pymupdf4llm.to_markdown(str(pdf))
        dt = time.monotonic() - t0
    except Exception as e:  # pymupdf raises broadly
        return {"time": 0.0, "chars": 0, "garbage": 0.0, "error": str(e), "md": ""}
    return {"time": dt, "chars": len(md), "garbage": garbage_ratio(md), "md": md}


def main() -> int:
    pattern = None
    skip_ocr = False
    donsetch_only = False
    args = sys.argv[1:]
    i = 0
    while i < len(args):
        if args[i] == "--pattern":
            pattern = re.compile(args[i + 1])
            i += 2
        elif args[i] == "--skip-ocr":
            skip_ocr = True
            i += 1
        elif args[i] == "--donsetch-only":
            donsetch_only = True
            i += 1
        else:
            i += 1

    pdfs: list[Path] = []
    for c in CORPORA:
        pdfs.extend(sorted(c.glob("*.pdf")))
    if pattern:
        pdfs = [p for p in pdfs if pattern.search(p.name)]
    if not pdfs:
        print("no pdfs matched")
        return 1

    rows = []
    (OUT / "donsetch").mkdir(parents=True, exist_ok=True)
    (OUT / "pymupdf").mkdir(parents=True, exist_ok=True)

    for pdf in pdfs:
        name = pdf.stem
        ds = donsetch_extract(pdf, skip_ocr)
        (OUT / "donsetch" / f"{name}.md").write_text(ds["md"])
        pm = None if donsetch_only else pymupdf_extract(pdf)
        if pm is not None:
            (OUT / "pymupdf" / f"{name}.md").write_text(pm["md"])
        rows.append((name, pdf.stat().st_size, ds, pm))
        st = f"{name:22s} ds={ds['chars']:>7d}ch {ds['time']:7.2f}s g={ds['garbage']:.4f}"
        if ds["panic"]:
            st += f" PANIC(rc={ds['rc']})"
        if pm:
            st += f" | pm={pm['chars']:>7d}ch {pm['time']:7.2f}s"
        print(st)

    # summary table
    with open(OUT / "report.md", "w") as f:
        f.write("| doc | size | donsetch chars | ds time | ds garbage | pymupdf chars | pm time | pm garbage | ds notes |\n")
        f.write("|---|---|---|---|---|---|---|---|---|\n")
        for name, size, ds, pm in rows:
            pm_chars = f"{pm['chars']}" if pm else "-"
            pm_time = f"{pm['time']:.2f}" if pm else "-"
            pm_g = f"{pm['garbage']:.4f}" if pm else "-"
            notes = "; ".join(ds["notes"])[:90]
            f.write(
                f"| {name} | {size//1024}K | {ds['chars']} | {ds['time']:.2f}s | "
                f"{ds['garbage']:.4f} | {pm_chars} | {pm_time} | {pm_g} | {notes} |\n"
            )
    print(f"\nreport at {OUT/'report.md'}")
    panics = [r for r in rows if r[2]["panic"]]
    if panics:
        print("PANICS:", [p[0] for p in panics])
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
