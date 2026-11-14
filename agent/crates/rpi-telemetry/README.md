# rpi-telemetry

[![crates.io](https://img.shields.io/crates/v/rpi-telemetry.svg)](https://crates.io/crates/rpi-telemetry)
[![docs.rs](https://docs.rs/rpi-telemetry/badge.svg)](https://docs.rs/rpi-telemetry)

Minimal span and event contracts, with a no-op default. This is the smallest
crate in the family and it exists so that instrumentation is a *choice*: the
agent loop, providers and tools emit spans through a trait object that does
nothing unless you supply a real context.

## Install

```toml
[dependencies]
rpi-telemetry = "0.3"
```

## Why a whole crate

The runtime crates call into telemetry at request boundaries. Putting those
calls behind a trait in a dependency-free leaf means:

- No tracing backend, exporter or OpenTelemetry dependency reaches a user who
  does not want one.
- The default is monomorphic and free: `NoopTelemetryContext` does nothing, and
  a consumer holds a `&dyn TelemetryContext` that defaults to
  `NOOP_TELEMETRY_CONTEXT`.
- Tests can assert on emitted spans without a global subscriber.

## Usage

```rust
use rpi_telemetry::{SpanOptions, NOOP_TELEMETRY_CONTEXT};

// The default: nothing is recorded, and no exporter is allocated.
let _span = NOOP_TELEMETRY_CONTEXT.start_span(SpanOptions::new("provider.request"));
// Dropping the guard ends the span.
```

Implement `TelemetryContext` to bridge into your own tracing stack.
`ProductionTelemetryContext` and the `testing` feature provide a starting point:
the former is the runtime-oriented context, the latter an in-memory recorder for
assertions in tests.

## Documentation

- Crate docs: <https://docs.rs/rpi-telemetry>

## License

MIT. See [LICENSE](https://github.com/bigfish1913/pi-rust/blob/main/LICENSE)
and [NOTICE](https://github.com/bigfish1913/pi-rust/blob/main/NOTICE).
