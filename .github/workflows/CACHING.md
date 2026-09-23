# How CI caches Cargo builds

This describes the caching in `ci.yml`'s `build-test` job, why it is shaped the way it is, and what to keep in mind when changing it. The workflow comments point back here; the two helper scripts are `scripts/dep-hash.py` and `scripts/ci-sweep.py`.

## What it is for

Almost all of a CI run is compilation. The test suite itself takes about 20 seconds; building the dependency tree from nothing takes 7 to 30 minutes depending on the runner. The cache exists so that a run only compiles what actually changed since the last run. Two situations matter most:

- **A dependency bump.** Only the bumped crate and the crates that depend on it should recompile, not the whole tree.
- **A release.** Bumping donsetch's own version must not invalidate anything.

At the same time, the cache must not grow without bound. GitHub gives each repository 10 GB of cache in total and evicts the least recently used entries once that is exceeded, so a bloated entry for one lane can push out another lane's.

## Two caches per lane

Each lane keeps two independent entries.

**`~/.cargo`** holds the crates.io index, the downloaded `.crate` archives and git dependencies. It is handled by `Swatinem/rust-cache` with `cache-targets: false`, using that action's own keying, which already ignores donsetch's version and falls back to the previous entry when the lockfile changes. It holds no compiled code. Measured against fetching the index, the git dependency and every crate from nothing, restoring it costs 19 s instead of 37 s on Windows and 10 s instead of 38 s on macOS x86_64; on Linux it is 2 s instead of 6-7 s, and on macOS arm64 it is about even (13 s against 11 s). Extracting many small files is slow on the Windows and Intel macOS runners, but downloading and writing them fresh is slower still. It also spares crates.io and GitHub a full re-download on every run. On a lockfile change, rust-cache additionally spends about 10 seconds cleaning up before it saves a new entry. It is not shared between lanes, because rust-cache always keys on the runner's OS and CPU architecture, and each platform downloads a different subset of crates anyway.

**`target/`** holds every compiled artifact. It is handled by `actions/cache/restore` and `actions/cache/save` directly, as separate steps, so the workflow controls exactly when and under which key it is saved.

rust-cache does not manage `target/` because of how it cleans up after a fallback restore: it deletes every artifact that was *compiled* more than seven days ago, before the build runs. Cargo never rewrites an artifact that is still fresh, and an exact cache hit is never saved again, so after a dependency bump that deletes most of a tree that is still in use. The rust-cache step must also run *before* the `target/` restore, because that cleanup walks `target/` even when told not to cache it. The action is pinned to a commit rather than the moving `v2` tag, because a re-tag has changed that cleanup's behavior before (rust-cache issue #375 and PR #377).

## The `target/` key

```
tgt-<target triple>-<kind>-<rustc commit>-v1-<dependency hash>-<workflow hash>
```

- **target triple** separates the lanes.
- **kind** is the shape of the build: `full` or `smoke` builds test binaries, `none` only runs `cargo check`. A check-only tree has no machine code, so a test build that restored one would recompile everything. The Windows lane is keyed `full` on every event, pull requests included (see below).
- **rustc commit** comes from `rustc -vV`. A new compiler invalidates every artifact, so there is nothing worth falling back to across compilers.
- **v1** is a manual reset. Changing it forces every lane to start from nothing once.
- **dependency hash** comes from `scripts/dep-hash.py`. It hashes `Cargo.lock` and every tracked `Cargo.toml`, parsed as TOML, with donsetch's own version and path-dependency versions zeroed and the workspace's own lockfile entries dropped. A release commit therefore keeps the same hash, while a real dependency, feature or profile change produces a new one. The files are read as UTF-8 explicitly: on the Windows runners Python otherwise decodes them with the cp1252 code page, and because the manifests contain non-ASCII text, Windows used to compute a different hash from every other lane.
- **workflow hash** is a hash of `ci.yml` itself, with line endings normalized. What a lane builds is decided there: its features (the Windows lane alone builds two feature sets, one of them spelled out inside a step), cargo flags, environment and steps. Without this part, a change to how a lane builds would keep an exact hit on a tree built the old way; the lane would then recompile the difference on every run and never save a corrected entry. Any edit to the file, even a comment, produces a new key, but that only costs one fallback run per lane, as described next.

The restore step tries, in order: the exact key; any key that starts with the exact key (this is how a `-partial-` entry from a failed run is found, see below); any key with the same dependency hash, which is the same dependencies under an older workflow; and finally any key with the same prefix up to `v1-`. When several entries match a prefix, GitHub restores the most recently created one, so after a dependency bump or a workflow change the previous state is restored. Cargo's own per-unit hashes then decide what is reusable. A fallback can therefore cost restore time, but it can never produce a wrong build.

## Keeping the cache bounded: the sweep

After a fallback, `target/` still contains the artifacts of the superseded dependency versions. Nothing reads them again, but without cleanup each bump would add them to the next saved entry. `scripts/ci-sweep.py` removes them before the save.

It works from file access times. Cargo reads every unit's fingerprint files under `.fingerprint/` on every build, including units that are still fresh and are not recompiled. So a unit whose fingerprint was read during this job is part of the build, and one that was not is stale. The script runs in two parts:

