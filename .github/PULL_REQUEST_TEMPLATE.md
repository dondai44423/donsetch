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

- [ ] `cargo test --release --features ocr,rerank` passes (622+ tests)
- [ ] `cargo clippy --release --all-targets --features ocr,rerank -- -Dwarnings` passes (zero warnings)
- [ ] `cargo fmt --all -- --check` passes
- [ ] CI is green on all 3 platforms (Linux, macOS, Windows)

## Breaking changes

<!-- If this PR breaks existing behavior, describe what changes and why. Use `feat!` or `fix!` in the commit message. -->

## Test plan

<!-- How did you verify this works? What edge cases did you test? -->
