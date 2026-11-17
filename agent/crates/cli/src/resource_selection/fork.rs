//! Resource authority is initialized for the NEW native identity, independently
//! of the historical fork cut. Never copy a source retry ledger as authority.
use super::*;
#[cfg(test)]
mod tests;

/// Startup preflight is advisory: authorize against the exact source snapshot
/// supplied by the metadata-first fork, before any destination is published.
pub(crate) fn initial_values_for_selection(
    source: &harness::session::SessionValues,
    selected: Option<&ResolvedSelection>,
) -> harness::error::SessionResult<Vec<(String, Value)>> {
    validate_session_binding(source, selected)
        .map_err(harness::error::SessionError::invalid_payload)?;
    initial_values(source)
}

pub(crate) fn initial_values(
    source: &harness::session::SessionValues,
) -> harness::error::SessionResult<Vec<(String, Value)>> {
    use harness::error::SessionError;
    let Some(binding) = source.get(SESSION_KEY) else {
        return Ok(Vec::new());
    };
    validate_persisted_binding(binding).map_err(SessionError::invalid_payload)?;
    let initial =
        json!({"revision":1,"declaration":binding["declaration"],"inventory":binding["inventory"]});
    if serde_json::to_vec(&initial)
        .map_err(|e| SessionError::invalid_payload(e.to_string()))?
        .len()
        > MAX_STATE_BYTES
    {
        return Err(SessionError::invalid_payload(
            "fork resource binding exceeds 256 KiB",
        ));
    }
    Ok(vec![(SESSION_KEY.into(), initial)])
}
