use std::collections::BTreeMap;

use crate::mcp_client::{McpClientAuth, McpRemoteTransport};

/// Describes the auth/header decoration capabilities for a remote transport.
#[derive(Debug, Clone)]
pub(crate) struct RemoteTransportConfig {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub auth: RemoteAuth,
    pub headers_helper: Option<String>,
}

/// Auth mode for a remote MCP transport.
#[derive(Debug, Clone)]
pub(crate) enum RemoteAuth {
    None,
    /// OAuth configuration with Bearer token injection and refresh support.
    OAuth {
        /// Key used to load/save credentials via load_oauth_credentials_for_provider().
        provider_key: String,
        client_id: Option<String>,
        callback_port: Option<u16>,
        auth_server_metadata_url: Option<String>,
    },
}

impl RemoteTransportConfig {
    /// Build from MCP client remote transport config.
    /// Returns the config if the transport is supportable, or an error reason.
    pub fn from_remote(
        server_name: &str,
        remote: &McpRemoteTransport,
    ) -> Result<Self, String> {
        let auth = match &remote.auth {
            McpClientAuth::None => RemoteAuth::None,
            McpClientAuth::OAuth(oauth_config) => RemoteAuth::OAuth {
                provider_key: format!("mcp_oauth::{server_name}"),
                client_id: oauth_config.client_id.clone(),
                callback_port: oauth_config.callback_port,
                auth_server_metadata_url: oauth_config.auth_server_metadata_url.clone(),
            },
        };

        Ok(Self {
            url: remote.url.clone(),
            headers: remote.headers.clone(),
            auth,
            headers_helper: remote.headers_helper.clone(),
        })
    }

    /// Build request headers including static headers and OAuth Bearer token if available.
    /// Returns the complete set of headers to apply to the HTTP request.
    pub fn authorize_request(&self) -> BTreeMap<String, String> {
        let mut headers = BTreeMap::new();

        // Apply static headers first
        for (key, value) in &self.headers {
            headers.insert(key.clone(), value.clone());
        }

        // Apply OAuth Bearer token if configured and available
        if let RemoteAuth::OAuth { ref provider_key, .. } = self.auth {
            match crate::oauth::load_oauth_credentials_for_provider(provider_key) {
                Ok(Some(token_set)) if !token_set.access_token.is_empty() => {
                    headers.insert(
                        "authorization".to_string(),
                        format!("Bearer {}", token_set.access_token),
                    );
                }
                Ok(_) => {
                    // No saved credentials — proceed without auth
                }
                Err(error) => {
                    eprintln!("warning: failed to load MCP OAuth credentials for {provider_key}: {error}");
                }
            }
        }

        headers
    }

