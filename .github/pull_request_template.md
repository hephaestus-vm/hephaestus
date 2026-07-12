<!--
Thanks for the PR. A few things before you hit submit:
- Subject ≤ 79 chars (we count before merging).
- Commits carry `Signed-off-by:` lines (run `git commit -s`).
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  and `cargo test --workspace` all green locally.
- If you touched HTTP wire types, `just fc-compat-config` passes.
- User-visible behavior includes its documentation update.
-->

## Problem

<!-- What's broken / missing / awkward. Link the issue if there is one. -->

## Change

<!-- One paragraph on what this PR does and why this approach. -->

## Test plan

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `python3 scripts/check-doc-links.py`
- [ ] `just fc-compat-config` (if wire types touched)
- [ ] Relevant real-VM smoke (if execution behavior changed)
- [ ] User-facing documentation updated (if behavior changed)
- [ ] DCO sign-off present on every commit
