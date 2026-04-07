use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::McpTransport;
use crate::mcp_client::{McpClientAuth, McpRemoteTransport};
use crate::mcp_types::*;

#[derive(Debug, Clone)]
pub(crate) struct ManagedMcpRemoteServer {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) headers: BTreeMap<String, String>,
}

pub(crate) fn plain_http_remote_server(
    server_name: &str,
    transport: McpTransport,
    remote: &McpRemoteTransport,
) -> Result<ManagedMcpRemoteServer, String> {
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

#[derive(Debug)]
pub(crate) struct HttpTransportClient {
    server_name: String,
    url: String,
    headers: BTreeMap<String, String>,
    initialized: bool,
}

impl HttpTransportClient {
    pub fn new(server_name: String, url: String, headers: BTreeMap<String, String>) -> Self {
        Self {
            server_name,
            url,
            headers,
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

        let client = reqwest::Client::new();
        // initialize
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
        let mut req = client
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req
            .json(&init_request)
            .send()
            .await
            .map_err(|e| format!("initialize failed: {e}"))?;
        let _: JsonRpcResponse = resp
            .json()
            .await
            .map_err(|e| format!("initialize parse failed: {e}"))?;

        // notifications/initialized
        let notif = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let mut req = client
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let _ = req.json(&notif).send().await;

        self.initialized = true;
        Ok(())
    }

    pub async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> Result<JsonRpcResponse<McpListToolsResult>, String> {
        let client = reqwest::Client::new();
        let request = JsonRpcRequest::new(id, "tools/list", params);
        let mut req = client
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        resp.json()
            .await
            .map_err(|e| format!("tools/list parse failed: {e}"))
    }

    pub async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> Result<JsonRpcResponse<McpToolCallResult>, String> {
        let client = reqwest::Client::new();
        let request = JsonRpcRequest::new(id, "tools/call", Some(params));
        let mut req = client
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("tools/call failed: {e}"))?;
        resp.json()
            .await
            .map_err(|e| format!("tools/call parse failed: {e}"))
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        self.initialized = false;
        Ok(())
    }
}