1. **`prepare`**, right after the restore: sets the access time of every restored file to 2000-01-01, leaving the modification time untouched to the nanosecond, because Cargo judges freshness by modification time. Then it writes a timestamp.
2. **`sweep`**, after the last cargo command: keeps every unit that has a fingerprint file read after the timestamp, and deletes every other unit's files from `.fingerprint/`, `build/`, `deps/` and the profile directory, matched by the 16-hex-digit hash in their names. Files without such a hash are left alone.

Then `cargo clean -p donsetch` removes donsetch's own artifacts, which every commit rebuilds anyway, and the tree is saved.

Access times behave differently on each runner OS, and each difference was found by measurement:

- **Linux** records the reads without help.
- **macOS** does not update the access time of a freshly restored file when it is read, because extraction leaves it newer than the modification time. Aging it to 2000 in `prepare` fixes that.
- **Windows** runners ship with access-time updates switched off (`DisableLastAccess = 3`). A workflow step switches them on with `fsutil behavior set disablelastaccess 0` before the restore. The change takes effect immediately, and the step fails the job if the setting does not read as enabled afterwards, because a sweep that sees no reads would delete the whole tree.

**Nothing may read `target/` between the restore and `prepare`.** Anything that does makes restored units look used. Even `du` counts: in Git Bash on Windows it updates the access time of every file it sizes. The sweep must also stay after the last cargo step, since it deletes everything the steps before it did not touch.

We tried `cargo-sweep` first. On the Windows runner it deleted 2.5 GiB of units that were still in use after a dependency bump, while the same rule applied by `ci-sweep.py` kept exactly the right ones on all five lanes. The cause was not found. `cargo-sweep` is also marked unmaintained and publishes no release binaries.

## When a run fails

A failed run, including one that hit a step's timeout, still saves its `target/`, but differently from a green one:

- **No sweep.** The steps after the failure never ran, so their artifacts were never read, and a sweep would delete exactly what the next run needs.
- **A separate key**, `<exact key>-partial-<run id>-<attempt>`. Saving under the exact key would be a trap: an exact hit is never saved again, so an incomplete tree there would stay until the dependencies change.

Before that save, a step stops any build processes still running. When a step hits its timeout, GitHub stops the step itself, but the compilers cargo started keep writing into `target/`, and `tar` then refuses to archive a tree that changes while it reads it. The save only logs a warning in that case, so without this step a timed-out run would silently save nothing. The step sends SIGTERM first and SIGKILL only to what is still running ten seconds later: `make` and the C compilers in build scripts (BoringSSL's CMake build, for one) delete their half-written output file on SIGTERM but not on SIGKILL, and a truncated object file with a fresh timestamp would look up to date to the next `make`. Cargo's own units are safe either way, since Cargo drops a unit's fingerprint before rebuilding it and writes a new one only after a successful compile.

The next run finds the partial entry through the first restore key, which matches the exact key as a prefix, and continues from it instead of starting from nothing, and the first green run saves the complete, swept entry under the exact key. This breaks the pattern where a slow cold build times out, saves nothing, and the next run times out the same way. Cargo records a unit only after it compiles, so a partial tree is incomplete but never wrong. Cancelled runs save nothing.

## Pull requests never save

Neither cache is saved from a `pull_request` run. GitHub scopes a pull request's cache to its merge ref, so only later runs of that same pull request could restore it; `master` and other pull requests never can. A pull request that does not touch dependencies gets an exact hit from `master`'s entry and would not save anyway, so the rule only matters for dependency updates. Saving those would add roughly 0.5 to 1 GB per lane to the shared 10 GB pool. Because a restore marks `master`'s entries as used at the *start* of a job while a pull request's save lands at its *end*, a burst of dependency pull requests would get `master`'s entries evicted first.

Pull requests still restore. Because they never save, the Windows lane can restore `master`'s test-built `full` entry for its `cargo check` run, which contains everything a check needs. That took the first Windows run of a pull request from 18 minutes to 4. It is safe only because nothing is written back: if pull requests ever start saving, the Windows lane needs its own key for them again.

## Tags never save

Release runs on a tag restore but never save (`release.yml`). A run for one tag cannot restore another tag's cache, and `master` never saves a release key, so an entry saved by a tag would never be read again. Those entries used to take up almost 4 GB of the pool.

## Forcing a refresh

An exact hit skips the sweep and the save, so a stale entry stays until its key changes. To replace it:

- **Change `ci.yml`.** Any edit changes the workflow hash, so every lane falls back to its previous entry, recompiles what changed, sweeps and saves a fresh entry. This is the cheap refresh, and it happens anyway whenever the workflow changes.
- **Bump `v1` in the key.** Every lane starts from nothing once.
- **Delete the entry** (`gh cache delete <key>`). The next run falls back to the newest older entry with the same prefix, or starts from nothing if there is none, then sweeps and saves.

## Known gaps

- On Windows, the release-feature gate recompiles `ort-sys` and the crates above it on every run, even on an exact hit. The cause is not known yet.
