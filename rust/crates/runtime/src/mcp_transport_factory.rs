use crate::config::{McpTransport, ScopedMcpServerConfig};
use crate::mcp_client::{McpClientBootstrap, McpClientTransport};
use crate::mcp_transport::TransportClient;
use crate::mcp_transport_auth::RemoteTransportConfig;
use crate::mcp_transport_http::HttpTransportClient;
use crate::mcp_transport_sse::SseTransportClient;
use crate::mcp_transport_stdio::StdioTransportClient;
use crate::mcp_types::UnsupportedMcpServer;

/// Result of attempting to build a transport for a single MCP server.
pub(crate) enum TransportBuildResult {
    /// Successfully created a transport client.
    Ok(TransportClient),
    /// Server configuration is not supported.
    Unsupported(UnsupportedMcpServer),
}

/// Build a [`TransportClient`] from a server config entry.
///
/// Returns `Ok(transport)` or `Unsupported` with a human-readable reason.
pub(crate) fn build_transport(
    server_name: &str,
    server_config: &ScopedMcpServerConfig,
) -> TransportBuildResult {
    let bootstrap = McpClientBootstrap::from_scoped_config(server_name, server_config);
    match (&bootstrap.transport, server_config.transport()) {
        (McpClientTransport::Stdio(_), McpTransport::Stdio) => {
            TransportBuildResult::Ok(TransportClient::Stdio(StdioTransportClient::new(
                server_name.to_string(),
                bootstrap,
            )))
        }
        (McpClientTransport::Sse(remote), transport @ McpTransport::Sse) => {
            match RemoteTransportConfig::from_remote(server_name, remote) {
                Ok(config) => {
                    for warning in config.has_unimplemented_features() {
                        eprintln!("  \x1b[33mMCP server `{server_name}`: {warning}\x1b[0m");
                    }
                    TransportBuildResult::Ok(TransportClient::Sse(SseTransportClient::new(
                        server_name.to_string(),
                        config,
                    )))
                }
                Err(reason) => TransportBuildResult::Unsupported(UnsupportedMcpServer {
                    server_name: server_name.to_string(),
                    transport,
                    reason,
                }),
            }
        }
        (McpClientTransport::Http(remote), transport @ McpTransport::Http) => {
            match RemoteTransportConfig::from_remote(server_name, remote) {
                Ok(config) => {
                    for warning in config.has_unimplemented_features() {
                        eprintln!("  \x1b[33mMCP server `{server_name}`: {warning}\x1b[0m");
                    }
                    TransportBuildResult::Ok(TransportClient::Http(
                        HttpTransportClient::from_config(server_name.to_string(), config),
                    ))
                }
                Err(reason) => TransportBuildResult::Unsupported(UnsupportedMcpServer {
                    server_name: server_name.to_string(),
                    transport,
                    reason,
                }),
            }
        }
        (_, other) => TransportBuildResult::Unsupported(UnsupportedMcpServer {
            server_name: server_name.to_string(),
            transport: other,
            reason: format!("transport {other:?} is not supported by McpServerManager"),
        }),
    }
}
