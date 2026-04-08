# Testing

Claw Code uses Rust's built-in test framework across its workspace.

## Running All Tests

```bash
cd rust
cargo test --workspace --exclude compat-harness
```

The `compat-harness` crate is excluded because it compares against an upstream TypeScript codebase that may not be present locally.

## Running Tests for a Specific Crate

```bash
# Runtime tests (config, sessions, MCP, tools)
cargo test -p runtime

# API client tests (Anthropic, OpenAI)
cargo test -p api

# Tool implementation tests
cargo test -p tools

# Command registry tests
cargo test -p commands

# CLI tests
cargo test -p rusty-claude-cli
```

## Test Count

The workspace contains 223+ tests across 6 crates:

| Crate | Area |
|-------|------|
| `api` | API client construction, SSE parsing, request building, OpenAI integration |
| `runtime` | Config loading, session management, MCP transports, prompt rendering, hooks |
| `tools` | Tool execution, argument parsing, permission checks |
| `commands` | Slash command registration and help rendering |
| `rusty-claude-cli` | CLI argument parsing, input handling |
| `compat-harness` | Upstream parity checks (excluded from default test runs) |

## Live Provider Tests

Some integration tests make real API calls and are marked with `#[ignore]` by default. To run them:

```bash
ANTHROPIC_API_KEY=your-key cargo test -p api -- --ignored
```

These tests require a valid API key and network access. They are not included in the standard test run.

## Building

Verify the project compiles without errors:

```bash
cd rust
cargo build --workspace
```

Build the release binary:

```bash
cd rust
cargo build --release -p rusty-claude-cli
```

## Future Plans

- CI pipeline with automated test runs on pull requests
- Nightly live provider tests against both Anthropic and OpenAI
- Coverage reporting integrated into the build pipeline
