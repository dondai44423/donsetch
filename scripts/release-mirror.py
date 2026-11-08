#!/usr/bin/env python3
"""Local release-asset mirror for a build, for when GitHub's download path 504s.

GitHub's `releases/download` transfer path intermittently answers 504 while the
REST API asset URL serves the same bytes (2026-11-08: every v5.1.0 release leg
failed on it). This fetches the pinned assets through `gh api` and serves them
over loopback in the layout the consumers expect:

  /v8/v<ver>/<name>                 -> point RUSTY_V8_MIRROR at http://127.0.0.1:PORT/v8
  /asset/<owner>/<repo>/<tag>/<name> -> a plain URL for a pinned prebuilt

Usage:
  python3 scripts/release-mirror.py --port 18777 [--v8 150.4.0] [--asset owner/repo/tag/name]...
Prints the environment lines to export, then serves until killed.
"""

import argparse
import functools
import http.server
import os
import pathlib
import subprocess
import sys

CACHE = pathlib.Path(os.environ.get("RELEASE_MIRROR_CACHE", "/var/tmp/donsetch-release-mirror"))


def gh_asset_url(repo: str, tag: str, name: str) -> str:
    out = subprocess.run(
        ["gh", "api", f"repos/{repo}/releases/tags/{tag}", "--jq",
         f'.assets[]|select(.name=="{name}")|.url'],
        capture_output=True, text=True, check=False,
    )
    url = out.stdout.strip()
    if out.returncode != 0 or not url:
        sys.exit(f"release-mirror: no asset {name} in {repo} {tag}")
    return url


def gh_release_assets(repo: str, tag: str) -> dict[str, str]:
    out = subprocess.run(
        ["gh", "api", "--paginate", f"repos/{repo}/releases/tags/{tag}",
         "--jq", r'.assets[]|"\(.name)\t\(.url)"'],
        capture_output=True, text=True, check=False,
    )
    if out.returncode != 0:
        sys.exit(f"release-mirror: cannot list {repo} {tag}: {out.stderr.strip()}")
    found = {}
    for line in out.stdout.splitlines():
        name, _, url = line.partition("\t")
        # --paginate appends an empty page after the last one; its jq
        # output is a blank tab pair, which must not become an asset.
        if name and url.startswith("https://"):
            found[name] = url
    return found


def fetch(url: str, dest: pathlib.Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.exists() and dest.stat().st_size > 0:
        return
    tmp = dest.with_suffix(dest.suffix + ".part")
    print(f"release-mirror: fetching {dest.name}", file=sys.stderr)
    subprocess.run(
        ["curl", "-fSL", "--retry", "5", "-H", "Accept: application/octet-stream",
         "-o", str(tmp), url], check=True,
    )
    tmp.rename(dest)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18777)
    ap.add_argument("--v8", help="rusty_v8 version to mirror, e.g. 150.4.0")
    ap.add_argument("--asset", action="append", default=[],
                    help="owner/repo/tag/name of a pinned prebuilt")
    args = ap.parse_args()

    routes: dict[str, tuple[str, pathlib.Path]] = {}
    if args.v8:
        # Every asset of the release, not just this host's: a build machine
        # that asks for another platform must get it, not a 404.
        ver = args.v8
        repo = "denoland/rusty_v8"
        for name, url in gh_release_assets(repo, f"v{ver}").items():
            routes[f"/v8/v{ver}/{name}"] = (url, CACHE / "v8" / f"v{ver}" / name)
        print(f"export RUSTY_V8_MIRROR=http://127.0.0.1:{args.port}/v8", file=sys.stderr)
    for spec in args.asset:
        owner, repo, tag, name = spec.split("/", 3)
        dest = CACHE / "asset" / owner / repo / tag / name
        routes[f"/asset/{owner}/{repo}/{tag}/{name}"] = (gh_asset_url(f"{owner}/{repo}", tag, name), dest)
        print(f"asset: http://127.0.0.1:{args.port}/asset/{owner}/{repo}/{tag}/{name}", file=sys.stderr)
    if not routes:
        sys.exit("release-mirror: give --v8 and/or --asset")

    class Handler(http.server.SimpleHTTPRequestHandler):
        # Serve by hand: SimpleHTTPRequestHandler's translate_path re-roots
        # the path under the directory argument and would never find these.
        def do_GET(self):
            entry = routes.get(self.path.split("?")[0])
            if entry is None:
                self.send_error(404)
                return
            url, target = entry
            try:
                fetch(url, target)  # first request for this asset downloads it
            except Exception as e:  # noqa: BLE001 - report, never die
                self.send_error(502, f"mirror fetch failed: {e}")
                return
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Length", str(target.stat().st_size))
            self.end_headers()
            with open(target, "rb") as f:
                while chunk := f.read(1 << 20):
                    self.wfile.write(chunk)

        def log_message(self, format, *args):  # noqa: A002 - base signature
            print(f"release-mirror: {self.requestline}", file=sys.stderr)

    http.server.ThreadingHTTPServer.allow_reuse_address = True
    with http.server.ThreadingHTTPServer(
        ("127.0.0.1", args.port), functools.partial(Handler, directory=str(CACHE))
    ) as srv:
        print(f"release-mirror: serving on http://127.0.0.1:{args.port}", file=sys.stderr)
        srv.serve_forever()


if __name__ == "__main__":
    main()
