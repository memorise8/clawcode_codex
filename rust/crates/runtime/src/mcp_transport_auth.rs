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
    /// OAuth configuration — not yet implemented, stored for future use.
    OAuth { client_id: Option<String> },
}

impl RemoteTransportConfig {
    /// Build from MCP client remote transport config.
    /// Returns the config if the transport is supportable, or an error reason.
    pub fn from_remote(
        _server_name: &str,
        remote: &McpRemoteTransport,
    ) -> Result<Self, String> {
        let auth = match &remote.auth {
            McpClientAuth::None => RemoteAuth::None,
            McpClientAuth::OAuth(oauth_config) => {
                // Accept OAuth config but mark it for future implementation
                RemoteAuth::OAuth {
                    client_id: oauth_config.client_id.clone(),
                }
            }
        };

        Ok(Self {
            url: remote.url.clone(),
            headers: remote.headers.clone(),
            auth,
            headers_helper: remote.headers_helper.clone(),
        })
    }

    /// Apply headers and auth to a request builder.
    /// Currently applies static headers only. OAuth and headersHelper
    /// will be implemented in future iterations.
    pub fn apply_headers(&self, headers: &mut BTreeMap<String, String>) {
        for (key, value) in &self.headers {
            headers.insert(key.clone(), value.clone());
        }
    }

    /// Returns true if this config uses features not yet fully implemented.
    pub fn has_unimplemented_features(&self) -> Vec<&'static str> {
        let mut warnings = Vec::new();
        if self.headers_helper.is_some() {
            warnings.push("headersHelper is configured but not yet executed");
        }
        if matches!(self.auth, RemoteAuth::OAuth { .. }) {
            warnings.push(
                "OAuth is configured but token refresh is not yet implemented for MCP transports",
            );
        }
        warnings
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
    fn from_remote_accepts_oauth_with_warning() {
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::new(),
            headers_helper: None,
            auth: McpClientAuth::OAuth(McpOAuthConfig {
                client_id: Some("id".to_string()),
                callback_port: None,
                auth_server_metadata_url: None,
                xaa: None,
            }),
        };
        let config = RemoteTransportConfig::from_remote("test", &remote).unwrap();
        let warnings = config.has_unimplemented_features();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("OAuth"));
    }

    #[test]
    fn apply_headers_merges_static_headers() {
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: BTreeMap::from([("Authorization".to_string(), "Bearer tok".to_string())]),
            headers_helper: None,
            auth: McpClientAuth::None,
        };
        let config = RemoteTransportConfig::from_remote("test", &remote).unwrap();
        let mut headers = BTreeMap::new();
        config.apply_headers(&mut headers);
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer tok");
    }
}
