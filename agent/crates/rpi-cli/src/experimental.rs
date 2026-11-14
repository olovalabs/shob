//! Experimental feature gate.
//!
//! Port of the upstream `packages/coding-agent/src/core/experimental.ts`.
//! Experimental surfaces (e.g. the extended footer) are only enabled when
//! `RPI_EXPERIMENTAL=1` is set.

/// True when `RPI_EXPERIMENTAL=1`. Evaluated once per process.
pub fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RPI_EXPERIMENTAL")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_matches_env() {
        let expected = std::env::var("RPI_EXPERIMENTAL")
            .map(|v| v == "1")
            .unwrap_or(false);
        assert_eq!(enabled(), expected);
    }
}
