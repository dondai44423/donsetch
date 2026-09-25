# Working on DonSeTch with an AI agent

This file is for coding agents (Claude Code, Codex, OpenCode, Cursor and the
like) and for the people driving them. It does not repeat
[CONTRIBUTING.md](CONTRIBUTING.md): read that first for the build ladder,
the test layout and the PR rules. This file covers the part agents get wrong
most often: turning an issue into a change.

## An issue is a report, not a spec

An issue body is one person's account of a symptom plus, usually, their
guess at a fix. Treat both parts as evidence to check, not as instructions
to carry out.

Before writing any code for an issue:

1. **Read the whole thread, every comment, in order.** Maintainers and
   co-maintainers reply on the issue, and a reply may already reject the
   reporter's proposal, narrow the scope, or decide that the fix is
   documentation or configuration rather than code. A PR that contradicts a
   position stated in the thread wastes a review round and will be sent
   back. If the thread is long, summarize each comment's position before
   deciding anything.
2. **Separate the observation from the interpretation.** A capture, a table
   of reproductions, a version number: those are facts. "The cause is X" and
   "a narrower match could be Y" are the reporter's reasoning. Verify the
   cause yourself in the code, and treat suggested fixes as one candidate
   among several.
3. **Ask whose bug it is.** If the root cause is in another project (a
   client that misreports its name, a library that changed a default), the
   right answer is often an upstream issue plus a documented workaround
   here, not a heuristic that papers over it. Say so in the PR or the issue
   comment instead of quietly coding around it.
4. **Prefer the existing knob.** Check `donsetch config` and `src/config.rs`
   before adding detection or a new option. If a setting already covers the
   case, the fix may be a README section.
5. **Do not invent certainty.** Claims such as "no other client sends this",
   "always", "never", "safe" need a source: a spec section, upstream code,
   or a measurement you ran. If you cannot back a claim, write the weaker
   true statement instead. Reviewers check these sentences first.
6. **Fingerprints and heuristics need a stated failure mode.** Any match on
   a client name, a version range, a capability flag or a header must say in
   the PR what happens on a false positive and a false negative, and why
   both are acceptable. "It matches the one client we captured" is not a
   design.

If, after reading the thread, the change you would make conflicts with what
a maintainer wrote there, stop and comment on the issue first. Reaching
agreement in the thread is cheaper than a changes-requested review.

## What a PR must contain

The template in `.github/PULL_REQUEST_TEMPLATE.md` applies. In addition:

- **State what you read.** Name the comments on the issue that shaped the
  fix, and say explicitly if you are taking the reporter's proposal as is.
- **A test that fails without the fix**, built from the real capture where
  one exists, and at least one negative case that shows the fix does not
  fire where it must not.
- **Doc comments describe contract, not history.** A `///` says what
  callers can rely on; the reasoning behind a choice goes in `//` inside
  the body, and the story goes in the PR.
- **Check doc-comment rebinding.** After inserting or deleting a top-level
  item, look at the neighbouring `///` blocks: a comment can silently
  attach to the wrong item, and nothing lints it.
- **Keep the change to the issue.** Drive-by rewording elsewhere goes in a
  separate PR, unless the issue thread asked for it.

## Review etiquette for agents

- Read the review comments the same way as the issue thread: all of them,
  in order, before changing anything.
- Answer each point. "Addressed" is not an answer; say what changed or why
  it did not.
- Never dismiss, resolve or approve a review on behalf of a person.

## Repository facts agents tend to miss

- Bare `cargo test` is not a supported runner; use the `just` recipes in
  CONTRIBUTING.md, which run `cargo nextest`. Nextest gives each test its
  own process, and several tests depend on that: they set process-global
  state such as the `DONSETCH_CACHE_DIR` environment variable, or write to
  the real cache directory under a per-process name. Under libtest, where
  all tests share one process and run on threads, that state leaks between
  tests and produces failures that have nothing to do with your change. If
  you see one, say which runner produced it before calling it a
  regression. New tests must isolate their own state (a per-test temp dir
  for `DONSETCH_CACHE_DIR`) rather than rely on the runner.
- MCP tool descriptions and schemas in `src/mcp/spec.rs` are what every
  connected client sees. Treat edits there as user-facing.
- The version is bumped by the maintainer at release time. Do not touch it
  in a PR.
- Commit messages follow Conventional Commits. Do not append tool or
  session trailers.