    /// Returns descriptions of features not yet fully implemented.
    pub fn has_unimplemented_features(&self) -> Vec<&'static str> {
        let mut warnings = Vec::new();
        if self.headers_helper.is_some() {
            warnings.push("headersHelper is configured but not yet executed");
        }
        warnings
    }

    /// Whether this config has OAuth with a refresh_token available.
    pub fn can_refresh_token(&self) -> bool {
        if let RemoteAuth::OAuth { ref provider_key, ref client_id, .. } = self.auth {
            if client_id.is_none() {
                return false;
            }
            if let Ok(Some(token_set)) = crate::oauth::load_oauth_credentials_for_provider(provider_key) {
                return token_set.refresh_token.is_some();
            }
        }
        false
    }

    /// Attempt to refresh the OAuth token using the stored refresh_token.
    pub async fn try_refresh_token(&self) -> Result<(), String> {
        let RemoteAuth::OAuth { ref provider_key, ref client_id, .. } = self.auth else {
            return Err("not an OAuth config".to_string());
        };
        let client_id = client_id.as_deref().ok_or("missing client_id for token refresh")?;
        let token_set = crate::oauth::load_oauth_credentials_for_provider(provider_key)
            .map_err(|e| format!("failed to load credentials: {e}"))?
            .ok_or("no saved credentials to refresh")?;
        let refresh_token = token_set
            .refresh_token
            .as_deref()
            .ok_or("no refresh_token available")?;

        let token_url = self.derive_token_url().await?;

        let http = reqwest::Client::new();
        let response = http
            .post(&token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .map_err(|e| format!("token refresh request failed: {e}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("token refresh failed ({status}): {body}"));
        }

        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("token refresh parse failed: {e}"))?;

        let access_token = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or("refresh response missing access_token")?;

        let new_refresh = value
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .filter(|t| !t.is_empty())
            .map(ToOwned::to_owned)
            .or(token_set.refresh_token.clone());

        let expires_at = value.get("expires_in").and_then(serde_json::Value::as_u64).map(|secs| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                + secs
        });

        crate::oauth::save_oauth_credentials_for_provider(
            provider_key,
            &crate::OAuthTokenSet {
                access_token: access_token.to_string(),
                refresh_token: new_refresh,
                expires_at,
                scopes: token_set.scopes.clone(),
            },
        )
        .map_err(|e| format!("failed to save refreshed credentials: {e}"))?;

        Ok(())
    }

    /// Resolve the OAuth token endpoint by fetching the authorization server
    /// metadata document (RFC 8414). The `auth_server_metadata_url` points to
    /// a JSON document containing a `token_endpoint` field — NOT the token
    /// endpoint itself.
    async fn derive_token_url(&self) -> Result<String, String> {
        let RemoteAuth::OAuth { ref auth_server_metadata_url, .. } = self.auth else {
            return Err("not an OAuth config".to_string());
        };
        let metadata_url = auth_server_metadata_url.as_deref()
            .ok_or("no auth_server_metadata_url configured for token refresh")?;

        let http = reqwest::Client::new();
        let response = http.get(metadata_url)
            .send()
            .await
            .map_err(|e| format!("failed to fetch OAuth metadata from {metadata_url}: {e}"))?;

        if !response.status().is_success() {
            return Err(format!(
                "OAuth metadata request to {metadata_url} returned {}",
                response.status()
            ));
        }

        let metadata: serde_json::Value = response.json()
            .await
            .map_err(|e| format!("failed to parse OAuth metadata from {metadata_url}: {e}"))?;

        metadata.get("token_endpoint")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| format!("OAuth metadata at {metadata_url} missing token_endpoint field"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpOAuthConfig;
    use crate::mcp_client::McpRemoteTransport;

    #[test]
    fn from_remote_accepts_plain_http() {
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let config = RemoteTransportConfig::from_remote("test", &remote).unwrap();
        assert_eq!(config.url, "http://localhost:8080");
        assert!(config.has_unimplemented_features().is_empty());
    }

    #[test]
    fn from_remote_accepts_headers_helper_with_warning() {
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            headers_helper: Some("helper.sh".to_string()),
            auth: McpClientAuth::None,
        };
        let config = RemoteTransportConfig::from_remote("test", &remote).unwrap();
        let warnings = config.has_unimplemented_features();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("headersHelper"));
    }

    #[test]
    fn from_remote_accepts_oauth_config() {
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::OAuth(McpOAuthConfig {
                client_id: Some("id".to_string()),
                callback_port: Some(9999),
                auth_server_metadata_url: Some("https://auth.example.com/token".to_string()),
                xaa: None,
            }),
        };
        let config = RemoteTransportConfig::from_remote("my-server", &remote).unwrap();
        // OAuth is now implemented — no unimplemented warnings for OAuth
        let warnings = config.has_unimplemented_features();
        assert!(warnings.is_empty(), "expected no warnings, got: {warnings:?}");
        // Verify provider_key is derived from server name
        if let RemoteAuth::OAuth { ref provider_key, ref client_id, callback_port, ref auth_server_metadata_url } = config.auth {
            assert_eq!(provider_key, "mcp_oauth::my-server");
            assert_eq!(client_id.as_deref(), Some("id"));
            assert_eq!(callback_port, Some(9999));
            assert_eq!(auth_server_metadata_url.as_deref(), Some("https://auth.example.com/token"));
        } else {
            panic!("expected OAuth auth variant");
        }
    }

    #[test]
    fn authorize_request_merges_static_headers() {
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::from([("x-custom".to_string(), "value".to_string())]),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let config = RemoteTransportConfig::from_remote("test", &remote).unwrap();
        let headers = config.authorize_request();
        assert_eq!(headers.get("x-custom").unwrap(), "value");
        // No authorization header without OAuth
        assert!(headers.get("authorization").is_none());
    }

    #[test]
    fn authorize_request_injects_bearer_token() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let _guard = crate::test_env_lock();
        let config_home = std::env::temp_dir().join(format!(
            "runtime-auth-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &config_home);

        let provider_key = "mcp_oauth::test-bearer";
        crate::oauth::save_oauth_credentials_for_provider(
            provider_key,
            &crate::OAuthTokenSet {
                access_token: "test-access-token".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: vec![],
            },
        )
        .expect("save credentials");

        let config = RemoteTransportConfig {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            auth: RemoteAuth::OAuth {
                provider_key: provider_key.to_string(),
                client_id: Some("cid".to_string()),
                callback_port: None,
                auth_server_metadata_url: None,
            },
            headers_helper: None,
        };
        let headers = config.authorize_request();
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer test-access-token"
        );

        std::env::remove_var("CLAUDE_CONFIG_HOME");
        std::fs::remove_dir_all(config_home).expect("cleanup");
    }

    #[test]
    fn can_refresh_token_requires_client_id_and_refresh_token() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let _guard = crate::test_env_lock();
        let config_home = std::env::temp_dir().join(format!(
            "runtime-refresh-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &config_home);

        let provider_key = "mcp_oauth::test-refresh";

        // No client_id -> cannot refresh
        let config_no_cid = RemoteTransportConfig {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            auth: RemoteAuth::OAuth {
                provider_key: provider_key.to_string(),
                client_id: None,
                callback_port: None,
                auth_server_metadata_url: None,
            },
            headers_helper: None,
        };
        assert!(!config_no_cid.can_refresh_token());

        // Has client_id but no saved credentials -> cannot refresh
        let config_with_cid = RemoteTransportConfig {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            auth: RemoteAuth::OAuth {
                provider_key: provider_key.to_string(),
                client_id: Some("cid".to_string()),
                callback_port: None,
                auth_server_metadata_url: None,
            },
            headers_helper: None,
        };
        assert!(!config_with_cid.can_refresh_token());

        // Save credentials without refresh_token -> cannot refresh
        crate::oauth::save_oauth_credentials_for_provider(
            provider_key,
            &crate::OAuthTokenSet {
                access_token: "at".to_string(),
                refresh_token: None,
                expires_at: None,
                scopes: vec![],
            },
        )
        .expect("save");
        assert!(!config_with_cid.can_refresh_token());

        // Save credentials with refresh_token -> can refresh
        crate::oauth::save_oauth_credentials_for_provider(
            provider_key,
            &crate::OAuthTokenSet {
                access_token: "at".to_string(),
                refresh_token: Some("rt".to_string()),
                expires_at: None,
                scopes: vec![],
            },
        )
        .expect("save");
        assert!(config_with_cid.can_refresh_token());

        // Not OAuth -> cannot refresh
        let config_none = RemoteTransportConfig {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            auth: RemoteAuth::None,
            headers_helper: None,
        };
        assert!(!config_none.can_refresh_token());

        std::env::remove_var("CLAUDE_CONFIG_HOME");
        std::fs::remove_dir_all(config_home).expect("cleanup");
    }
}
