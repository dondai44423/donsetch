## Summary

<!-- What does this PR do? One paragraph. -->

## Type

- [ ] `feat` — new feature
- [ ] `fix` — bug fix
- [ ] `docs` — documentation only
- [ ] `chore` — deps, CI, tooling
- [ ] `refactor` — no behavior change
- [ ] `test` — tests only

## Checks

- [ ] `cargo nextest run --all-features` passes (not `cargo test`: several tests rely on nextest's one process per test)
- [ ] `cargo clippy --all-targets --all-features -- -Dwarnings` passes (zero warnings)
- [ ] `cargo fmt --all -- --check` passes
- [ ] CI is green on all 5 lanes (Linux x86_64 and aarch64, macOS arm64 and x86_64, Windows)

## Breaking changes

<!-- If this PR breaks existing behavior, describe what changes and why. Use `feat!` or `fix!` in the commit message. -->

## Test plan

<!-- How did you verify this works? What edge cases did you test? -->
