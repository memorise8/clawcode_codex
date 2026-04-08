use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::mcp_types::*;

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct ManagedMcpRemoteServer {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) headers: BTreeMap<String, String>,
}

#[cfg(test)]
pub(crate) fn plain_http_remote_server(
    server_name: &str,
    transport: crate::config::McpTransport,
    remote: &crate::mcp_client::McpRemoteTransport,
) -> Result<ManagedMcpRemoteServer, String> {
    use crate::config::McpTransport;
    use crate::mcp_client::McpClientAuth;
    if remote.headers_helper.is_some() {
        return Err(format!(
            "{transport:?} transport with headersHelper is not yet supported by McpServerManager"
        ));
    }
    if matches!(remote.auth, McpClientAuth::OAuth(_)) {
        return Err(format!(
            "{transport:?} transport with oauth is not yet supported by McpServerManager"
        ));
    }
    if transport == McpTransport::Sse {
        return Err("Sse transport is not yet supported by McpServerManager".to_string());
    }

    Ok(ManagedMcpRemoteServer {
        name: server_name.to_string(),
        url: remote.url.clone(),
        headers: remote.headers.clone(),
    })
}

/// Global counter for remote JSON-RPC request IDs to avoid collisions.
static REMOTE_REQUEST_ID: AtomicU64 = AtomicU64::new(1000);

pub(crate) fn next_remote_request_id() -> JsonRpcId {
    JsonRpcId::Number(REMOTE_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
}

use crate::mcp_transport::default_initialize_params;

use crate::mcp_transport_auth::RemoteTransportConfig;

/// Send a JSON-RPC request over HTTP and parse the typed response.
async fn send_jsonrpc_request<TParams: serde::Serialize, TResult: serde::de::DeserializeOwned>(
    config: &RemoteTransportConfig,
    request: &JsonRpcRequest<TParams>,
) -> Result<JsonRpcResponse<TResult>, String> {
    let client = reqwest::Client::new();
    let mut applied_headers = BTreeMap::new();
    config.apply_headers(&mut applied_headers);
    let mut req = client.post(&config.url).header("content-type", "application/json");
    for (k, v) in &applied_headers {
        req = req.header(k, v);
    }
    let resp = req
        .json(request)
        .send()
        .await
        .map_err(|e| format!("{} failed: {e}", request.method))?;
    resp.json()
        .await
        .map_err(|e| format!("{} parse failed: {e}", request.method))
}

/// Fire-and-forget a JSON-RPC notification over HTTP (no response expected).
async fn send_jsonrpc_notification(
    config: &RemoteTransportConfig,
    notification: &serde_json::Value,
) {
    let client = reqwest::Client::new();
    let mut applied_headers = BTreeMap::new();
    config.apply_headers(&mut applied_headers);
    let mut req = client.post(&config.url).header("content-type", "application/json");
    for (k, v) in &applied_headers {
        req = req.header(k, v);
    }
    let _ = req.json(notification).send().await;
}

#[derive(Debug)]
pub(crate) struct HttpTransportClient {
    server_name: String,
    config: RemoteTransportConfig,
    initialized: bool,
}

impl HttpTransportClient {
    #[cfg(test)]
    pub fn new(server_name: String, url: String, headers: BTreeMap<String, String>) -> Self {
        use crate::mcp_transport_auth::RemoteAuth;
        Self {
            server_name,
            config: RemoteTransportConfig {
                url,
                headers,
                auth: RemoteAuth::None,
                headers_helper: None,
            },
            initialized: false,
        }
    }

    pub fn from_config(
        server_name: String,
        config: RemoteTransportConfig,
    ) -> Self {
        Self {
            server_name,
            config,
            initialized: false,
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub async fn ensure_ready(&mut self) -> Result<(), String> {
        if self.initialized {
            return Ok(());
        }

        let init_params = default_initialize_params();
        let init_request = JsonRpcRequest::new(
            next_remote_request_id(),
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": init_params.protocol_version,
                "capabilities": init_params.capabilities,
                "clientInfo": {
                    "name": init_params.client_info.name,
                    "version": init_params.client_info.version,
                }
            })),
        );
        let _: JsonRpcResponse = send_jsonrpc_request(&self.config, &init_request).await?;

        let notif = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        send_jsonrpc_notification(&self.config, &notif).await;

        self.initialized = true;
        Ok(())
    }

    pub async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> Result<JsonRpcResponse<McpListToolsResult>, String> {
        let request = JsonRpcRequest::new(id, "tools/list", params);
        send_jsonrpc_request(&self.config, &request).await
    }

    pub async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> Result<JsonRpcResponse<McpToolCallResult>, String> {
        let request = JsonRpcRequest::new(id, "tools/call", Some(params));
        send_jsonrpc_request(&self.config, &request).await
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.initialized = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_transport::default_initialize_params;

    #[test]
    fn default_protocol_version_is_2025() {
        // Regression: remote server discovery was sending "2024-11-05" while
        // stdio used "2025-03-26". All transports must use the same version.
        let params = default_initialize_params();
        assert_eq!(params.protocol_version, "2025-03-26");
    }

    #[test]
    fn remote_request_ids_are_unique() {
        // Regression: remote JSON-RPC ids were hardcoded constants.
        let ids: Vec<_> = (0..10).map(|_| next_remote_request_id()).collect();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            match id {
                crate::mcp_types::JsonRpcId::Number(n) => {
                    assert!(seen.insert(*n), "duplicate id: {n}");
                }
                other => panic!("expected numeric id, got {other:?}"),
            }
        }
    }

    #[test]
    fn plain_http_rejects_headers_helper() {
        use crate::mcp_client::McpRemoteTransport;
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: std::collections::BTreeMap::new(),
            headers_helper: Some("helper.sh".to_string()),
            auth: crate::mcp_client::McpClientAuth::None,
        };
        let result = plain_http_remote_server("test", crate::config::McpTransport::Http, &remote);
        assert!(result.is_err(), "headersHelper should be rejected");
    }

    #[test]
    fn plain_http_rejects_oauth() {
        use crate::mcp_client::McpRemoteTransport;
        let remote = McpRemoteTransport {
            url: "http://localhost:8080".to_string(),
            headers: std::collections::BTreeMap::new(),
            headers_helper: None,
            auth: crate::mcp_client::McpClientAuth::OAuth(crate::config::McpOAuthConfig {
                client_id: Some("id".to_string()),
                callback_port: None,
                auth_server_metadata_url: None,
                xaa: None,
            }),
        };
        let result = plain_http_remote_server("test", crate::config::McpTransport::Http, &remote);
        assert!(result.is_err(), "OAuth should be rejected");
    }
}
