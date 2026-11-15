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

<!-- Documentation-only changes need no build or test run. -->
- [ ] Relevant scoped tests pass via `just t <scope>` (or `just tf <scope>` for feature-gated code; nextest, not bare `cargo test`)
- [ ] `just all` passes for shared code changes (format, compile, lockfile, clippy and static budget checks)
- [ ] The required CI `gate` is green; the workflow selects the applicable test and build lanes

## Breaking changes

<!-- If this PR breaks existing behavior, describe what changes and why. Use `feat!` or `fix!` in the commit message. -->

## Test plan

<!-- How did you verify this works? What edge cases did you test? -->
