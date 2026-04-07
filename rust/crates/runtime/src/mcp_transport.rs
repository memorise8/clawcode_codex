use serde_json::Value as JsonValue;

use crate::mcp_types::*;

/// Default MCP initialize parameters for all transport types.
pub(crate) fn default_initialize_params() -> McpInitializeParams {
    McpInitializeParams {
        protocol_version: "2025-03-26".to_string(),
        capabilities: JsonValue::Object(serde_json::Map::new()),
        client_info: McpInitializeClientInfo {
            name: "runtime".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    }
}

#[derive(Debug)]
pub(crate) enum TransportClient {
    Stdio(crate::mcp_transport_stdio::StdioTransportClient),
    Http(crate::mcp_transport_http::HttpTransportClient),
}

impl TransportClient {
    pub fn server_name(&self) -> &str {
        match self {
            Self::Stdio(t) => t.server_name(),
            Self::Http(t) => t.server_name(),
        }
    }

    pub async fn ensure_ready(&mut self) -> Result<(), McpServerManagerError> {
        match self {
            Self::Stdio(t) => {
                let response = t.ensure_ready().await?;
                let server_name = t.server_name().to_string();
                if let Some(error) = response.error {
                    return Err(McpServerManagerError::JsonRpc {
                        server_name,
                        method: "initialize",
                        error,
                    });
                }
                Ok(())
            }
            Self::Http(t) => {
                let server_name = t.server_name().to_string();
                t.ensure_ready().await.map_err(|details| {
                    McpServerManagerError::InvalidResponse {
                        server_name,
                        method: "initialize",
                        details,
                    }
                })
            }
        }
    }

    pub async fn list_tools(
        &mut self,
        id: JsonRpcId,
        params: Option<McpListToolsParams>,
    ) -> Result<JsonRpcResponse<McpListToolsResult>, McpServerManagerError> {
        match self {
            Self::Stdio(t) => {
                let response = t.list_tools(id, params).await?;
                Ok(response)
            }
            Self::Http(t) => {
                let server_name = t.server_name().to_string();
                t.list_tools(id, params).await.map_err(|details| {
                    McpServerManagerError::InvalidResponse {
                        server_name,
                        method: "tools/list",
                        details,
                    }
                })
            }
        }
    }

    pub async fn call_tool(
        &mut self,
        id: JsonRpcId,
        params: McpToolCallParams,
    ) -> Result<JsonRpcResponse<McpToolCallResult>, McpServerManagerError> {
        match self {
            Self::Stdio(t) => {
                let response = t.call_tool(id, params).await?;
                Ok(response)
            }
            Self::Http(t) => {
                let server_name = t.server_name().to_string();
                t.call_tool(id, params).await.map_err(|details| {
                    McpServerManagerError::InvalidResponse {
                        server_name,
                        method: "tools/call",
                        details,
                    }
                })
            }
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), McpServerManagerError> {
        match self {
            Self::Stdio(t) => {
                t.shutdown().await?;
                Ok(())
            }
            Self::Http(t) => {
                let server_name = t.server_name().to_string();
                t.shutdown().await.map_err(|details| {
                    McpServerManagerError::InvalidResponse {
                        server_name,
                        method: "shutdown",
                        details,
                    }
                })
            }
        }
    }
}
