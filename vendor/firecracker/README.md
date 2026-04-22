# vendor/firecracker — upstream reference tree

This directory contains verbatim copies of crates from upstream
[Firecracker](https://github.com/firecracker-microvm/firecracker). They
are **not built** by `cargo build` on macOS; they live here as the
diff reference for the copy-and-sync pattern used by
`src/hephaestus-fc-api`'s wire types.

## Why keep them in-tree?

`hephaestus-fc-api` copies the pure-serde wire types from upstream's
`src/vmm/src/vmm_config/*` so the macOS port can speak the same HTTP
protocol without dragging in Firecracker's Linux-only dependencies
(KVM, vhost, memfd, userfaultfd, and `micro_http`'s epoll/eventfd
server loop). Each file in `src/hephaestus-fc-api/src/vmm_config/`
carries a `// upstream:` header pointing at the exact file here so
mechanical diff-and-port is the rebase workflow.

## Layout

| crate | upstream path | status on macOS |
| --- | --- | --- |
| `vmm/` | `src/vmm/` | Linux-only (KVM, vhost, userfaultfd) — reference only |
| `firecracker/` | `src/firecracker/` | Linux-only HTTP server (`micro_http` + epoll) |
| `jailer/` | `src/jailer/` | Linux-only process isolation |
| `utils/` | `src/utils/` | Linux-only helpers (eventfd, signalfd) |
| `seccompiler/` | `src/seccompiler/` | Linux seccomp-BPF compiler |
| `cpu-template-helper/` | `src/cpu-template-helper/` | KVM CPU feature control |
| `snapshot-editor/` | `src/snapshot-editor/` | KVM snapshot blob editor |
| `rebase-snap/` | `src/rebase-snap/` | diff-snapshot rebasing |
| `acpi-tables/` | `src/acpi-tables/` | OS-agnostic upstream utility |
| `clippy-tracing/` | `src/clippy-tracing/` | OS-agnostic upstream lint |
| `log-instrument/`, `log-instrument-macros/` | `src/log-instrument*/` | OS-agnostic upstream logging |

None of these are in the workspace `members` list; they're in
`exclude` so `cargo` ignores them on macOS.

## Re-syncing with upstream

```bash
git remote add upstream https://github.com/firecracker-microvm/firecracker
git fetch upstream
git diff upstream/main -- vendor/firecracker/vmm/src/vmm_config/
```

When upstream makes a wire-shape change to a config struct, port it
into `src/hephaestus-fc-api/src/vmm_config/<same-file>.rs`, then run
`just fc-compat` to verify the Go SDK still deserializes cleanly
against our server.
