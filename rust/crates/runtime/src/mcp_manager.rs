use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::config::{RuntimeConfig, ScopedMcpServerConfig};
use crate::mcp::mcp_tool_name;
use crate::mcp_transport::TransportClient;
use crate::mcp_transport_factory::{build_transport, TransportBuildResult};

use crate::mcp_types::*;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolRoute {
    server_name: String,
    raw_name: String,
}

#[derive(Debug)]
pub struct McpServerManager {
    pub(crate) transports: BTreeMap<String, TransportClient>,
    unsupported_servers: Vec<UnsupportedMcpServer>,
    tool_index: BTreeMap<String, ToolRoute>,
    next_request_id: u64,
}

impl McpServerManager {
    #[must_use]
    pub fn from_runtime_config(config: &RuntimeConfig) -> Self {
        Self::from_servers(config.mcp().servers())
    }

    #[must_use]
    pub fn from_servers(servers: &BTreeMap<String, ScopedMcpServerConfig>) -> Self {
        let mut transports = BTreeMap::new();
        let mut unsupported_servers = Vec::new();

        for (server_name, server_config) in servers {
            match build_transport(server_name, server_config) {
                TransportBuildResult::Ok(transport) => {
                    transports.insert(server_name.clone(), transport);
                }
                TransportBuildResult::Unsupported(unsupported) => {
                    unsupported_servers.push(unsupported);
                }
            }
        }

        Self {
            transports,
            unsupported_servers,
            tool_index: BTreeMap::new(),
            next_request_id: 1,
        }
    }

    #[must_use]
    pub fn unsupported_servers(&self) -> &[UnsupportedMcpServer] {
        &self.unsupported_servers
    }

    pub async fn discover_tools(&mut self) -> Result<Vec<ManagedMcpTool>, McpServerManagerError> {
        let server_names = self.transports.keys().cloned().collect::<Vec<_>>();
        let mut discovered_tools = Vec::new();

        for server_name in server_names {
            let transport = self.transports.get_mut(&server_name).ok_or_else(|| {
                McpServerManagerError::UnknownServer {
                    server_name: server_name.clone(),
                }
            })?;

            if let Err(error) = transport.ensure_ready().await {
                eprintln!(
                    "warning: MCP server '{}' skipped during discovery: {error}",
                    server_name
                );
                continue;
            }

            // Clear existing routes for this server
            self.tool_index
                .retain(|_, route| route.server_name != server_name);

            let mut cursor = None;
            loop {
                let request_id = self.take_request_id();
                let transport =
                    self.transports.get_mut(&server_name).ok_or_else(|| {
                        McpServerManagerError::UnknownServer {
                            server_name: server_name.clone(),
                        }
                    })?;

                let response = match transport
                    .list_tools(
                        request_id,
                        Some(McpListToolsParams {
                            cursor: cursor.clone(),
                        }),
                    )
                    .await
                {
                    Ok(resp) => resp,
                    Err(error) => {
                        eprintln!(
                            "warning: MCP server '{}' skipped during tool listing: {error}",
                            server_name
                        );
                        break;
                    }
                };

                if let Some(error) = response.error {
                    eprintln!(
                        "warning: MCP server '{}' returned error during tool listing: {}",
                        server_name, error.message
                    );
                    break;
                }

                let result = match response.result {
                    Some(r) => r,
                    None => {
                        eprintln!(
                            "warning: MCP server '{}' returned empty result during tool listing",
                            server_name
                        );
                        break;
                    }
                };

                for tool in result.tools {
                    let qualified_name = mcp_tool_name(&server_name, &tool.name);
                    self.tool_index.insert(
                        qualified_name.clone(),
                        ToolRoute {
                            server_name: server_name.clone(),
                            raw_name: tool.name.clone(),
                        },
                    );
                    discovered_tools.push(ManagedMcpTool {
                        server_name: server_name.clone(),
                        qualified_name,
                        raw_name: tool.name.clone(),
                        tool,
                    });
                }

                match result.next_cursor {
                    Some(next_cursor) => cursor = Some(next_cursor),
                    None => break,
                }
            }
        }

        Ok(discovered_tools)
    }

