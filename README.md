# Claw Code

A terminal-based AI coding assistant powered by Claude and OpenAI Codex.

## Features

- Multi-provider: Anthropic Claude and OpenAI Codex
- MCP server integration (stdio, HTTP, SSE)
- OAuth authentication for both providers
- Three permission modes: read-only, workspace-write, danger-full-access
- Vim-style line editing with history
- Session management, compaction, and export
- Tool suite: bash, file ops, web fetch/search, REPL, and more

## Quick Start

```bash
# Build
cd rust && cargo build --release

# Run REPL
./target/release/claw

# Run with OpenAI Codex
./target/release/claw --provider openai

# Login
./target/release/claw login
./target/release/claw --provider openai login

# One-shot prompt
./target/release/claw prompt "explain this codebase"
```

## Configuration

Settings are loaded from (highest priority first):
1. `.claude/settings.local.json` (project-local, gitignored)
2. `.claude/settings.json` (project)
3. `~/.claude/settings.json` (user)

See [docs/config.md](docs/config.md) for details.

## Documentation

- [Authentication](docs/auth.md) -- OAuth login, API keys, credential storage
- [MCP Servers](docs/mcp.md) -- Transport types, configuration, auth
- [Configuration](docs/config.md) -- Settings files, permissions, hooks
- [Testing](docs/testing.md) -- Running tests, live E2E, CI

## Architecture

```
rust/crates/
  api/                -- Anthropic & OpenAI API clients
  runtime/            -- Core runtime: MCP, config, sessions, tools
  commands/           -- Slash command registry
  tools/              -- Tool implementations (bash, file ops, web, REPL)
  rusty-claude-cli/   -- CLI binary (REPL, streaming, permissions)
```

## License

See repository for license details.
