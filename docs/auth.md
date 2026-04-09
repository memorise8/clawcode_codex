# Authentication

Claw Code supports three providers, each with different credential sources.

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

This opens a browser to OpenAI's authorization page, starts a local callback server on port 4546, and exchanges the code for tokens. Tokens are stored in `~/.claude/credentials.json` under the `openai_oauth` key.

### Logout

```bash
claw --provider openai logout
```

### Codex CLI Token Fallback

If you have the Codex CLI installed (`~/.codex/auth.json`), Claw Code will automatically use those tokens as a last resort. This means `codex login` also works for Claw Code.

## Ollama (Local LLMs)

Ollama runs locally and requires **no authentication**. You just need a running Ollama instance.

### Setup

```bash
# Install Ollama (if not installed)
curl -fsSL https://ollama.com/install.sh | sh

# Pull a model
ollama pull gemma4:31b-it-q4_K_M    # 31B dense (20GB, needs 24GB+ VRAM)
ollama pull gemma4:e4b                # 4B edge (9.6GB, runs on most GPUs)
ollama pull gemma4:e2b                # 2B edge (7.2GB, runs on CPU)
```

### Usage

```bash
claw --provider ollama                              # default: gemma4:31b-it-q4_K_M
claw --provider ollama --model gemma4:e4b           # smaller model
claw --provider ollama --model llama3:8b            # any Ollama model works
```

### Custom Ollama URL

By default Claw Code connects to `http://localhost:11434`. To use a remote Ollama server or a different port:

```bash
OLLAMA_BASE_URL=http://gpu-server:11434 claw --provider ollama
OLLAMA_BASE_URL=http://localhost:8080 claw --provider ollama
```

### Available Gemma 4 Models

| Model | Size | Min VRAM | Best for |
|-------|------|----------|----------|
| `gemma4:e2b` | 7.2 GB | 8 GB RAM | Laptops, CPU-only |
| `gemma4:e4b` | 9.6 GB | 16 GB | Consumer GPUs |
| `gemma4:26b` | 18 GB | 24 GB | Workstations (MoE) |
| `gemma4:31b-it-q4_K_M` | 20 GB | 24 GB | Data center GPUs |

### GPU Selection

If you have multiple GPUs, control which one Ollama uses:

```bash
# Start Ollama on a specific GPU
CUDA_VISIBLE_DEVICES=2 ollama serve

# Then connect from Claw Code
claw --provider ollama
```

### Login / Logout

`claw --provider ollama login` and `logout` are no-ops — Ollama does not use authentication.

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