    pub async fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<JsonValue>,
    ) -> Result<JsonRpcResponse<McpToolCallResult>, McpServerManagerError> {
        let route = self
            .tool_index
            .get(qualified_tool_name)
            .cloned()
            .ok_or_else(|| McpServerManagerError::UnknownTool {
                qualified_name: qualified_tool_name.to_string(),
            })?;

        let transport =
            self.transports
                .get_mut(&route.server_name)
                .ok_or_else(|| McpServerManagerError::UnknownServer {
                    server_name: route.server_name.clone(),
                })?;

        transport.ensure_ready().await?;
        let request_id = self.take_request_id();
        let transport =
            self.transports
                .get_mut(&route.server_name)
                .ok_or_else(|| McpServerManagerError::UnknownServer {
                    server_name: route.server_name.clone(),
                })?;

        let response = transport
            .call_tool(
                request_id,
                McpToolCallParams {
                    name: route.raw_name,
                    arguments,
                    meta: None,
                },
            )
            .await?;
        Ok(response)
    }

    pub async fn shutdown(&mut self) -> Result<(), McpServerManagerError> {
        let server_names = self.transports.keys().cloned().collect::<Vec<_>>();
        for server_name in server_names {
            if let Some(transport) = self.transports.get_mut(&server_name) {
                transport.shutdown().await?;
            }
        }
        Ok(())
    }

    fn take_request_id(&mut self) -> JsonRpcId {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        JsonRpcId::Number(id)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::{json, Value};
    use tokio::runtime::Builder;

    use crate::config::{
        ConfigSource, McpRemoteServerConfig, McpSdkServerConfig, McpServerConfig,
        McpStdioServerConfig, McpWebSocketServerConfig, ScopedMcpServerConfig,
    };
    use crate::mcp::mcp_tool_name;
    use crate::mcp_types::McpServerManagerError;
    use super::McpServerManager;

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-mcp-manager-{nanos}"))
    }

    struct RemoteHttpTestServer {
        addr: SocketAddr,
        methods: Arc<Mutex<Vec<String>>>,
        shutdown: Option<std::sync::mpsc::Sender<()>>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl RemoteHttpTestServer {
        fn spawn() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind remote http server");
            listener
                .set_nonblocking(true)
                .expect("set nonblocking listener");
            let addr = listener.local_addr().expect("local addr");
            let methods = Arc::new(Mutex::new(Vec::new()));
            let methods_for_thread = Arc::clone(&methods);
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            let handle = thread::spawn(move || loop {
                if rx.try_recv().is_ok() {
                    break;
                }

                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buffer = [0_u8; 4096];
                        let size = stream.read(&mut buffer).expect("read request");
                        let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
                        let body = request
                            .split_once("\r\n\r\n")
                            .map(|(_, body)| body)
                            .unwrap_or_default();
                        let value: Value =
                            serde_json::from_str(body).expect("parse JSON-RPC request");
                        let method = value
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        methods_for_thread
                            .lock()
                            .expect("methods lock")
                            .push(method.clone());

                        let response = match method.as_str() {
                            "initialize" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": value.get("id").cloned().unwrap_or(Value::Null),
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": {},
                                    "serverInfo": {
                                        "name": "remote-http-test",
                                        "version": "0.1.0"
                                    }
                                }
                            }),
                            "notifications/initialized" => serde_json::json!({}),
                            "tools/list" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": value.get("id").cloned().unwrap_or(Value::Null),
                                "result": {
                                    "tools": [
                                        {
                                            "name": "echo",
                                            "description": "Echo remote input",
                                            "inputSchema": {
                                                "type": "object",
                                                "properties": {
                                                    "text": { "type": "string" }
                                                }
                                            }
                                        }
                                    ]
                                }
                            }),
                            "tools/call" => {
                                let text = value
                                    .get("params")
                                    .and_then(|params| params.get("arguments"))
                                    .and_then(|args| args.get("text"))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": value.get("id").cloned().unwrap_or(Value::Null),
                                    "result": {
                                        "content": [
                                            {
                                                "type": "text",
                                                "text": format!("echo:{text}")
                                            }
                                        ],
                                        "structuredContent": {
                                            "echo": text
                                        }
                                    }
                                })
                            }
                            other => panic!("unexpected remote method: {other}"),
                        };

                        let response_body =
                            serde_json::to_string(&response).expect("serialize response");
                        let http_response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            response_body.len(),
                            response_body,
                        );
                        stream
                            .write_all(http_response.as_bytes())
                            .expect("write response");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("remote server accept failed: {error}"),
                }
            });

            Self {
                addr,
                methods,
                shutdown: Some(tx),
                handle: Some(handle),
            }
        }

        fn addr(&self) -> SocketAddr {
            self.addr
        }

        fn methods(&self) -> Vec<String> {
            self.methods.lock().expect("methods lock").clone()
        }
    }

    impl Drop for RemoteHttpTestServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn write_manager_mcp_server_script() -> PathBuf {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("temp dir");
        let script_path = root.join("manager-mcp-server.py");
        let script = [
            "#!/usr/bin/env python3",
            "import json, os, sys",
            "",
            "LABEL = os.environ.get('MCP_SERVER_LABEL', 'server')",
            "LOG_PATH = os.environ.get('MCP_LOG_PATH')",
            "initialize_count = 0",
            "",
            "def log(method):",
            "    if LOG_PATH:",
            "        with open(LOG_PATH, 'a', encoding='utf-8') as handle:",
            "            handle.write(f'{method}\\n')",
            "",
            "def read_message():",
            "    header = b''",
            r"    while not header.endswith(b'\r\n\r\n'):",
            "        chunk = sys.stdin.buffer.read(1)",
            "        if not chunk:",
            "            return None",
            "        header += chunk",
            "    length = 0",
            r"    for line in header.decode().split('\r\n'):",
            r"        if line.lower().startswith('content-length:'):",
            r"            length = int(line.split(':', 1)[1].strip())",
            "    payload = sys.stdin.buffer.read(length)",
            "    return json.loads(payload.decode())",
            "",
            "def send_message(message):",
            "    payload = json.dumps(message).encode()",
            r"    sys.stdout.buffer.write(f'Content-Length: {len(payload)}\r\n\r\n'.encode() + payload)",
            "    sys.stdout.buffer.flush()",
            "",
            "while True:",
            "    request = read_message()",
            "    if request is None:",
            "        break",
            "    method = request['method']",
            "    log(method)",
            "    if method == 'initialize':",
            "        initialize_count += 1",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'protocolVersion': request['params']['protocolVersion'],",
            "                'capabilities': {'tools': {}},",
            "                'serverInfo': {'name': LABEL, 'version': '1.0.0'}",
            "            }",
            "        })",
            "    elif method == 'tools/list':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'tools': [",
            "                    {",
            "                        'name': 'echo',",
            "                        'description': f'Echo tool for {LABEL}',",
            "                        'inputSchema': {",
            "                            'type': 'object',",
            "                            'properties': {'text': {'type': 'string'}},",
            "                            'required': ['text']",
            "                        }",
            "                    }",
            "                ]",
            "            }",
            "        })",
            "    elif method == 'tools/call':",
            "        args = request['params'].get('arguments') or {}",
            "        text = args.get('text', '')",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'content': [{'type': 'text', 'text': f'{LABEL}:{text}'}],",
            "                'structuredContent': {",
            "                    'server': LABEL,",
            "                    'echoed': text,",
            "                    'initializeCount': initialize_count",
            "                },",
            "                'isError': False",
            "            }",
            "        })",
            "    else:",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'error': {'code': -32601, 'message': f'unknown method: {method}'},",
            "        })",
            "",
        ]
        .join("\n");
        fs::write(&script_path, script).expect("write script");
        let mut permissions = fs::metadata(&script_path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod");
        script_path
    }

    fn cleanup_script(script_path: &Path) {
        fs::remove_file(script_path).expect("cleanup script");
        fs::remove_dir_all(script_path.parent().expect("script parent")).expect("cleanup dir");
    }

    fn manager_server_config(
        script_path: &Path,
        label: &str,
        log_path: &Path,
    ) -> ScopedMcpServerConfig {
        ScopedMcpServerConfig {
            scope: ConfigSource::Local,
            config: McpServerConfig::Stdio(McpStdioServerConfig {
                command: "python3".to_string(),
                args: vec![script_path.to_string_lossy().into_owned()],
                env: BTreeMap::from([
                    ("MCP_SERVER_LABEL".to_string(), label.to_string()),
                    (
                        "MCP_LOG_PATH".to_string(),
                        log_path.to_string_lossy().into_owned(),
                    ),
                ]),
            }),
        }
    }

    #[test]
    fn manager_discovers_tools_from_stdio_config() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_manager_mcp_server_script();
            let root = script_path.parent().expect("script parent");
            let log_path = root.join("alpha.log");
            let servers = BTreeMap::from([(
                "alpha".to_string(),
                manager_server_config(&script_path, "alpha", &log_path),
            )]);
            let mut manager = McpServerManager::from_servers(&servers);

            let tools = manager.discover_tools().await.expect("discover tools");

            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].server_name, "alpha");
            assert_eq!(tools[0].raw_name, "echo");
            assert_eq!(tools[0].qualified_name, mcp_tool_name("alpha", "echo"));
            assert_eq!(tools[0].tool.name, "echo");
            assert!(manager.unsupported_servers().is_empty());

            manager.shutdown().await.expect("shutdown");
            cleanup_script(&script_path);
        });
    }

    #[test]
    fn manager_routes_tool_calls_to_correct_server() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_manager_mcp_server_script();
            let root = script_path.parent().expect("script parent");
            let alpha_log = root.join("alpha.log");
            let beta_log = root.join("beta.log");
            let servers = BTreeMap::from([
                (
                    "alpha".to_string(),
                    manager_server_config(&script_path, "alpha", &alpha_log),
                ),
                (
                    "beta".to_string(),
                    manager_server_config(&script_path, "beta", &beta_log),
                ),
            ]);
            let mut manager = McpServerManager::from_servers(&servers);

            let tools = manager.discover_tools().await.expect("discover tools");
            assert_eq!(tools.len(), 2);

            let alpha = manager
                .call_tool(
                    &mcp_tool_name("alpha", "echo"),
                    Some(json!({"text": "hello"})),
                )
                .await
                .expect("call alpha tool");
            let beta = manager
                .call_tool(
                    &mcp_tool_name("beta", "echo"),
                    Some(json!({"text": "world"})),
                )
                .await
                .expect("call beta tool");

            assert_eq!(
                alpha
                    .result
                    .as_ref()
                    .and_then(|result| result.structured_content.as_ref())
                    .and_then(|value| value.get("server")),
                Some(&json!("alpha"))
            );
            assert_eq!(
                beta.result
                    .as_ref()
                    .and_then(|result| result.structured_content.as_ref())
                    .and_then(|value| value.get("server")),
                Some(&json!("beta"))
            );

            manager.shutdown().await.expect("shutdown");
            cleanup_script(&script_path);
        });
    }

    #[test]
    fn manager_records_unsupported_non_stdio_servers_without_panicking() {
        let servers = BTreeMap::from([
            (
                "http".to_string(),
                ScopedMcpServerConfig {
                    scope: ConfigSource::Local,
                    config: McpServerConfig::Http(McpRemoteServerConfig {
                        url: "https://example.test/mcp".to_string(),
                        headers: BTreeMap::new(),
                        headers_helper: None,
                        oauth: None,
                    }),
                },
            ),
            (
                "sdk".to_string(),
                ScopedMcpServerConfig {
                    scope: ConfigSource::Local,
                    config: McpServerConfig::Sdk(McpSdkServerConfig {
                        name: "sdk-server".to_string(),
                    }),
                },
            ),
            (
                "ws".to_string(),
                ScopedMcpServerConfig {
                    scope: ConfigSource::Local,
                    config: McpServerConfig::Ws(McpWebSocketServerConfig {
                        url: "wss://example.test/mcp".to_string(),
                        headers: BTreeMap::new(),
                        headers_helper: None,
                    }),
                },
            ),
        ]);

        let manager = McpServerManager::from_servers(&servers);
        let unsupported = manager.unsupported_servers();

        assert_eq!(unsupported.len(), 2);
        assert_eq!(unsupported[0].server_name, "sdk");
        assert_eq!(unsupported[1].server_name, "ws");

        // HTTP server should now be accepted as a transport
        assert_eq!(
            manager
                .transports
                .values()
                .filter(|t| matches!(t, super::TransportClient::Http(_)))
                .count(),
            1
        );
    }

    #[test]
    fn manager_accepts_remote_servers_with_unimplemented_features() {
        // headersHelper and OAuth are now accepted (with warnings) instead of rejected.
        // SSE is accepted as a transport stub.
        let servers = BTreeMap::from([
            (
                "http-helper".to_string(),
                ScopedMcpServerConfig {
                    scope: ConfigSource::Local,
                    config: McpServerConfig::Http(McpRemoteServerConfig {
                        url: "https://example.test/mcp".to_string(),
                        headers: BTreeMap::new(),
                        headers_helper: Some("headers.sh".to_string()),
                        oauth: None,
                    }),
                },
            ),
            (
                "http-oauth".to_string(),
                ScopedMcpServerConfig {
                    scope: ConfigSource::Local,
                    config: McpServerConfig::Http(McpRemoteServerConfig {
                        url: "https://example.test/mcp".to_string(),
                        headers: BTreeMap::new(),
                        headers_helper: None,
                        oauth: Some(crate::config::McpOAuthConfig {
                            client_id: Some("client-id".to_string()),
                            callback_port: None,
                            auth_server_metadata_url: None,
                            xaa: None,
                        }),
                    }),
                },
            ),
            (
                "sse".to_string(),
                ScopedMcpServerConfig {
                    scope: ConfigSource::Local,
                    config: McpServerConfig::Sse(McpRemoteServerConfig {
                        url: "https://example.test/sse".to_string(),
                        headers: BTreeMap::new(),
                        headers_helper: None,
                        oauth: None,
                    }),
                },
            ),
        ]);

        let manager = McpServerManager::from_servers(&servers);
        let unsupported = manager.unsupported_servers();

        // All three are now accepted as transports
        assert!(unsupported.is_empty(), "expected no unsupported servers, got: {unsupported:?}");
        assert_eq!(
            manager
                .transports
                .values()
                .filter(|t| matches!(t, super::TransportClient::Http(_)))
                .count(),
            2,
            "http-helper and http-oauth should be Http transports"
        );
        assert_eq!(
            manager
                .transports
                .values()
                .filter(|t| matches!(t, super::TransportClient::Sse(_)))
                .count(),
            1,
            "sse should be an Sse transport"
        );
    }

    #[test]
    fn manager_discovers_and_calls_plain_http_remote_tools() {
        let server = RemoteHttpTestServer::spawn();
        let servers = BTreeMap::from([(
            "http".to_string(),
            ScopedMcpServerConfig {
                scope: ConfigSource::Local,
                config: McpServerConfig::Http(McpRemoteServerConfig {
                    url: format!("http://{}/mcp", server.addr()),
                    headers: BTreeMap::new(),
                    headers_helper: None,
                    oauth: None,
                }),
            },
        )]);
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let mut manager = McpServerManager::from_servers(&servers);
            let tools = manager.discover_tools().await.expect("discover tools");
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].qualified_name, mcp_tool_name("http", "echo"));

            let response = manager
                .call_tool(&mcp_tool_name("http", "echo"), Some(json!({"text": "hello"})))
                .await
                .expect("call remote tool");

            assert_eq!(
                response
                    .result
                    .as_ref()
                    .and_then(|result| result.structured_content.as_ref())
                    .and_then(|value| value.get("echo")),
                Some(&json!("hello"))
            );
        });

        assert_eq!(
            server.methods(),
            vec![
                "initialize".to_string(),
                "notifications/initialized".to_string(),
                "tools/list".to_string(),
                "tools/call".to_string(),
            ]
        );
    }

    #[test]
    fn manager_shutdown_terminates_spawned_children_and_is_idempotent() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_manager_mcp_server_script();
            let root = script_path.parent().expect("script parent");
            let log_path = root.join("alpha.log");
            let servers = BTreeMap::from([(
                "alpha".to_string(),
                manager_server_config(&script_path, "alpha", &log_path),
            )]);
            let mut manager = McpServerManager::from_servers(&servers);

            manager.discover_tools().await.expect("discover tools");
            manager.shutdown().await.expect("first shutdown");
            manager.shutdown().await.expect("second shutdown");

            cleanup_script(&script_path);
        });
    }

    #[test]
    fn manager_reuses_spawned_server_between_discovery_and_call() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_manager_mcp_server_script();
            let root = script_path.parent().expect("script parent");
            let log_path = root.join("alpha.log");
            let servers = BTreeMap::from([(
                "alpha".to_string(),
                manager_server_config(&script_path, "alpha", &log_path),
            )]);
            let mut manager = McpServerManager::from_servers(&servers);

            manager.discover_tools().await.expect("discover tools");
            let response = manager
                .call_tool(
                    &mcp_tool_name("alpha", "echo"),
                    Some(json!({"text": "reuse"})),
                )
                .await
                .expect("call tool");

            assert_eq!(
                response
                    .result
                    .as_ref()
                    .and_then(|result| result.structured_content.as_ref())
                    .and_then(|value| value.get("initializeCount")),
                Some(&json!(1))
            );

            let log = fs::read_to_string(&log_path).expect("read log");
            assert_eq!(log.lines().filter(|line| *line == "initialize").count(), 1);
            assert_eq!(
                log.lines().collect::<Vec<_>>(),
                vec!["initialize", "tools/list", "tools/call"]
            );

            manager.shutdown().await.expect("shutdown");
            cleanup_script(&script_path);
        });
    }

    #[test]
    fn discover_tools_skips_failing_transports() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_manager_mcp_server_script();
            let root = script_path.parent().expect("script parent");
            let log_path = root.join("alpha.log");

            // Build a config with a working stdio server AND an SSE server (which will fail ensure_ready)
            let servers = BTreeMap::from([
                (
                    "alpha".to_string(),
                    manager_server_config(&script_path, "alpha", &log_path),
                ),
                (
                    "sse-server".to_string(),
                    ScopedMcpServerConfig {
                        scope: ConfigSource::Local,
                        config: McpServerConfig::Sse(McpRemoteServerConfig {
                            url: "https://example.test/sse".to_string(),
                            headers: BTreeMap::new(),
                            headers_helper: None,
                            oauth: None,
                        }),
                    },
                ),
            ]);
            let mut manager = McpServerManager::from_servers(&servers);

            // SSE is accepted as a transport (not unsupported)
            assert!(
                manager.unsupported_servers().is_empty(),
                "SSE should be accepted as a transport, not unsupported"
            );

            // discover_tools must succeed despite SSE failing ensure_ready
            let tools = manager.discover_tools().await.expect("discover_tools should not fail");

            // The stdio server's tools are discovered
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].server_name, "alpha");
            assert_eq!(tools[0].raw_name, "echo");

            manager.shutdown().await.expect("shutdown");
            cleanup_script(&script_path);
        });
    }

    #[test]
    fn manager_reports_unknown_qualified_tool_name() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_manager_mcp_server_script();
            let root = script_path.parent().expect("script parent");
            let log_path = root.join("alpha.log");
            let servers = BTreeMap::from([(
                "alpha".to_string(),
                manager_server_config(&script_path, "alpha", &log_path),
            )]);
            let mut manager = McpServerManager::from_servers(&servers);

            let error = manager
                .call_tool(
                    &mcp_tool_name("alpha", "missing"),
                    Some(json!({"text": "nope"})),
                )
                .await
                .expect_err("unknown qualified tool should fail");

            match error {
                McpServerManagerError::UnknownTool { qualified_name } => {
                    assert_eq!(qualified_name, mcp_tool_name("alpha", "missing"));
                }
                other => panic!("expected unknown tool error, got {other:?}"),
            }

            cleanup_script(&script_path);
        });
    }
}
