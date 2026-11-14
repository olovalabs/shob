# rpi-durable

Backend-independent checkpoint/recovery primitives for the native rpi host.
The initial module, `progress`, is the existing active assistant-progress
reducer extracted from `rpi-harness`, not a second journal or storage backend.

## Boundary

```text
rpi-harness (Memory / JSONL / SQLite adapters, append ACK, terminal proof)
    -> rpi-durable (validation, sequence stamping, checkpoint projection)
        -> serde / thiserror
```

`ProgressRecord` leaves payloads opaque. `ProgressState` validates batches,
projects stream/run retirement without cloning discarded payloads, and validates
restored checkpoints. After deserialization call `validate()` before accepting
mutations. Apply only a successfully prepared and acknowledged batch; publish
checkpoint candidates in the backend before replacing the live state.

The host, not this crate, supplies lane-scoped completed-run proof. This library
does not execute/replay tools, start workers, write files, decide terminal status,
or promise fsync, power-loss atomicity or exactly-once external side effects.
The host's history and progress sequence domains remain independent.

## Compatibility and scope

The serialized state remains `{ "sequence": N, "frames": [...] }`; the host
adapter preserves the existing frame format, companion discriminator and error
codes. No session is automatically migrated. Primary JSONL history still needs
a full initial scan; extracting a reducer alone does not make startup bounded.

This is a **native library**, distinct from the existing `rpi-package`
`rpi-durable` workflow plugin. This step neither loads both packages into one
workspace nor replaces the plugin. Before a future release/integration, the
native/library and plugin naming/version ownership must be reconciled explicitly.
The native crate participates in the workspace's version/dependency metadata so
that the published harness is not made dependent on a `publish = false` package.
No publishing is performed as part of this change.

See [session recovery design](../../docs/architecture/native-durable-session.md)
for the implementation boundary, diagnostics, tests and remaining work.
