//! Mirrors `packages/ai/src/api/anthropic-messages.ts::getCacheControl` and
//! `resolveCacheRetention` — the cache-control blob builder the request mapper
//! attaches to system / tool / message blocks.
//!
//! TS `resolveCacheRetention` defaults to `"short"` and reads `PI_CACHE_RETENTION`
//! for backward compat. The Rust port dims env access to the caller's
//! `SimpleStreamOptions.cache_retention` (which the agent already resolved) and
//! documents the env-var hook as a TODO; the default remains `Short` so behaviour
//! matches when the caller leaves it unset.

use crate::model::{AnthropicMessagesCompat, Model, StreamingProtocolCompat};
use crate::provider::CacheRetention;
use serde::{Deserialize, Serialize};

/// The Anthropic `cache_control` block, attached to breakable content to opt it
/// into prompt caching. Mirrors TS `CacheControlEphemeral`.
///
/// - `{ type: "ephemeral" }` — 5m retention (the Anthropic default).
/// - `{ type: "ephemeral", ttl: "1h" }` — 1h retention, only when the model
///   reports `supportsLongCacheRetention`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CacheControlEphemeral {
    /// Always `"ephemeral"`.
    #[serde(rename = "type")]
    pub kind: EphemeralType,
    /// `"1h"` when present; absent means the provider default (5m).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<EphemeralTtl>,
}

/// Marker that serializes as the literal `"ephemeral"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EphemeralType;
impl Serialize for EphemeralType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("ephemeral")
    }
}
impl<'de> Deserialize<'de> for EphemeralType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "ephemeral" {
            return Err(serde::de::Error::custom(format!(
                "expected \"ephemeral\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

/// Marker that serializes as the literal `"1h"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EphemeralTtl;
impl Serialize for EphemeralTtl {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("1h")
    }
}
impl<'de> Deserialize<'de> for EphemeralTtl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "1h" {
            return Err(serde::de::Error::custom(format!(
                "expected \"1h\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

/// The resolved cache-control decision for a turn. Mirrors the TS return shape
/// `{ retention, cacheControl? }` of `getCacheControl`.
///
/// `cache_control` is `None` only when `retention == None` (cache control
/// disabled); otherwise a `CacheControlEphemeral` is always emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheControlOption {
    pub retention: CacheRetention,
    pub cache_control: Option<CacheControlEphemeral>,
}

impl CacheControlOption {
    /// True when a `cache_control` block should be attached to content.
    pub fn has_block(&self) -> bool {
        self.cache_control.is_some()
    }
}

/// Resolve the retention preference to use for this call. Mirrors the TS
/// `resolveCacheRetention`: the caller-supplied `cache_retention` wins; when
/// unset it collapses to the `Short` default. The `PI_CACHE_RETENTION` env-var
/// fallback the TS uses is intentionally NOT modeled here (no ambient env in the
/// provider layer); callers that want it should resolve and forward
/// `CacheRetention::Long` via `SimpleStreamOptions`. The default stays `Short`.
pub fn resolve_cache_retention(cache_retention: CacheRetention) -> CacheRetention {
    cache_retention
}

/// Build the Anthropic `cache_control` blob for `model` + `retention`. Mirrors
/// TS `getCacheControl`. When `retention == None` no block is produced; when
/// `retention == Long` AND the model supports long cache retention, the block
/// carries `ttl: "1h"`; otherwise the block is a plain `{ type: "ephemeral" }`.
///
/// `compat` is read off `model.compat` via the `AnthropicMessagesCompat` flag
/// `supports_long_cache_retention`. A model with no compat configured is treated
/// as supporting the long tier (matching the field's `true` default).
pub fn get_cache_control(model: &Model, cache_retention: CacheRetention) -> CacheControlOption {
    let retention = resolve_cache_retention(cache_retention);
    if matches!(retention, CacheRetention::None) {
        return CacheControlOption {
            retention,
            cache_control: None,
        };
    }

    let supports_long = model
        .compat
        .as_ref()
        .and_then(|c| match c {
            StreamingProtocolCompat::AnthropicMessages(a) => Some(a),
            _ => None,
        })
        .map(|c: &AnthropicMessagesCompat| c.long_cache_retention())
        .unwrap_or(true);

    let ttl = if matches!(retention, CacheRetention::Long) && supports_long {
        Some(EphemeralTtl)
    } else {
        None
    };

    CacheControlOption {
        retention,
        cache_control: Some(CacheControlEphemeral {
            kind: EphemeralType,
            ttl,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AnthropicMessagesCompat, StreamingProtocolCompat};
    use crate::provider::CacheRetention;
    use crate::types::{Api, InputModality, ModelCost};

    fn model_with_compat(compat: AnthropicMessagesCompat) -> Model {
        let mut m = Model::new(
            "claude-opus-4-7",
            "Claude Opus 4.7",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        m.input = vec![InputModality::Text, InputModality::Image];
        m.context_window = 200_000;
        m.max_tokens = 32_000;
        m.cost = ModelCost::default();
        m.compat = Some(StreamingProtocolCompat::AnthropicMessages(compat));
        m
    }

    #[test]
    fn none_retention_produces_no_block() {
        let model = model_with_compat(AnthropicMessagesCompat::default());
        let opt = get_cache_control(&model, CacheRetention::None);
        assert_eq!(opt.retention, CacheRetention::None);
        assert!(opt.cache_control.is_none());
        assert!(!opt.has_block());
    }

    #[test]
    fn short_retention_produces_plain_ephemeral() {
        let model = model_with_compat(AnthropicMessagesCompat::default());
        let opt = get_cache_control(&model, CacheRetention::Short);
        let cc = opt.cache_control.expect("short produces a block");
        assert_eq!(opt.retention, CacheRetention::Short);
        assert!(cc.ttl.is_none());
        // Serializes as {"type":"ephemeral"} (no ttl key).
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v["type"], serde_json::json!("ephemeral"));
        assert!(v.get("ttl").is_none());
    }

    #[test]
    fn long_retention_with_support_attaches_1h_ttl() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_long_cache_retention = Some(true);
        let model = model_with_compat(compat);
        let opt = get_cache_control(&model, CacheRetention::Long);
        let cc = opt.cache_control.expect("long produces a block");
        assert!(cc.ttl.is_some());
        let v = serde_json::to_value(&cc).unwrap();
        assert_eq!(v["type"], serde_json::json!("ephemeral"));
        assert_eq!(v["ttl"], serde_json::json!("1h"));
    }

    #[test]
    fn long_retention_without_support_falls_back_to_plain_ephemeral() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_long_cache_retention = Some(false);
        let model = model_with_compat(compat);
        let opt = get_cache_control(&model, CacheRetention::Long);
        // Long requested but unsupported — block still emitted, just no ttl.
        let cc = opt.cache_control.expect("block still emitted");
        assert!(cc.ttl.is_none());
        assert_eq!(opt.retention, CacheRetention::Long);
    }

    #[test]
    fn no_compat_configured_treats_long_as_supported() {
        let mut m = Model::new(
            "claude-opus-4-7",
            "Claude Opus 4.7",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        m.compat = None;
        let opt = get_cache_control(&m, CacheRetention::Long);
        assert!(opt.cache_control.unwrap().ttl.is_some());
    }
}
