# Vendored upstream

This directory is a trimmed vendor of
[`bigfish1913/pi-rust`](https://github.com/bigfish1913/pi-rust), imported as a
Git subtree.

| Field | Value |
|---|---|
| Tag | `v0.21.0` |
| Resolved commit | `02d4a961cf9fd31285173f61bf06cf0e51435c82` |
| Git subtree prefix | `agent/` |
| Import commit | `161ec18` (`Squashed 'agent/' content from commit 02d4a96`) |

The import commit carries the standard `git-subtree-dir: agent` and
`git-subtree-split: 02d4a961…` trailers, so the subtree can be pulled or pushed
with the usual `git subtree` commands.

After import, the following non-build items were removed to keep the tree
limited to what Kathamo compiles:

- `website/`, `docs/`, `scripts/`, `packaging/`, `demo/`, `deploy/`
- `.github/`, `.codegraph/`, `.rpi/`, `.runtime-tests.tmp`
- the other standalone `examples/` (`examples/plugin-stub` is retained because
  the upstream resource-selection integration tests build it as a fixture)
- `Taskfile.yml`, `release-helper.rs`, `release-plz.toml`, `commit.id`,
  `.git-blame-ignore-revs`
- root upstream README/CHANGELOG/CONTRIBUTING/CODE_OF_CONDUCT/SECURITY/ROADMAP/
  RPI_SELF_REVIEW markdown

All library crates under `crates/`, their in-crate tests, `LICENSE`, `NOTICE`,
`Cargo.toml` and `Cargo.lock` remain. Kathamo's host-facing wrapper is
`agent/crates/rpi-embedded`; see `../EMBEDDED_AGENT.md` for integration and
verification details.
