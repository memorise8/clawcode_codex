# MCP Servers

Claw Code integrates with Model Context Protocol (MCP) servers to extend its tool capabilities.

## Supported Transports

| Transport | Status | Notes |
|-----------|--------|-------|
| stdio     | Full   | Spawns a local process, communicates over stdin/stdout |
| HTTP      | Full   | Connects to remote HTTP endpoints, supports OAuth |
| SSE       | Stub   | Server-Sent Events transport is defined but not yet implemented |

## Configuration

MCP servers are configured in your settings file under the `mcpServers` key. Settings files are loaded from (highest priority first):

1. `.claude/settings.local.json` (project-local)
2. `.claude/settings.json` (project)
3. `~/.claude/settings.json` (user)

### Stdio Server

Spawns a local process and communicates over stdin/stdout:

```json
{
  "mcpServers": {
    "my-server": {
      "command": "node",
      "args": ["server.js"]
    }
  }
}
```

### HTTP Server

Connects to a remote endpoint:

```json
{
  "mcpServers": {
    "remote": {
      "url": "https://mcp.example.com/v1",
      "headers": {
        "X-Api-Key": "your-api-key"
      }
    }
  }
}
```

## OAuth for Remote Servers

Remote MCP servers can use OAuth for authentication. Add an `oauth` block to the server configuration:

```json
{
  "mcpServers": {
    "remote": {
      "url": "https://mcp.example.com/v1",
      "oauth": {
        "clientId": "your-client-id",
        "authServerMetadataUrl": "https://auth.example.com/.well-known/openid-configuration"
      }
    }
  }
}
```

When configured, Claw Code runs the OAuth flow on first connection and stores tokens in `~/.claude/credentials.json` under the key `mcp_oauth::<server_name>`. See [auth.md](auth.md) for details on credential storage.

## Headers Helper

The `headersHelper` field is accepted in server configuration but currently logs a warning and is not executed. This field is reserved for future use where an external command would be run to dynamically generate request headers.

```json
{
  "mcpServers": {
    "remote": {
      "url": "https://mcp.example.com/v1",
      "headersHelper": ["node", "get-headers.js"]
    }
  }
}
```

## Tool Discovery

When a session starts, Claw Code connects to all configured MCP servers and calls `tools/list` on each one. Discovered tools are merged into the active tool set and made available to the model alongside built-in tools.

Each MCP tool is namespaced by its server name. For example, a tool called `search` from a server named `remote` appears as `mcp__remote__search` in the tool list.

## Module Structure

The MCP implementation lives in `rust/crates/runtime/src/` and is split across 8 files:

| File | Purpose |
|------|---------|
| `mcp_stdio.rs` | Stdio transport: process spawning, JSON-RPC over stdin/stdout |
| `mcp_http.rs` | HTTP transport: request/response over HTTP with optional auth |
| `mcp_sse.rs` | SSE transport stub |
| `mcp_oauth.rs` | OAuth flow for remote MCP servers |
| `mcp_router.rs` | Dispatches tool calls to the correct server/transport |
| `mcp_types.rs` | Shared MCP protocol types and JSON-RPC message definitions |
| `mcp_config.rs` | Parses MCP server configuration from settings files |
| `mcp_registry.rs` | Manages connected servers and their discovered tools |
