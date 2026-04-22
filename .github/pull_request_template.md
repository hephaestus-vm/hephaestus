<!--
Thanks for the PR. A few things before you hit submit:
- Subject ≤ 79 chars (we count before merging).
- Commits carry `Signed-off-by:` lines (run `git commit -s`).
- `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
  and `cargo test --workspace` all green locally.
- If you touched HTTP wire types, `just fc-compat` passes.
-->

## Problem

<!-- What's broken / missing / awkward. Link the issue if there is one. -->

## Change

<!-- One paragraph on what this PR does and why this approach. -->

## Test plan

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `just fc-compat` (if wire types touched)
- [ ] DCO sign-off present on every commit
