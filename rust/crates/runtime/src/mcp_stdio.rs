use std::collections::BTreeMap;

use serde_json::Value as JsonValue;

use crate::config::{McpTransport, RuntimeConfig, ScopedMcpServerConfig};
use crate::mcp::mcp_tool_name;
use crate::mcp_client::{
    McpClientBootstrap, McpClientTransport,
};

pub(crate) use crate::mcp_transport_http::*;
pub use crate::mcp_transport_stdio::*;
pub use crate::mcp_types::*;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolRoute {
    server_name: String,
    raw_name: String,
}

#[derive(Debug)]
struct ManagedMcpServer {
    bootstrap: McpClientBootstrap,
    process: Option<McpStdioProcess>,
    initialized: bool,
}

impl ManagedMcpServer {
    fn new(bootstrap: McpClientBootstrap) -> Self {
        Self {
            bootstrap,
            process: None,
            initialized: false,
        }
    }
}


#[derive(Debug)]
pub struct McpServerManager {
    servers: BTreeMap<String, ManagedMcpServer>,
    remote_servers: Vec<ManagedMcpRemoteServer>,
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
        let mut managed_servers = BTreeMap::new();
        let mut remote_servers = Vec::new();
        let mut unsupported_servers = Vec::new();

        for (server_name, server_config) in servers {
            let bootstrap = McpClientBootstrap::from_scoped_config(server_name, server_config);
            match (&bootstrap.transport, server_config.transport()) {
                (McpClientTransport::Stdio(_), McpTransport::Stdio) => {
                    managed_servers.insert(server_name.clone(), ManagedMcpServer::new(bootstrap));
                }
                (McpClientTransport::Sse(remote), transport @ McpTransport::Sse)
                | (McpClientTransport::Http(remote), transport @ McpTransport::Http) => {
                    match plain_http_remote_server(server_name, transport, remote) {
                        Ok(server) => remote_servers.push(server),
                        Err(reason) => unsupported_servers.push(UnsupportedMcpServer {
                            server_name: server_name.clone(),
                            transport,
                            reason,
                        }),
                    }
                }
                (_, other) => {
                    unsupported_servers.push(UnsupportedMcpServer {
                        server_name: server_name.clone(),
                        transport: other,
                        reason: format!(
                            "transport {other:?} is not supported by McpServerManager",
                        ),
                    });
                }
            }
        }

