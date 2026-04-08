# Configuration

Claw Code uses a layered settings system with JSON configuration files.

## Settings File Locations

Settings are loaded and merged in priority order (highest first):

1. **`.claude/settings.local.json`** -- Project-local overrides, typically gitignored.
2. **`.claude/settings.json`** -- Project-level settings, committed to the repository.
3. **`~/.claude/settings.json`** -- User-level defaults.

The config home directory (`~/.claude/`) can be overridden with the `CLAUDE_CONFIG_HOME` environment variable.

## Key Sections

### model

Set the default model for API calls:

```json
{
  "model": "claude-opus-4-6"
}
```

Override at runtime with `--model <model>` or the `/model` slash command.

### permissions

Control what the assistant is allowed to do:

```json
{
  "permissions": {
    "defaultMode": "workspace-write",
    "allow": ["bash(git *)"],
    "deny": ["bash(rm -rf *)"]
  }
}
```

### hooks

Register lifecycle hooks that run external commands:

```json
{
  "hooks": {
    "preToolUse": [
      {
        "matcher": "bash",
        "hooks": [
          {
            "type": "command",
            "command": "echo 'about to run bash'"
          }
        ]
      }
    ],
    "postToolUse": [],
    "stop": []
  }
}
```

See the Hooks section below for details.

### mcpServers

Configure MCP server connections. See [mcp.md](mcp.md) for full documentation.

### oauth

Configure OAuth settings for provider login. See [auth.md](auth.md) for full documentation.

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `ANTHROPIC_API_KEY` | Anthropic API key (highest priority credential) |
| `ANTHROPIC_AUTH_TOKEN` | Anthropic bearer token override |
| `ANTHROPIC_BASE_URL` | Override Anthropic API base URL |
| `OPENAI_API_KEY` | OpenAI API key |
| `RUSTY_CLAUDE_PERMISSION_MODE` | Default permission mode |
| `CLAUDE_CONFIG_HOME` | Override config directory (default `~/.claude/`) |

## Permission Modes

Claw Code supports three permission levels that control which tools the assistant can use:

### read-only

The assistant can only read files and run non-destructive commands. No file writes, no shell commands that modify state.

### workspace-write

The assistant can read and write files within the current workspace. Destructive operations outside the workspace are blocked. This is the recommended default for development.

### danger-full-access

The assistant has unrestricted access to all tools including arbitrary shell commands. Use with caution.

Set the mode via:
- Environment variable: `RUSTY_CLAUDE_PERMISSION_MODE=workspace-write`
- Settings file: `"permissions": { "defaultMode": "workspace-write" }`
- CLI flag or slash command: `/permissions workspace-write`

## Hooks

Hooks let you run external commands at specific points in the assistant lifecycle.

### Hook Types

- **preToolUse** -- Runs before a tool is executed. Can be used for validation or logging.
- **postToolUse** -- Runs after a tool completes. Can be used for formatting or checks.
- **stop** -- Runs when the session ends. Can be used for final verification.

### Hook Structure

Each hook entry has:
- `matcher` -- The tool name to match (e.g., `bash`, `edit`, `write`).
- `hooks` -- An array of hook definitions, each with a `type` (currently `command`) and a `command` string to execute.

Hooks are discovered from all settings files in the merge chain. Multiple hooks for the same matcher are executed in order.
