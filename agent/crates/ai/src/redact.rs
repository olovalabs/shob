//! Helpers that keep credentials out of `Debug` output. Used by the option
//! structs whose `Debug` impls mask `api_key` and sensitive headers.

use std::collections::BTreeMap;
use std::fmt;

/// Debug view of a header map that masks values of sensitive headers.
pub(crate) struct RedactedHeaders<'a>(pub(crate) &'a BTreeMap<String, String>);

impl fmt::Debug for RedactedHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(name, value)| {
                let value = if is_sensitive_header(name) {
                    "<redacted>"
                } else {
                    value.as_str()
                };
                (name, value)
            }))
            .finish()
    }
}

/// True for header names that usually carry credentials (case-insensitive).
/// Substring matches also cover `proxy-authorization`, `x-api-key`,
/// `cf-aig-authorization`, `set-cookie`, and `x-session-token`.
pub(crate) fn is_sensitive_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["authorization", "api-key", "cookie", "token", "secret"]
        .iter()
        .any(|needle| name.contains(needle))
}
