# Vendored agent-detection rule packs

The `*.toml` files in this directory are copied **unmodified** from the herdr
project and are licensed under the Apache License, Version 2.0.

- Upstream: <https://github.com/herdrdev/herdr>
- Directory: `src/detect/manifests/`
- Revision: `bc8c28988f75c538d4ca058b702f5adbea30566e` (2026-10-29)
- License: Apache-2.0 — <https://github.com/herdrdev/herdr/blob/master/LICENSE>
- Copyright: the herdr authors

They are evaluated by `src/agent_rules.rs`, which is a port of herdr's
`src/detect/manifest.rs` (same manifest schema, region slicing, and gate
semantics) so that the packs keep working exactly as upstream intends.

## Updating

Use `kathamo integration update-rules` to download upstream packs into the per-user update cache. Kathamo validates each manifest and its `min_engine_version`, chooses only newer compatible remote packs, and keeps local overrides in `~/.config/kathamo/agent-detection/` as the highest-precedence source. New custom agent packs in that local directory can add aliases and screen rules without Rust changes.
`index.toml` is upstream's registry; this crate instead auto-discovers every
`agent-detection/*.toml` embedded under `assets/`.

If the packs are ever distributed as part of a binary release, include the full
Apache-2.0 license text alongside them, as the license requires.