        Self {
            servers: managed_servers,
            remote_servers,
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
        let server_names = self.servers.keys().cloned().collect::<Vec<_>>();
        let mut discovered_tools = Vec::new();

        for server_name in server_names {
            self.ensure_server_ready(&server_name).await?;
            self.clear_routes_for_server(&server_name);

            let mut cursor = None;
            loop {
                let request_id = self.take_request_id();
                let response = {
                    let server = self.server_mut(&server_name)?;
                    let process = server.process.as_mut().ok_or_else(|| {
                        McpServerManagerError::InvalidResponse {
                            server_name: server_name.clone(),
                            method: "tools/list",
                            details: "server process missing after initialization".to_string(),
                        }
                    })?;
                    process
                        .list_tools(
                            request_id,
                            Some(McpListToolsParams {
                                cursor: cursor.clone(),
                            }),
                        )
                        .await?
                };

                if let Some(error) = response.error {
                    return Err(McpServerManagerError::JsonRpc {
                        server_name: server_name.clone(),
                        method: "tools/list",
                        error,
                    });
                }

                let result =
                    response
                        .result
                        .ok_or_else(|| McpServerManagerError::InvalidResponse {
                            server_name: server_name.clone(),
                            method: "tools/list",
                            details: "missing result payload".to_string(),
                        })?;

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

        // Discover tools from remote (SSE/HTTP) servers
        let remote_tools = self.discover_remote_tools().await;
        for tool in remote_tools {
            self.tool_index.insert(
                tool.qualified_name.clone(),
                ToolRoute {
                    server_name: tool.server_name.clone(),
                    raw_name: tool.raw_name.clone(),
                },
            );
            discovered_tools.push(tool);
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

        // Check if this routes to a remote server
        if let Some(_remote) = self
            .remote_servers
            .iter()
            .find(|s| s.name == route.server_name)
        {
            return self
                .call_remote_tool(&route.server_name, &route.raw_name, &arguments)
                .await;
        }

        self.ensure_server_ready(&route.server_name).await?;
        let request_id = self.take_request_id();
        let response =
            {
                let server = self.server_mut(&route.server_name)?;
                let process = server.process.as_mut().ok_or_else(|| {
                    McpServerManagerError::InvalidResponse {
                        server_name: route.server_name.clone(),
                        method: "tools/call",
                        details: "server process missing after initialization".to_string(),
                    }
                })?;
                process
                    .call_tool(
                        request_id,
                        McpToolCallParams {
                            name: route.raw_name,
                            arguments,
                            meta: None,
                        },
                    )
                    .await?
            };
        Ok(response)
    }

    async fn discover_remote_tools(&self) -> Vec<ManagedMcpTool> {
        let mut tools = Vec::new();
        for server in &self.remote_servers {
            match discover_remote_server_tools(server).await {
                Ok(server_tools) => {
                    for tool in server_tools {
                        tools.push(tool);
                    }
                }
                Err(error) => {
                    eprintln!(
                        "warning: failed to discover tools from MCP server '{}': {error}",
                        server.name
                    );
                }
            }
        }
        tools
    }

    async fn call_remote_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: &Option<JsonValue>,
    ) -> Result<JsonRpcResponse<McpToolCallResult>, McpServerManagerError> {
        let server = self
            .remote_servers
            .iter()
            .find(|s| s.name == server_name)
            .ok_or_else(|| McpServerManagerError::UnknownServer {
                server_name: server_name.to_string(),
            })?;

        let client = reqwest::Client::new();
        let request = JsonRpcRequest::new(
            next_remote_request_id(),
            "tools/call",
            Some(McpToolCallParams {
                name: tool_name.to_string(),
                arguments: arguments.clone(),
                meta: None,
            }),
        );

        let mut req = client
            .post(&server.url)
            .header("content-type", "application/json");
        for (key, value) in &server.headers {
            req = req.header(key, value);
        }

        let response = req.json(&request).send().await.map_err(|e| {
            McpServerManagerError::InvalidResponse {
                server_name: server_name.to_string(),
                method: "tools/call",
                details: format!("HTTP request failed: {e}"),
            }
        })?;

        let result: JsonRpcResponse<McpToolCallResult> = response.json().await.map_err(|e| {
            McpServerManagerError::InvalidResponse {
                server_name: server_name.to_string(),
                method: "tools/call",
                details: format!("failed to parse response: {e}"),
            }
        })?;

        if let Some(error) = result.error.clone() {
            return Err(McpServerManagerError::JsonRpc {
                server_name: server_name.to_string(),
                method: "tools/call",
                error,
            });
        }

        Ok(result)
    }

    pub async fn shutdown(&mut self) -> Result<(), McpServerManagerError> {
        let server_names = self.servers.keys().cloned().collect::<Vec<_>>();
        for server_name in server_names {
            let server = self.server_mut(&server_name)?;
            if let Some(process) = server.process.as_mut() {
                process.shutdown().await?;
            }
            server.process = None;
            server.initialized = false;
        }
        Ok(())
    }

    fn clear_routes_for_server(&mut self, server_name: &str) {
        self.tool_index
            .retain(|_, route| route.server_name != server_name);
    }

    fn server_mut(
        &mut self,
        server_name: &str,
    ) -> Result<&mut ManagedMcpServer, McpServerManagerError> {
        self.servers
            .get_mut(server_name)
            .ok_or_else(|| McpServerManagerError::UnknownServer {
                server_name: server_name.to_string(),
            })
    }

    fn take_request_id(&mut self) -> JsonRpcId {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        JsonRpcId::Number(id)
    }

    async fn ensure_server_ready(
        &mut self,
        server_name: &str,
    ) -> Result<(), McpServerManagerError> {
        let needs_spawn = self
            .servers
            .get(server_name)
            .map(|server| server.process.is_none())
            .ok_or_else(|| McpServerManagerError::UnknownServer {
                server_name: server_name.to_string(),
            })?;

        if needs_spawn {
            let server = self.server_mut(server_name)?;
            server.process = Some(spawn_mcp_stdio_process(&server.bootstrap)?);
            server.initialized = false;
        }

        let needs_initialize = self
            .servers
            .get(server_name)
            .map(|server| !server.initialized)
            .ok_or_else(|| McpServerManagerError::UnknownServer {
                server_name: server_name.to_string(),
            })?;

        if needs_initialize {
            let request_id = self.take_request_id();
            let response = {
                let server = self.server_mut(server_name)?;
                let process = server.process.as_mut().ok_or_else(|| {
                    McpServerManagerError::InvalidResponse {
                        server_name: server_name.to_string(),
                        method: "initialize",
                        details: "server process missing before initialize".to_string(),
                    }
                })?;
                process
                    .initialize(request_id, default_initialize_params())
                    .await?
            };

            if let Some(error) = response.error {
                return Err(McpServerManagerError::JsonRpc {
                    server_name: server_name.to_string(),
                    method: "initialize",
                    error,
                });
            }

            if response.result.is_none() {
                return Err(McpServerManagerError::InvalidResponse {
                    server_name: server_name.to_string(),
                    method: "initialize",
                    details: "missing result payload".to_string(),
                });
            }

            let server = self.server_mut(server_name)?;
            server.initialized = true;
        }

        Ok(())
    }
}


fn default_initialize_params() -> McpInitializeParams {
    McpInitializeParams {
        protocol_version: "2025-03-26".to_string(),
        capabilities: JsonValue::Object(serde_json::Map::new()),
        client_info: McpInitializeClientInfo {
            name: "runtime".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
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
    use crate::mcp_client::McpClientBootstrap;

    use super::{
        spawn_mcp_stdio_process, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
        McpInitializeClientInfo, McpInitializeParams, McpInitializeResult, McpInitializeServerInfo,
        McpListToolsResult, McpReadResourceParams, McpReadResourceResult, McpServerManager,
        McpServerManagerError, McpStdioProcess, McpTool, McpToolCallParams,
    };

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-mcp-stdio-{nanos}"))
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

    fn write_echo_script() -> PathBuf {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("temp dir");
        let script_path = root.join("echo-mcp.sh");
        fs::write(
            &script_path,
            "#!/bin/sh\nprintf 'READY:%s\\n' \"$MCP_TEST_TOKEN\"\nIFS= read -r line\nprintf 'ECHO:%s\\n' \"$line\"\n",
        )
        .expect("write script");
        let mut permissions = fs::metadata(&script_path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod");
        script_path
    }

    fn write_jsonrpc_script() -> PathBuf {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("temp dir");
        let script_path = root.join("jsonrpc-mcp.py");
        let script = [
            "#!/usr/bin/env python3",
            "import json, sys",
            "header = b''",
            r"while not header.endswith(b'\r\n\r\n'):",
            "    chunk = sys.stdin.buffer.read(1)",
            "    if not chunk:",
            "        raise SystemExit(1)",
            "    header += chunk",
            "length = 0",
            r"for line in header.decode().split('\r\n'):",
            r"    if line.lower().startswith('content-length:'):",
            r"        length = int(line.split(':', 1)[1].strip())",
            "payload = sys.stdin.buffer.read(length)",
            "request = json.loads(payload.decode())",
            r"assert request['jsonrpc'] == '2.0'",
            r"assert request['method'] == 'initialize'",
            r"response = json.dumps({",
            r"    'jsonrpc': '2.0',",
            r"    'id': request['id'],",
            r"    'result': {",
            r"        'protocolVersion': request['params']['protocolVersion'],",
            r"        'capabilities': {'tools': {}},",
            r"        'serverInfo': {'name': 'fake-mcp', 'version': '0.1.0'}",
            r"    }",
            r"}).encode()",
            r"sys.stdout.buffer.write(f'Content-Length: {len(response)}\r\n\r\n'.encode() + response)",
            "sys.stdout.buffer.flush()",
            "",
        ]
        .join("\n");
        fs::write(&script_path, script).expect("write script");
        let mut permissions = fs::metadata(&script_path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script_path, permissions).expect("chmod");
        script_path
    }

    #[allow(clippy::too_many_lines)]
    fn write_mcp_server_script() -> PathBuf {
        let root = temp_dir();
        fs::create_dir_all(&root).expect("temp dir");
        let script_path = root.join("fake-mcp-server.py");
        let script = [
            "#!/usr/bin/env python3",
            "import json, sys",
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
            "    if method == 'initialize':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'protocolVersion': request['params']['protocolVersion'],",
            "                'capabilities': {'tools': {}, 'resources': {}},",
            "                'serverInfo': {'name': 'fake-mcp', 'version': '0.2.0'}",
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
            "                        'description': 'Echoes text',",
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
            "        if request['params']['name'] == 'fail':",
            "            send_message({",
            "                'jsonrpc': '2.0',",
            "                'id': request['id'],",
            "                'error': {'code': -32001, 'message': 'tool failed'},",
            "            })",
            "        else:",
            "            text = args.get('text', '')",
            "            send_message({",
            "                'jsonrpc': '2.0',",
            "                'id': request['id'],",
            "                'result': {",
            "                    'content': [{'type': 'text', 'text': f'echo:{text}'}],",
            "                    'structuredContent': {'echoed': text},",
            "                    'isError': False",
            "                }",
            "            })",
            "    elif method == 'resources/list':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'resources': [",
            "                    {",
            "                        'uri': 'file://guide.txt',",
            "                        'name': 'guide',",
            "                        'description': 'Guide text',",
            "                        'mimeType': 'text/plain'",
            "                    }",
            "                ]",
            "            }",
            "        })",
            "    elif method == 'resources/read':",
            "        uri = request['params']['uri']",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'contents': [",
            "                    {",
            "                        'uri': uri,",
            "                        'mimeType': 'text/plain',",
            "                        'text': f'contents for {uri}'",
            "                    }",
            "                ]",
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

    fn sample_bootstrap(script_path: &Path) -> McpClientBootstrap {
        let config = ScopedMcpServerConfig {
            scope: ConfigSource::Local,
            config: McpServerConfig::Stdio(McpStdioServerConfig {
                command: "/bin/sh".to_string(),
                args: vec![script_path.to_string_lossy().into_owned()],
                env: BTreeMap::from([("MCP_TEST_TOKEN".to_string(), "secret-value".to_string())]),
            }),
        };
        McpClientBootstrap::from_scoped_config("stdio server", &config)
    }

    fn script_transport(script_path: &Path) -> crate::mcp_client::McpStdioTransport {
        crate::mcp_client::McpStdioTransport {
            command: "python3".to_string(),
            args: vec![script_path.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
        }
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
    fn spawns_stdio_process_and_round_trips_io() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_echo_script();
            let bootstrap = sample_bootstrap(&script_path);
            let mut process = spawn_mcp_stdio_process(&bootstrap).expect("spawn stdio process");

            let ready = process.read_line().await.expect("read ready");
            assert_eq!(ready, "READY:secret-value\n");

            process
                .write_line("ping from client")
                .await
                .expect("write line");

            let echoed = process.read_line().await.expect("read echo");
            assert_eq!(echoed, "ECHO:ping from client\n");

            let status = process.wait().await.expect("wait for exit");
            assert!(status.success());

            cleanup_script(&script_path);
        });
    }

    #[test]
    fn rejects_non_stdio_bootstrap() {
        let config = ScopedMcpServerConfig {
            scope: ConfigSource::Local,
            config: McpServerConfig::Sdk(crate::config::McpSdkServerConfig {
                name: "sdk-server".to_string(),
            }),
        };
        let bootstrap = McpClientBootstrap::from_scoped_config("sdk server", &config);
        let error = spawn_mcp_stdio_process(&bootstrap).expect_err("non-stdio should fail");
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn round_trips_initialize_request_and_response_over_stdio_frames() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_jsonrpc_script();
            let transport = script_transport(&script_path);
            let mut process = McpStdioProcess::spawn(&transport).expect("spawn transport directly");

            let response = process
                .initialize(
                    JsonRpcId::Number(1),
                    McpInitializeParams {
                        protocol_version: "2025-03-26".to_string(),
                        capabilities: json!({"roots": {}}),
                        client_info: McpInitializeClientInfo {
                            name: "runtime-tests".to_string(),
                            version: "0.1.0".to_string(),
                        },
                    },
                )
                .await
                .expect("initialize roundtrip");

            assert_eq!(response.id, JsonRpcId::Number(1));
            assert_eq!(response.error, None);
            assert_eq!(
                response.result,
                Some(McpInitializeResult {
                    protocol_version: "2025-03-26".to_string(),
                    capabilities: json!({"tools": {}}),
                    server_info: McpInitializeServerInfo {
                        name: "fake-mcp".to_string(),
                        version: "0.1.0".to_string(),
                    },
                })
            );

            let status = process.wait().await.expect("wait for exit");
            assert!(status.success());

            cleanup_script(&script_path);
        });
    }

    #[test]
    fn write_jsonrpc_request_emits_content_length_frame() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_jsonrpc_script();
            let transport = script_transport(&script_path);
            let mut process = McpStdioProcess::spawn(&transport).expect("spawn transport directly");
            let request = JsonRpcRequest::new(
                JsonRpcId::Number(7),
                "initialize",
                Some(json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "runtime-tests", "version": "0.1.0"}
                })),
            );

            process.send_request(&request).await.expect("send request");
            let response: JsonRpcResponse<serde_json::Value> =
                process.read_response().await.expect("read response");

            assert_eq!(response.id, JsonRpcId::Number(7));
            assert_eq!(response.jsonrpc, "2.0");

            let status = process.wait().await.expect("wait for exit");
            assert!(status.success());

            cleanup_script(&script_path);
        });
    }

    #[test]
    fn direct_spawn_uses_transport_env() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_echo_script();
            let transport = crate::mcp_client::McpStdioTransport {
                command: "/bin/sh".to_string(),
                args: vec![script_path.to_string_lossy().into_owned()],
                env: BTreeMap::from([("MCP_TEST_TOKEN".to_string(), "direct-secret".to_string())]),
            };
            let mut process = McpStdioProcess::spawn(&transport).expect("spawn transport directly");
            let ready = process.read_available().await.expect("read ready");
            assert_eq!(String::from_utf8_lossy(&ready), "READY:direct-secret\n");
            process.terminate().await.expect("terminate child");
            let _ = process.wait().await.expect("wait after kill");

            cleanup_script(&script_path);
        });
    }

    #[test]
    fn lists_tools_calls_tool_and_reads_resources_over_jsonrpc() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_mcp_server_script();
            let transport = script_transport(&script_path);
            let mut process = McpStdioProcess::spawn(&transport).expect("spawn fake mcp server");

            let tools = process
                .list_tools(JsonRpcId::Number(2), None)
                .await
                .expect("list tools");
            assert_eq!(tools.error, None);
            assert_eq!(tools.id, JsonRpcId::Number(2));
            assert_eq!(
                tools.result,
                Some(McpListToolsResult {
                    tools: vec![McpTool {
                        name: "echo".to_string(),
                        description: Some("Echoes text".to_string()),
                        input_schema: Some(json!({
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"]
                        })),
                        annotations: None,
                        meta: None,
                    }],
                    next_cursor: None,
                })
            );

            let call = process
                .call_tool(
                    JsonRpcId::String("call-1".to_string()),
                    McpToolCallParams {
                        name: "echo".to_string(),
                        arguments: Some(json!({"text": "hello"})),
                        meta: None,
                    },
                )
                .await
                .expect("call tool");
            assert_eq!(call.error, None);
            let call_result = call.result.expect("tool result");
            assert_eq!(call_result.is_error, Some(false));
            assert_eq!(
                call_result.structured_content,
                Some(json!({"echoed": "hello"}))
            );
            assert_eq!(call_result.content.len(), 1);
            assert_eq!(call_result.content[0].kind, "text");
            assert_eq!(
                call_result.content[0].data.get("text"),
                Some(&json!("echo:hello"))
            );

            let resources = process
                .list_resources(JsonRpcId::Number(3), None)
                .await
                .expect("list resources");
            let resources_result = resources.result.expect("resources result");
            assert_eq!(resources_result.resources.len(), 1);
            assert_eq!(resources_result.resources[0].uri, "file://guide.txt");
            assert_eq!(
                resources_result.resources[0].mime_type.as_deref(),
                Some("text/plain")
            );

            let read = process
                .read_resource(
                    JsonRpcId::Number(4),
                    McpReadResourceParams {
                        uri: "file://guide.txt".to_string(),
                    },
                )
                .await
                .expect("read resource");
            assert_eq!(
                read.result,
                Some(McpReadResourceResult {
                    contents: vec![super::McpResourceContents {
                        uri: "file://guide.txt".to_string(),
                        mime_type: Some("text/plain".to_string()),
                        text: Some("contents for file://guide.txt".to_string()),
                        blob: None,
                        meta: None,
                    }],
                })
            );

            process.terminate().await.expect("terminate child");
            let _ = process.wait().await.expect("wait after kill");
            cleanup_script(&script_path);
        });
    }

    #[test]
    fn surfaces_jsonrpc_errors_from_tool_calls() {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let script_path = write_mcp_server_script();
            let transport = script_transport(&script_path);
            let mut process = McpStdioProcess::spawn(&transport).expect("spawn fake mcp server");

            let response = process
                .call_tool(
                    JsonRpcId::Number(9),
                    McpToolCallParams {
                        name: "fail".to_string(),
                        arguments: None,
                        meta: None,
                    },
                )
                .await
                .expect("call tool with error response");

            assert_eq!(response.id, JsonRpcId::Number(9));
            assert!(response.result.is_none());
            assert_eq!(response.error.as_ref().map(|e| e.code), Some(-32001));
            assert_eq!(
                response.error.as_ref().map(|e| e.message.as_str()),
                Some("tool failed")
            );

            process.terminate().await.expect("terminate child");
            let _ = process.wait().await.expect("wait after kill");
            cleanup_script(&script_path);
        });
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

        // HTTP server should now be accepted as a remote server
        assert_eq!(manager.remote_servers.len(), 1);
        assert_eq!(manager.remote_servers[0].name, "http");
    }

    #[test]
    fn manager_rejects_unimplemented_remote_server_features() {
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

        assert_eq!(manager.remote_servers.len(), 0);
        assert_eq!(unsupported.len(), 3);
        assert!(unsupported
            .iter()
            .any(|server| server.server_name == "http-helper"
                && server.reason.contains("headersHelper")));
        assert!(unsupported
            .iter()
            .any(|server| server.server_name == "http-oauth"
                && server.reason.contains("oauth")));
        assert!(unsupported
            .iter()
            .any(|server| server.server_name == "sse"
                && server.reason.contains("Sse transport")));
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

    #[test]
    fn remote_request_ids_are_unique_across_calls() {
        // Regression: remote JSON-RPC ids were hardcoded constants (1, 2, 102).
        // Now they use an atomic counter and must be unique.
        let ids: Vec<JsonRpcId> = (0..10)
            .map(|_| super::next_remote_request_id())
            .collect();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            match id {
                JsonRpcId::Number(n) => {
                    assert!(seen.insert(*n), "duplicate remote request id: {n}");
                }
                other => panic!("expected numeric id, got {other:?}"),
            }
        }
    }
}
