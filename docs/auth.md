# Authentication

Claw Code supports two providers, each with multiple credential sources.

## Anthropic (Claude)

Credentials are resolved in this order:

1. **`ANTHROPIC_API_KEY`** environment variable -- highest priority, used directly as the API key.
2. **`ANTHROPIC_AUTH_TOKEN`** environment variable -- bearer-token override when no API key is set.
3. **OAuth credentials** stored in `~/.claude/credentials.json` -- used when neither env var is present.

### OAuth Login

```bash
claw login
```

This opens a browser window, starts a local callback server, exchanges the authorization code for tokens, and stores them in `~/.claude/credentials.json`.

OAuth settings are configured in your settings file under the `oauth` block:

```json
{
  "oauth": {
    "clientId": "your-client-id",
    "authorizeUrl": "https://auth.example.com/authorize",
    "tokenUrl": "https://auth.example.com/token",
    "callbackPort": 7773,
    "scopes": ["openid", "offline_access"]
  }
}
```

### Logout

```bash
claw logout
```

Removes stored OAuth credentials from `credentials.json` while preserving other fields.

## OpenAI (Codex)

Credentials are resolved in this order:

1. **`OPENAI_API_KEY`** environment variable -- used directly as the API key.
2. **OAuth login** -- stores tokens in `~/.claude/credentials.json` under the `openai` key.
3. **Codex CLI token fallback** -- if a Codex CLI token exists locally, it is used as a last resort.

### OAuth Login

```bash
claw --provider openai login
```

Uses the same browser-based OAuth flow as the Anthropic provider, but targets OpenAI's authorization endpoints.

## Credential Storage

All OAuth tokens are stored in:

```
~/.claude/credentials.json
```

This path can be overridden with the `CLAUDE_CONFIG_HOME` environment variable, in which case credentials are stored at `$CLAUDE_CONFIG_HOME/credentials.json`.

The file contains separate entries for each provider. Example structure:

```json
{
  "claudeAiOauth": {
    "accessToken": "...",
    "refreshToken": "...",
    "expiresAt": "..."
  },
  "openai": {
    "accessToken": "...",
    "refreshToken": "...",
    "expiresAt": "..."
  }
}
```

## Token Refresh

Both providers support automatic token refresh. When an access token expires and a refresh token is available, Claw Code transparently requests a new access token before making the API call. No manual intervention is required.

## MCP Server OAuth

Remote MCP servers that require OAuth store their credentials in the same `credentials.json` file under the key `mcp_oauth::<server_name>`. For example, a server named `remote` would store tokens at:

```json
{
  "mcp_oauth::remote": {
    "accessToken": "...",
    "refreshToken": "...",
    "expiresAt": "..."
  }
}
```

MCP OAuth configuration is specified per-server in the `mcpServers` settings block. See [mcp.md](mcp.md) for details.
