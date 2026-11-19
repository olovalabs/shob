//! Per-role model routing.
//!
//! Borrowed from oh-my-pi's roles (`packages/coding-agent/src/config/model-resolver.ts`:
//! `@smol` / `@slow` / `@default`, with `memory`/`tiny`/`widget` aliasing `smol`).
//! The useful part is not the number of roles; it is that a caller asks for a
//! *role* and the routing layer decides the model, so a cheap task cannot
//! accidentally inherit the expensive conversation model.
//!
//! Shob's concrete case is title generation: `embedded::SessionTitleRequest`
//! currently builds its `SessionOptions` from the same `provider/model` selector
//! as the conversation. Naming a conversation does not need a frontier model.
//!
//! Falls back to the primary selector whenever a role is unconfigured, so
//! routing can never fail a task that used to work.

use serde::{Deserialize, Serialize};

/// What a model is being asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The conversation itself.
    Primary,
    /// Short, mechanical tasks: titles, summaries of already-decided work.
    Smol,
    /// Verification and classification.
    Judge,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Primary => "primary",
            Role::Smol => "smol",
            Role::Judge => "judge",
        }
    }
}

/// A `provider/model` selector. Kept as the single string Shob already passes to
/// `SessionOptions::with_selector`, so routing cannot introduce a second
/// spelling of a model identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selector(pub String);

impl Selector {
    pub fn new(selector: impl Into<String>) -> Self {
        Self(selector.into())
    }

    /// The provider half, if the selector is `provider/model`.
    pub fn provider(&self) -> Option<&str> {
        self.0.split_once('/').map(|(provider, _)| provider)
    }

    pub fn model(&self) -> &str {
        self.0.split_once('/').map_or(self.0.as_str(), |(_, model)| model)
    }
}

/// Role → selector table. `primary` is always known; the others are optional and
/// inherit it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Router {
    primary: Option<Selector>,
    smol: Option<Selector>,
    judge: Option<Selector>,
    route_cheap_tasks: bool,
}

impl Router {
    pub fn new(primary: Option<Selector>, smol: Option<Selector>, route_cheap_tasks: bool) -> Self {
        Self {
            primary,
            smol,
            judge: None,
            route_cheap_tasks,
        }
    }

    /// Build from the layer's settings plus the conversation's selector.
    pub fn from_settings(
        primary: Option<&str>,
        settings: &super::config::TokenEfficiencySettings,
    ) -> Self {
        Self {
            primary: primary.map(Selector::new),
            smol: settings.smol_model.as_deref().map(Selector::new),
            judge: None,
            route_cheap_tasks: settings.route_cheap_tasks,
        }
    }

    pub fn with_judge(mut self, judge: Option<Selector>) -> Self {
        self.judge = judge;
        self
    }

    /// The selector to use for `role`. `None` only when the role is unconfigured
    /// *and* there is no primary to fall back to.
    pub fn route(&self, role: Role) -> Option<&Selector> {
        if !self.route_cheap_tasks && role != Role::Primary {
            return self.primary.as_ref();
        }
        let candidate = match role {
            Role::Primary => self.primary.as_ref(),
            Role::Smol => self.smol.as_ref().or(self.primary.as_ref()),
            Role::Judge => self
                .judge
                .as_ref()
                .or(self.smol.as_ref())
                .or(self.primary.as_ref()),
        };
        candidate
    }

    /// Whether `role` resolves to something cheaper than the primary.
    pub fn is_routed_away_from_primary(&self, role: Role) -> bool {
        match (self.route(role), &self.primary) {
            (Some(selector), Some(primary)) => selector != primary,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_efficiency::config::TokenEfficiencySettings;

    fn router() -> Router {
        Router::new(
            Some(Selector::new("anthropic/claude-opus-4-7")),
            Some(Selector::new("anthropic/claude-haiku-4-5")),
            true,
        )
    }

    #[test]
    fn smol_routes_to_the_cheap_model() {
        let router = router();
        assert_eq!(router.route(Role::Smol).unwrap().0, "anthropic/claude-haiku-4-5");
        assert!(router.is_routed_away_from_primary(Role::Smol));
    }

    #[test]
    fn primary_is_untouched() {
        let router = router();
        assert_eq!(router.route(Role::Primary).unwrap().0, "anthropic/claude-opus-4-7");
        assert!(!router.is_routed_away_from_primary(Role::Primary));
    }

    #[test]
    fn an_unconfigured_smol_falls_back_to_primary() {
        let router = Router::new(Some(Selector::new("openai/gpt-5")), None, true);
        assert_eq!(router.route(Role::Smol).unwrap().0, "openai/gpt-5");
        assert!(!router.is_routed_away_from_primary(Role::Smol));
    }

    #[test]
    fn judge_falls_back_through_smol_then_primary() {
        let judge_router = router().with_judge(Some(Selector::new("openai/gpt-5-mini")));
        assert_eq!(judge_router.route(Role::Judge).unwrap().0, "openai/gpt-5-mini");

        let no_judge = router();
        assert_eq!(no_judge.route(Role::Judge).unwrap().0, "anthropic/claude-haiku-4-5");

        let only_primary = Router::new(Some(Selector::new("x/y")), None, true);
        assert_eq!(only_primary.route(Role::Judge).unwrap().0, "x/y");
    }

    #[test]
    fn disabling_routing_sends_every_role_to_primary() {
        let router = Router::new(
            Some(Selector::new("primary/model")),
            Some(Selector::new("cheap/model")),
            false,
        );
        for role in [Role::Primary, Role::Smol, Role::Judge] {
            assert_eq!(router.route(role).unwrap().0, "primary/model");
        }
    }

    #[test]
    fn no_primary_and_no_role_is_none_not_a_panic() {
        let router = Router::default();
        assert!(router.route(Role::Smol).is_none());
        assert!(!router.is_routed_away_from_primary(Role::Smol));
    }

    #[test]
    fn selector_splits_provider_and_model_once() {
        let selector = Selector::new("openrouter/anthropic/claude-sonnet");
        assert_eq!(selector.provider(), Some("openrouter"));
        assert_eq!(selector.model(), "anthropic/claude-sonnet");

        let bare = Selector::new("claude-sonnet");
        assert_eq!(bare.provider(), None);
        assert_eq!(bare.model(), "claude-sonnet");
    }

    #[test]
    fn from_settings_reads_the_configured_smol_model() {
        let mut settings = TokenEfficiencySettings::default();
        settings.smol_model = Some("anthropic/claude-haiku-4-5".into());
        let router = Router::from_settings(Some("anthropic/claude-opus-4-7"), &settings);
        assert_eq!(router.route(Role::Smol).unwrap().0, "anthropic/claude-haiku-4-5");

        let unconfigured = Router::from_settings(
            Some("anthropic/claude-opus-4-7"),
            &TokenEfficiencySettings::default(),
        );
        assert_eq!(unconfigured.route(Role::Smol).unwrap().0, "anthropic/claude-opus-4-7");
    }

    #[test]
    fn role_labels_are_stable() {
        assert_eq!(Role::Primary.as_str(), "primary");
        assert_eq!(Role::Smol.as_str(), "smol");
        assert_eq!(Role::Judge.as_str(), "judge");
    }
}
