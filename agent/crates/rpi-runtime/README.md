# rpi-runtime

Host-independent model catalogs, provider-scoped credentials, model/thinking
selection and private execution bindings. Hosts supply configuration, storage,
environment values and already-loaded provider implementations. No CLI, terminal
or dynamic-library loader is required.

Use `RuntimeConfig::resolve` with a `ModelRequest`, `ProviderRegistry` and
`ValueSource`. The returned providers implement `rpi_ai::Provider` and can be
registered with an Agent or Harness. Public catalogs omit private request headers.

See `examples/embedded.rs` and `docs/architecture/reusable-runtime.md` in the
repository for the migration boundary and storage adapters.
