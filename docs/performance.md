# Performance

Long-form companion to the [README Performance
section](../README.md#performance). Explains how warm-restore wall
time is instrumented, what the baseline numbers mean, and how to
reproduce them.

## What we measured

For each of the three restore paths exposed by `hephaestus-firecracker`
— agent-flavor pool, stock-init-flavor pool, `PUT /snapshot/load` —
the restore is split into five sequential phases and each is logged
separately:

1. **`cp -c` clone** — pool-only; the APFS CoW copy of the pristine
   rootfs into the per-slot clone. (Not measured on the
   `/snapshot/load` path, which reuses the client-supplied rootfs.)
2. **Config build** — `ExecSession.makeSnapshotable` /
   `buildConfig` / `buildLongRunningConfig` in Swift. Includes path
   canonicalization, `VZGenericMachineIdentifier` load, attachment
   setup, and `configuration.validate()`.
3. **VM construct** — `VZVirtualMachine(configuration:queue:)`.
4. **`restoreMachineStateFrom:`** — the VZ primitive that actually
   reads the save blob and rebuilds guest state.
5. **`resume()`** — `VZVirtualMachine.resume()`.

Instrumentation lives in:

- Swift-side timers (`DispatchTime.now()` per phase) in
  `swift/HephaestusBridge/Sources/HephaestusBridge/DirectVZ.swift`.
- `HbRestoreTimings` C struct
  (`config_nanos`/`construct_nanos`/`restore_nanos`/`resume_nanos`)
  in `src/hephaestus-bridge/src/lib.rs`.
- `PoolRestoreBreakdown` (adds `clone_nanos`) in
  `src/hephaestus-pool/src/lib.rs`.

## Reference hardware and guest

- M-series Mac
- macOS 26
- Alpine Linux 3.20
- 2 vCPU / 512 MiB

## Medians (5 runs each)

| phase (ms)                | agent pool | stock pool | snapshot load |
| :------------------------ | ---------: | ---------: | ------------: |
| `cp -c` clone              |        4.0 |        4.5 |           n/a |
| config build              |       20.1 |       20.5 |          19.6 |
| VM construct              |        0.3 |        1.5 |           0.3 |
| **restoreMachineStateFrom** |  **228.9** |  **213.2** |     **214.5** |
| resume                    |        0.2 |        0.2 |           0.2 |
| **total**                 |  **253.0** |  **243.4** |     **234.7** |

## Interpretation

**Restore is at the VZ floor.** `restoreMachineStateFrom:` alone is
88–94 % of wall time across all three paths. This primitive is Apple-
provided; a win there would need a Framework-level change, not a
hephaestus one.

**Config build is the only plausible future target.** ~20 ms,
probably dominated by `configuration.validate()` + the
`VZGenericMachineIdentifier` read. A long-lived daemon that caches a
validated config across restores could shave it. No current use case
needs it, but the shape is documented here in case one appears.

**Everything else is noise.** `cp -c` is ~4 ms (APFS Copy-on-Write
metadata only), `VZVirtualMachine` construction is ~0.3 ms, `resume`
is ~0.2 ms. Not worth optimizing.

## What the measurement does *not* include

Worth calling out because early design notes mis-scoped it: the
restore timer starts at the beginning of
`hb_vz_{pool,stock_pool,long}_restore_long` and stops immediately
after `resume()`. The host-side `connectToAgent()` retry loop — where
the CLI's `vz-warm run` / `vz-exec` paths wait for the guest agent to
answer on vsock — fires *after* that, and only on the command-
injection path. The HTTP path never enters it, so vsock retries are
not part of the HTTP restore wall time. If you're chasing a slowdown
on the CLI warm path, that loop is a separate thing to measure.

## Reproducing

Each recipe runs the harness once and prints the per-phase log line
from the server's stderr. For medians, run each recipe 5+ times and
parse the log:

```bash
for _ in $(seq 1 5); do just fc-compat-pool;       done
for _ in $(seq 1 5); do just fc-compat-pool-stock; done
for _ in $(seq 1 5); do just fc-compat-snapshot;   done
```

Each log line looks like:

```
hephaestus-firecracker: pool hit slot=0 total=253.0ms
  (clone=4.0 config=20.1 construct=0.3 restore=228.9 resume=0.2)
```
