use std::io;

use crate::mcp_transport_auth::RemoteTransportConfig;
use crate::mcp_types::*;

/// SSE transport client — stub implementation.
/// Currently returns NotImplemented errors for all operations.
/// Will be fully implemented when SSE streaming support is added.
#[derive(Debug)]
pub(crate) struct SseTransportClient {
    server_name: String,
    #[allow(dead_code)]
    config: RemoteTransportConfig,
}

impl SseTransportClient {
    pub fn new(server_name: String, config: RemoteTransportConfig) -> Self {
        Self {
            server_name,
            config,
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub async fn ensure_ready(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "SSE transport for MCP server '{}' is not yet implemented",
                self.server_name
            ),
        ))
    }

    pub async fn list_tools(
        &mut self,
        _id: JsonRpcId,
        _params: Option<McpListToolsParams>,
    ) -> io::Result<JsonRpcResponse<McpListToolsResult>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SSE list_tools not implemented",
        ))
    }

    pub async fn call_tool(
        &mut self,
        _id: JsonRpcId,
        _params: McpToolCallParams,
    ) -> io::Result<JsonRpcResponse<McpToolCallResult>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SSE call_tool not implemented",
        ))
    }

    pub async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}
