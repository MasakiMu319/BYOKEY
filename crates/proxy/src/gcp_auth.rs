//! Google Cloud Platform token management for Vertex AI backends.
//!
//! Wraps the `gcp_auth` crate to lazily create and cache
//! [`TokenProvider`](gcp_auth::TokenProvider) instances keyed by
//! credentials file path.  The `gcp_auth` crate handles internal
//! token refresh/caching, so this module only caches the providers.

use byokey_types::ByokError;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

const CLOUD_PLATFORM_SCOPE: &[&str] = &["https://www.googleapis.com/auth/cloud-platform"];

/// Manages GCP access tokens for Vertex AI requests.
///
/// Each distinct `credentials_file` path (or `None` for ADC) gets its own
/// cached [`TokenProvider`].  Token refresh is handled internally by `gcp_auth`.
pub struct GcpTokenManager {
    providers: Mutex<HashMap<Option<PathBuf>, Arc<dyn gcp_auth::TokenProvider>>>,
}

impl Default for GcpTokenManager {
    fn default() -> Self {
        Self {
            providers: Mutex::new(HashMap::new()),
        }
    }
}

impl GcpTokenManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Obtain a GCP access token string.
    ///
    /// - `credentials_file = Some(path)` → service account JSON key.
    /// - `credentials_file = None` → Application Default Credentials
    ///   (`GOOGLE_APPLICATION_CREDENTIALS` env var, metadata server, etc.).
    ///
    /// # Errors
    ///
    /// Returns [`ByokError::Auth`] if credentials cannot be loaded or the
    /// token exchange fails.
    pub async fn get_token(&self, credentials_file: Option<&str>) -> Result<String, ByokError> {
        let key = credentials_file.map(PathBuf::from);

        // Fast path: provider already cached.
        {
            let providers = self.providers.lock().await;
            if let Some(provider) = providers.get(&key) {
                let token = provider
                    .token(CLOUD_PLATFORM_SCOPE)
                    .await
                    .map_err(|e| ByokError::Auth(format!("GCP token error: {e}")))?;
                return Ok(token.as_str().to_owned());
            }
        }

        // Slow path: create and cache a new provider.
        let provider: Arc<dyn gcp_auth::TokenProvider> = if let Some(path) = &key {
            let sa = gcp_auth::CustomServiceAccount::from_file(path)
                .map_err(|e| ByokError::Auth(format!("GCP credentials file error: {e}")))?;
            Arc::new(sa)
        } else {
            gcp_auth::provider()
                .await
                .map_err(|e| ByokError::Auth(format!("GCP default credentials error: {e}")))?
        };

        let token = provider
            .token(CLOUD_PLATFORM_SCOPE)
            .await
            .map_err(|e| ByokError::Auth(format!("GCP token error: {e}")))?;
        let token_str = token.as_str().to_owned();

        self.providers.lock().await.insert(key, provider);

        Ok(token_str)
    }
}
