use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value as JsonValue;

use crate::config::McpTransport;
use crate::mcp::mcp_tool_name;
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

pub(crate) async fn discover_remote_server_tools(
    server: &ManagedMcpRemoteServer,
) -> Result<Vec<ManagedMcpTool>, String> {
    let client = reqwest::Client::new();

        // Send JSON-RPC initialize request
        let init_request = JsonRpcRequest::new(
            next_remote_request_id(),
            "initialize",
            Some(serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "claw-code",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })),
        );

        let mut req = client
            .post(&server.url)
            .header("content-type", "application/json");
        for (key, value) in &server.headers {
            req = req.header(key, value);
        }

        let response = req
            .json(&init_request)
            .send()
            .await
            .map_err(|e| format!("initialize failed: {e}"))?;
        let _init_result: JsonRpcResponse = response
            .json()
            .await
            .map_err(|e| format!("initialize parse failed: {e}"))?;

        // Send initialized notification (no response expected)
        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let mut req = client
            .post(&server.url)
            .header("content-type", "application/json");
        for (key, value) in &server.headers {
            req = req.header(key, value);
        }
        let _ = req.json(&initialized).send().await;

        // Send tools/list request
        let list_request = JsonRpcRequest::new(
            next_remote_request_id(),
            "tools/list",
            None::<JsonValue>,
        );

        let mut req = client
            .post(&server.url)
            .header("content-type", "application/json");
        for (key, value) in &server.headers {
            req = req.header(key, value);
        }

        let response = req
            .json(&list_request)
            .send()
            .await
            .map_err(|e| format!("tools/list failed: {e}"))?;
        let list_result: JsonRpcResponse<McpListToolsResult> = response
            .json()
            .await
            .map_err(|e| format!("tools/list parse failed: {e}"))?;

        let mut tools = Vec::new();
        if let Some(result) = list_result.result {
            for tool in result.tools {
                let qualified_name = mcp_tool_name(&server.name, &tool.name);
                tools.push(ManagedMcpTool {
                    server_name: server.name.clone(),
                    qualified_name,
                    raw_name: tool.name.clone(),
                    tool,
                });
            }
        }

    Ok(tools)
}
