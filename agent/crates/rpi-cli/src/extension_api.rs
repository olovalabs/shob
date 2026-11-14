//! Stable capability contracts shared by native and Node extension backends.
//!
//! The CLI consumes extension metadata through these small contracts instead
//! of depending on the implementation (cdylib or Node/RPC). New backends can
//! opt into capabilities without changing the TUI or agent orchestration.

use std::collections::BTreeSet;

pub const EXTENSION_API_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtensionCapability {
    Tools,
    Commands,
    Resources,
    UiNotify,
    UiEditor,
    UiCustom,
    UiSelect,
    UiConfirm,
    UiInput,
    UiEditorDialog,
    Session,
    Models,
    Events,
    Providers,
    ProviderCalls,
    Renderers,
    RuntimeActions,
}

impl ExtensionCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Commands => "commands",
            Self::Resources => "resources",
            Self::UiNotify => "ui.notify",
            Self::UiEditor => "ui.editor",
            Self::UiCustom => "ui.custom",
            Self::UiSelect => "ui.select",
            Self::UiConfirm => "ui.confirm",
            Self::UiInput => "ui.input",
            Self::UiEditorDialog => "ui.editor_dialog",
            Self::Session => "session",
            Self::Models => "models",
            Self::Events => "events",
            Self::Providers => "providers",
            Self::ProviderCalls => "provider_calls",
            Self::Renderers => "renderers",
            Self::RuntimeActions => "runtime_actions",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionBackendInfo {
    pub name: &'static str,
    pub api_version: u32,
    pub capabilities: BTreeSet<ExtensionCapability>,
}

impl ExtensionBackendInfo {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            api_version: EXTENSION_API_VERSION,
            capabilities: BTreeSet::new(),
        }
    }

    pub fn with_capabilities(
        name: &'static str,
        capabilities: impl IntoIterator<Item = ExtensionCapability>,
    ) -> Self {
        Self {
            name,
            api_version: EXTENSION_API_VERSION,
            capabilities: capabilities.into_iter().collect(),
        }
    }

    pub fn supports(&self, capability: ExtensionCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn capability_names(&self) -> Vec<&'static str> {
        self.capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect()
    }
}

/// Common metadata contract implemented by every extension backend.
pub trait ExtensionBackend {
    fn backend_info(&self) -> ExtensionBackendInfo;
}

/// Capabilities currently provided by the Rust-native cdylib backend.
pub fn native_backend_info() -> ExtensionBackendInfo {
    ExtensionBackendInfo::with_capabilities(
        "native-rust",
        [
            ExtensionCapability::Tools,
            ExtensionCapability::Commands,
            ExtensionCapability::Resources,
            ExtensionCapability::Session,
            ExtensionCapability::Models,
            ExtensionCapability::Events,
            ExtensionCapability::Providers,
            ExtensionCapability::ProviderCalls,
            ExtensionCapability::Renderers,
            ExtensionCapability::RuntimeActions,
        ],
    )
}

impl ExtensionBackend for rpi_extensions::ExtensionSession {
    fn backend_info(&self) -> ExtensionBackendInfo {
        native_backend_info()
    }
}
