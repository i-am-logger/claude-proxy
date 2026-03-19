# Changelog

## [0.3.0](https://github.com/i-am-logger/claude-code-proxy/compare/claude-code-proxy-v0.2.0...claude-code-proxy-v0.3.0) (2026-03-19)


### ⚠ BREAKING CHANGES

* binary renamed from claude-proxy to claude-code-proxy, crate renamed from claude-proxy to claude-code-proxy.

### Features

* add CLI args with clap, fix --help/--version panic ([ffce150](https://github.com/i-am-logger/claude-code-proxy/commit/ffce1507be3c67538eca300458c12c29c84a53e0))
* OpenAI-compatible proxy for Claude Code CLI ([6bfa9ea](https://github.com/i-am-logger/claude-code-proxy/commit/6bfa9eaad3f0d4b81a3296bc597fa0b78bd14ffb))
* rename to claude-code-proxy ([f5121d8](https://github.com/i-am-logger/claude-code-proxy/commit/f5121d8a31afbf96595f5ad9cdba9b4fb093a083))


### Bug Fixes

* replace async_stream with tokio::spawn for child process lifetime ([72e5e26](https://github.com/i-am-logger/claude-code-proxy/commit/72e5e26ee38986150701b8f474fbd8937c4a3d3d))
* revert to async_stream for SSE streaming ([4ec3ac9](https://github.com/i-am-logger/claude-code-proxy/commit/4ec3ac99578f61ae41f5534058e4ec32fb7849fb))
* streaming SSE — keep child process alive during stream ([a00657b](https://github.com/i-am-logger/claude-code-proxy/commit/a00657bf6f9f69053943667c13f981d44ddda7cd))


### Performance Improvements

* zero-copy stream parsing, response size cap, shared command builder ([d290920](https://github.com/i-am-logger/claude-code-proxy/commit/d2909202d6872daef2e00fbacb8fb2ee08ea35cb))

## [0.2.0](https://github.com/i-am-logger/claude-proxy/compare/claude-proxy-v0.1.0...claude-proxy-v0.2.0) (2026-03-18)


### Features

* add CLI args with clap, fix --help/--version panic ([ffce150](https://github.com/i-am-logger/claude-proxy/commit/ffce1507be3c67538eca300458c12c29c84a53e0))
* OpenAI-compatible proxy for Claude Code CLI ([6bfa9ea](https://github.com/i-am-logger/claude-proxy/commit/6bfa9eaad3f0d4b81a3296bc597fa0b78bd14ffb))
