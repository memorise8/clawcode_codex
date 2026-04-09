# Claw Code

A terminal-based AI coding assistant powered by Claude and OpenAI Codex.

## Features

- Multi-provider: Anthropic Claude, OpenAI Codex, and Ollama (local LLMs)
- MCP server integration (stdio, HTTP, SSE)
- OAuth authentication for both providers
- Three permission modes: read-only, workspace-write, danger-full-access
- Vim-style line editing with history
- Session management, compaction, and export
- Tool suite: bash, file ops, web fetch/search, REPL, and more

## Installation

### Prerequisites

- **Rust toolchain** (1.75+): [rustup.rs](https://rustup.rs/)
- **Git**: for repository operations
- At least one provider:
  - **Anthropic**: `ANTHROPIC_API_KEY` env var or OAuth login
  - **OpenAI Codex**: `OPENAI_API_KEY` env var or OAuth login
  - **Ollama**: local install, no API key needed

### Build from source

```bash
git clone https://github.com/memorise8/clawcode_codex.git
cd clawcode_codex/rust
cargo build --release
```

The binary is at `./target/release/claw`. Optionally add it to your PATH:

```bash
# Linux / macOS
cp target/release/claw ~/.local/bin/
# or
sudo cp target/release/claw /usr/local/bin/
```

### Provider setup

```bash
# Option 1: Anthropic Claude (API key)
export ANTHROPIC_API_KEY="sk-ant-..."
claw

# Option 2: OpenAI Codex (API key)
export OPENAI_API_KEY="sk-..."
claw --provider openai

# Option 3: OpenAI Codex (OAuth login)
claw --provider openai login

# Option 4: Ollama (local, no key needed)
ollama pull gemma4:e4b           # download a model first
claw --provider ollama
```

See [docs/auth.md](docs/auth.md) for full authentication details.

## Quick Start

```bash
# Interactive REPL (default: Anthropic Claude)
claw

# Switch provider
claw --provider openai
claw --provider ollama
claw --provider ollama --model gemma4:e4b

# One-shot prompt
claw prompt "explain this codebase"

# JSON output
claw --output-format json prompt "summarize README.md"

# Custom permission mode
claw --permission-mode danger-full-access
```

## Configuration

Settings are loaded from (highest priority first):
1. `.claude/settings.local.json` (project-local, gitignored)
2. `.claude/settings.json` (project)
3. `~/.claude/settings.json` (user)

See [docs/config.md](docs/config.md) for details.

## Documentation

- [Authentication](docs/auth.md) -- OAuth login, API keys, Ollama setup, credential storage
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
