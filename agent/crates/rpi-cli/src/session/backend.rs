//! Construction only: callers must first enforce `native_store`'s fresh,
//! print-only selection policy. JSONL construction and writer locks stay in
//! the parent module's existing typed-repository flow.

use std::{path::Path, sync::Arc};

use rpi_harness::session::types::SessionStorage;

use super::BuildError;

/// Create a fresh experimental session, never restore or select an existing one.
/// Keep the feature check here so the selection branch has one construction path.
pub(super) async fn fresh_experimental_native(
    root: &Path,
) -> Result<Arc<dyn SessionStorage>, BuildError> {
    #[cfg(feature = "experimental-native-store")]
    {
        use rpi_harness::session::{
            experimental_native::ExperimentalNativeRepo,
            types::{SessionCreateOptions, SessionRepo},
        };

        let repo = ExperimentalNativeRepo::new(root.to_path_buf())
            .await
            .map_err(|e| BuildError::SessionDir(e.to_string()))?;
        let storage = repo
            .create(&SessionCreateOptions::default())
            .await
            .map_err(|e| BuildError::SessionOpen(e.to_string()))?;
        Ok(Arc::new(storage))
    }
    #[cfg(not(feature = "experimental-native-store"))]
    {
        let _ = root;
        Err(BuildError::NativeStorePolicy(
            "binary lacks experimental-native-store feature".into(),
        ))
    }
}
