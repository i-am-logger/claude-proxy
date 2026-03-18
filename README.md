# claude-proxy [ARCHIVED]

> **WARNING: Using this tool violates [Anthropic's Consumer Terms of Service](https://www.anthropic.com/legal/consumer-terms) as of February 19, 2026.** Anthropic explicitly prohibits using OAuth tokens from Claude Free, Pro, or Max subscriptions in any third-party tool or service. This includes proxying the Claude Code CLI. See [Anthropic's announcement](https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access/) for details. This repository is archived for reference only.

[![License: CC BY-NC-SA 4.0](https://img.shields.io/badge/License-CC%20BY--NC--SA%204.0-lightgrey.svg)](https://creativecommons.org/licenses/by-nc-sa/4.0/)
[![Rust](https://img.shields.io/badge/Rust-2024-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)

> OpenAI-compatible API proxy for [Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code). Uses your authenticated Claude Code (Max subscription) for inference — no API keys needed.

## Why

Claude Code CLI is powerful but only works in the terminal. This proxy exposes it as an OpenAI-compatible HTTP API, enabling:

- **[OpenClaw](https://openclaw.ai)** to use Claude as its LLM backend
- **Cursor**, **Continue**, and other AI coding tools to connect to Claude Code
- Any OpenAI-compatible client to use your Max subscription

## Architecture

```
Client (OpenClaw/Cursor/etc.)
    │
    ▼  POST /v1/chat/completions or /v1/responses
claude-proxy (this binary)
    │
    ▼  claude --print --model opus --output-format stream-json
Claude Code CLI (uses Max subscription)
    │
    ▼
Anthropic API
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Health check |
| `GET` | `/v1/models` | List available models |
| `POST` | `/v1/chat/completions` | Chat Completions API (streaming + non-streaming) |
| `POST` | `/v1/responses` | Responses API (streaming + non-streaming) |

All endpoints also available without the `/v1` prefix.

## Usage

```bash
PROXY_API_KEY=your-secret claude-proxy
```

Then point your client to `http://localhost:8080/v1`.

### Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `PROXY_API_KEY` | Yes | — | Bearer token for proxy authentication |
| `PORT` | No | `8080` | Listen port |
| `CLAUDE_MODEL` | No | `sonnet` | Default model (`haiku`, `sonnet`, `opus`) |
| `RUST_LOG` | No | `claude_proxy=info` | Log level |

### Examples

```bash
# Non-streaming
curl -H "Authorization: Bearer your-secret" \
     -H "Content-Type: application/json" \
     -d '{"model":"opus","messages":[{"role":"user","content":"hello"}]}' \
     http://localhost:8080/v1/chat/completions

# Streaming (SSE)
curl -H "Authorization: Bearer your-secret" \
     -H "Content-Type: application/json" \
     -d '{"model":"opus","messages":[{"role":"user","content":"hello"}],"stream":true}' \
     http://localhost:8080/v1/chat/completions

# Responses API
curl -H "Authorization: Bearer your-secret" \
     -H "Content-Type: application/json" \
     -d '{"model":"opus","input":"hello"}' \
     http://localhost:8080/v1/responses
```

### OpenClaw Configuration

```json
{
  "models": {
    "providers": {
      "claude-proxy": {
        "api": "openai-completions",
        "baseUrl": "http://127.0.0.1:8080/v1",
        "apiKey": "your-secret",
        "models": [
          { "id": "opus", "name": "Claude Opus" }
        ]
      }
    }
  }
}
```

## Features

- Both Chat Completions and Responses API formats
- Full SSE streaming on all endpoints
- Content blocks normalization (handles `[{"type":"text","text":"..."}]` and plain strings)
- Constant-time auth comparison (`subtle` crate)
- 1MB request body limit
- `kill_on_drop` child process management (no zombies)
- Proper UTF-8 validation on CLI output
- Model normalization (`claude-sonnet-4.5` → `sonnet`)

## Development

```bash
# Enter devenv shell
direnv allow

# Build
dev-build

# Run
dev-run

# Test (44 tests including property-based)
dev-test
```

## NixOS

This proxy is designed to run as a systemd user service via [mynixos](https://github.com/i-am-logger/mynixos):

```nix
my.ai.claudeProxy = {
  enable = true;
  model = "opus";
};
```

## License

Creative Commons Attribution-NonCommercial-ShareAlike (CC BY-NC-SA) 4.0 International

See [LICENSE](LICENSE) for details.
