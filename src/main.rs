use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{convert::Infallible, process::Stdio, sync::Arc};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_stream::wrappers::LinesStream;
use tracing::info;

/// OpenAI-compatible API proxy for Claude Code CLI
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Bearer token for proxy authentication
    #[arg(long, env = "PROXY_API_KEY")]
    api_key: String,

    /// Listen port
    #[arg(short, long, env = "PORT", default_value = "8080")]
    port: u16,

    /// Default Claude model (haiku/sonnet/opus)
    #[arg(short, long, env = "CLAUDE_MODEL", default_value = "sonnet")]
    model: String,
}

#[derive(Clone)]
struct AppState {
    api_key: String,
    default_model: String,
}

// --- Request types ---

#[derive(Deserialize)]
struct ChatRequest {
    model: Option<String>,
    messages: Vec<ChatMessage>,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(default, deserialize_with = "deserialize_content")]
    content: String,
}

/// Handles both string content and OpenAI content blocks array
fn deserialize_content<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(s) => Ok(s),
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            for block in blocks {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    texts.push(text.to_string());
                }
            }
            Ok(texts.join("\n"))
        }
        Value::Null => Ok(String::new()),
        _ => Ok(value.to_string()),
    }
}

#[derive(Deserialize)]
struct ResponsesRequest {
    model: Option<String>,
    input: Value,
    #[serde(default)]
    stream: bool,
}

// --- Response types ---

#[derive(Serialize)]
struct ChatResponse {
    id: String,
    object: &'static str,
    created: u64,
    model: String,
    choices: Vec<Choice>,
    usage: Usage,
}

#[derive(Serialize)]
struct Choice {
    index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<ChoiceMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<Delta>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_reason: Option<String>,
}

#[derive(Serialize)]
struct ChoiceMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct Delta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Serialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Serialize)]
struct ResponsesResponse {
    id: String,
    object: &'static str,
    created_at: u64,
    model: String,
    output: Vec<ResponsesOutput>,
    usage: ResponsesUsage,
}

#[derive(Serialize)]
struct ResponsesOutput {
    r#type: &'static str,
    id: String,
    role: &'static str,
    content: Vec<ResponsesContent>,
}

#[derive(Serialize)]
struct ResponsesContent {
    r#type: &'static str,
    text: String,
}

#[derive(Serialize)]
struct ResponsesUsage {
    input_tokens: u32,
    output_tokens: u32,
    total_tokens: u32,
}

#[derive(Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelData>,
}

#[derive(Serialize)]
struct ModelData {
    id: String,
    object: &'static str,
    owned_by: &'static str,
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    message: String,
    r#type: String,
}

// --- Auth ---

fn check_auth(headers: &HeaderMap, api_key: &str) -> Result<(), (StatusCode, Json<ErrorBody>)> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !auth.starts_with("Bearer ")
        || auth.as_bytes()[7..].ct_eq(api_key.as_bytes()).unwrap_u8() != 1
    {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                error: ErrorDetail {
                    message: "Invalid API key".into(),
                    r#type: "auth_error".into(),
                },
            }),
        ));
    }
    Ok(())
}

// --- Claude CLI ---

fn normalize_model(m: &str) -> String {
    let m = m.to_lowercase();
    let m = m
        .trim_start_matches("claude-")
        .trim_start_matches("claude_");
    for base in &["haiku", "sonnet", "opus"] {
        if m.starts_with(base) {
            return base.to_string();
        }
    }
    if m.is_empty() {
        "sonnet".into()
    } else {
        m.into()
    }
}

struct ParsedMessages {
    system_prompt: String,
    user_prompt: String,
}

fn parse_chat_messages(messages: &[ChatMessage]) -> ParsedMessages {
    let mut system_prompt = String::new();
    let mut user_prompt = String::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                if !system_prompt.is_empty() {
                    system_prompt.push_str("\n\n");
                }
                system_prompt.push_str(&msg.content);
            }
            "user" => {
                user_prompt.push_str(&msg.content);
                user_prompt.push('\n');
            }
            "assistant" => {
                user_prompt.push_str("[Previous response: ");
                user_prompt.push_str(&msg.content);
                user_prompt.push_str("]\n");
            }
            _ => {}
        }
    }

    ParsedMessages {
        system_prompt,
        user_prompt,
    }
}

fn parse_responses_input(input: &Value) -> ParsedMessages {
    let mut system_prompt = String::new();
    let mut user_prompt = String::new();

    match input {
        Value::String(s) => {
            user_prompt = s.clone();
        }
        Value::Array(items) => {
            for item in items {
                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                let content = match item.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(blocks)) => blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => continue,
                };

                match role {
                    "system" => {
                        if !system_prompt.is_empty() {
                            system_prompt.push_str("\n\n");
                        }
                        system_prompt.push_str(&content);
                    }
                    "user" => {
                        user_prompt.push_str(&content);
                        user_prompt.push('\n');
                    }
                    "assistant" => {
                        user_prompt.push_str("[Previous response: ");
                        user_prompt.push_str(&content);
                        user_prompt.push_str("]\n");
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    ParsedMessages {
        system_prompt,
        user_prompt,
    }
}

async fn call_claude(
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String, String> {
    let mut args = vec![
        "--print".to_string(),
        "--model".to_string(),
        model.to_string(),
    ];

    if !system_prompt.is_empty() {
        args.push("--system-prompt".into());
        args.push(system_prompt.to_string());
    }

    info!(
        model,
        system_len = system_prompt.len(),
        user_len = user_prompt.len(),
        "calling claude"
    );

    let mut child = Command::new("claude")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(user_prompt.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to claude stdin: {e}"))?;
        drop(stdin);
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("Claude process error: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Claude exited with {}: {stderr}", output.status));
    }

    String::from_utf8(output.stdout)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("Claude output is not valid UTF-8: {e}"))
}

async fn call_claude_streaming(
    model: &str,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<tokio::process::Child, String> {
    let mut args = vec![
        "--print".to_string(),
        "--model".to_string(),
        model.to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];

    if !system_prompt.is_empty() {
        args.push("--system-prompt".into());
        args.push(system_prompt.to_string());
    }

    info!(model, "starting claude streaming");

    let mut child = Command::new("claude")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(user_prompt.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to claude stdin: {e}"))?;
        drop(stdin);
    }

    Ok(child)
}

// --- Stream parsing ---

/// Extract text content from a Claude stream-json line.
/// Returns Some(text) for lines containing assistant text or result text,
/// None for lines that should be skipped (system, rate_limit, empty, etc.)
/// Returns (text, is_from_assistant) — callers use is_from_assistant to avoid
/// emitting the result fallback when assistant content was already sent.
fn extract_stream_text(line: &str) -> Option<(String, bool)> {
    if line.is_empty() {
        return None;
    }
    let msg: Value = serde_json::from_str(line).ok()?;
    let msg_type = msg.get("type").and_then(|t| t.as_str())?;

    if msg_type == "assistant"
        && let Some(blocks) = msg.pointer("/message/content").and_then(|c| c.as_array())
    {
        let texts: Vec<&str> = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .filter(|t| !t.is_empty())
            .collect();
        if !texts.is_empty() {
            return Some((texts.join(""), true));
        }
    }

    if msg_type == "result"
        && let Some(result) = msg.get("result").and_then(|r| r.as_str())
        && !result.is_empty()
    {
        return Some((result.to_string(), false));
    }

    None
}

// --- Handlers ---

async fn health() -> &'static str {
    "ok"
}

async fn models(State(state): State<Arc<AppState>>) -> Json<ModelsResponse> {
    Json(ModelsResponse {
        object: "list",
        data: vec![
            ModelData {
                id: state.default_model.clone(),
                object: "model",
                owned_by: "anthropic",
            },
            ModelData {
                id: "haiku".into(),
                object: "model",
                owned_by: "anthropic",
            },
            ModelData {
                id: "sonnet".into(),
                object: "model",
                owned_by: "anthropic",
            },
            ModelData {
                id: "opus".into(),
                object: "model",
                owned_by: "anthropic",
            },
        ],
    })
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    check_auth(&headers, &state.api_key)?;

    let model = normalize_model(req.model.as_deref().unwrap_or(&state.default_model));
    let parsed = parse_chat_messages(&req.messages);

    info!(
        model,
        messages = req.messages.len(),
        stream = req.stream,
        "chat completions"
    );

    if req.stream {
        let mut child = call_claude_streaming(&model, &parsed.system_prompt, &parsed.user_prompt)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorBody {
                        error: ErrorDetail {
                            message: e,
                            r#type: "server_error".into(),
                        },
                    }),
                )
            })?;

        // Take stdout BEFORE creating the stream — child stays alive via _child binding
        let stdout = child.stdout.take().ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: ErrorDetail {
                        message: "Failed to capture claude stdout".into(),
                        r#type: "server_error".into(),
                    },
                }),
            )
        })?;
        let _child = child; // keep child alive for the stream's lifetime

        let reader = BufReader::new(stdout);
        let lines = LinesStream::new(reader.lines());
        let chat_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let stream = async_stream::stream! {
            use tokio_stream::StreamExt;
            // _child is captured by reference, keeping the process alive
            let _ = &_child;
            let mut lines = lines;
            let mut sent_role = false;

            while let Some(Ok(line)) = lines.next().await {
                let line: String = line;
                if let Some((text, is_assistant)) = extract_stream_text(&line) {
                    // Skip result fallback if assistant content was already sent
                    if !is_assistant && sent_role { continue; }
                    if !sent_role {
                        let chunk = serde_json::json!({
                            "id": &chat_id, "object": "chat.completion.chunk",
                            "created": created, "model": &model,
                            "choices": [{"index": 0, "delta": {"role": "assistant"}}]
                        });
                        yield Ok::<_, Infallible>(Event::default().data(chunk.to_string()));
                        sent_role = true;
                    }
                    let chunk = serde_json::json!({
                        "id": &chat_id, "object": "chat.completion.chunk",
                        "created": created, "model": &model,
                        "choices": [{"index": 0, "delta": {"content": text}}]
                    });
                    yield Ok(Event::default().data(chunk.to_string()));
                }
            }

            // Final chunk
            let final_chunk = serde_json::json!({
                "id": &chat_id, "object": "chat.completion.chunk",
                "created": created, "model": &model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            });
            yield Ok(Event::default().data(final_chunk.to_string()));
            yield Ok(Event::default().data("[DONE]"));
        };

        Ok(Sse::new(stream).into_response())
    } else {
        let response = call_claude(&model, &parsed.system_prompt, &parsed.user_prompt)
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorBody {
                        error: ErrorDetail {
                            message: e,
                            r#type: "server_error".into(),
                        },
                    }),
                )
            })?;

        let prompt_len = (parsed.system_prompt.len() + parsed.user_prompt.len()) as u32 / 4;
        let completion_len = response.len() as u32 / 4;

        Ok(Json(ChatResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion",
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            model,
            choices: vec![Choice {
                index: 0,
                message: Some(ChoiceMessage {
                    role: "assistant".into(),
                    content: response,
                }),
                delta: None,
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: prompt_len,
                completion_tokens: completion_len,
                total_tokens: prompt_len + completion_len,
            },
        })
        .into_response())
    }
}

async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ResponsesRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorBody>)> {
    check_auth(&headers, &state.api_key)?;

    let model = normalize_model(req.model.as_deref().unwrap_or(&state.default_model));
    let parsed = parse_responses_input(&req.input);

    info!(model, stream = req.stream, "responses");

    let make_error = |e: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                error: ErrorDetail {
                    message: e,
                    r#type: "server_error".into(),
                },
            }),
        )
    };

    if req.stream {
        let mut child = call_claude_streaming(&model, &parsed.system_prompt, &parsed.user_prompt)
            .await
            .map_err(make_error)?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| make_error("Failed to capture claude stdout".into()))?;
        let _child = child;

        let reader = BufReader::new(stdout);
        let lines = LinesStream::new(reader.lines());
        let resp_id = format!("resp_{}", uuid::Uuid::new_v4());
        let item_id = format!("msg_{}", uuid::Uuid::new_v4());

        let stream = async_stream::stream! {
            use tokio_stream::StreamExt;
            let _ = &_child;
            let mut lines = lines;

            // response.created
            yield Ok::<_, Infallible>(Event::default()
                .event("response.created")
                .data(serde_json::json!({"type":"response.created","response":{"id":&resp_id,"object":"response","status":"in_progress"}}).to_string()));

            // response.output_item.added
            yield Ok(Event::default()
                .event("response.output_item.added")
                .data(serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":&item_id,"role":"assistant"}}).to_string()));

            // response.content_part.added
            yield Ok(Event::default()
                .event("response.content_part.added")
                .data(serde_json::json!({"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}).to_string()));

            let mut sent_content = false;
            while let Some(Ok(line)) = lines.next().await {
                let line: String = line;
                if let Some((text, is_assistant)) = extract_stream_text(&line) {
                    if !is_assistant && sent_content { continue; }
                    sent_content = true;
                    yield Ok(Event::default()
                        .event("response.output_text.delta")
                        .data(serde_json::json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":text}).to_string()));
                }
            }

            // response.output_text.done
            yield Ok(Event::default()
                .event("response.output_text.done")
                .data(serde_json::json!({"type":"response.output_text.done","output_index":0,"content_index":0}).to_string()));

            // response.output_item.done
            yield Ok(Event::default()
                .event("response.output_item.done")
                .data(serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":&item_id,"role":"assistant"}}).to_string()));

            // response.completed
            yield Ok(Event::default()
                .event("response.completed")
                .data(serde_json::json!({"type":"response.completed","response":{"id":&resp_id,"object":"response","status":"completed"}}).to_string()));
        };

        Ok(Sse::new(stream).into_response())
    } else {
        let response = call_claude(&model, &parsed.system_prompt, &parsed.user_prompt)
            .await
            .map_err(make_error)?;

        let input_len = (parsed.system_prompt.len() + parsed.user_prompt.len()) as u32 / 4;
        let output_len = response.len() as u32 / 4;
        let output_id = format!("msg_{}", uuid::Uuid::new_v4());

        Ok(Json(ResponsesResponse {
            id: format!("resp_{}", uuid::Uuid::new_v4()),
            object: "response",
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            model,
            output: vec![ResponsesOutput {
                r#type: "message",
                id: output_id,
                role: "assistant",
                content: vec![ResponsesContent {
                    r#type: "output_text",
                    text: response,
                }],
            }],
            usage: ResponsesUsage {
                input_tokens: input_len,
                output_tokens: output_len,
                total_tokens: input_len + output_len,
            },
        })
        .into_response())
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "claude_code_proxy=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let api_key = cli.api_key;
    let default_model = normalize_model(&cli.model);
    let port = cli.port;

    let state = Arc::new(AppState {
        api_key,
        default_model: default_model.clone(),
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/chat/completions", post(chat_completions))
        .route("/v1/responses", post(responses))
        .route("/responses", post(responses))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)) // 1MB max request body
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("Failed to bind");

    info!(port, model = default_model, "claude-code-proxy starting");
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use proptest::prelude::*;
    use tower::ServiceExt;

    fn test_app() -> Router {
        let state = Arc::new(AppState {
            api_key: "test-key".into(),
            default_model: "sonnet".into(),
        });

        Router::new()
            .route("/health", get(health))
            .route("/v1/models", get(models.clone()))
            .route("/models", get(models))
            .route("/v1/chat/completions", post(chat_completions.clone()))
            .route("/chat/completions", post(chat_completions))
            .route("/v1/responses", post(responses.clone()))
            .route("/responses", post(responses))
            .with_state(state)
    }

    async fn send_request(
        app: Router,
        method: &str,
        uri: &str,
        body: &str,
        auth: bool,
    ) -> axum::http::Response<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if !body.is_empty() {
            builder = builder.header("content-type", "application/json");
        }
        if auth {
            builder = builder.header(header::AUTHORIZATION, "Bearer test-key");
        }
        let body = if body.is_empty() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        };
        app.oneshot(builder.body(body).unwrap()).await.unwrap()
    }

    async fn response_json(resp: axum::http::Response<Body>) -> Value {
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    // ========== normalize_model ==========

    #[test]
    fn normalize_known_models() {
        for model in &["haiku", "sonnet", "opus"] {
            assert_eq!(normalize_model(model), *model);
        }
    }

    #[test]
    fn normalize_strips_claude_prefix() {
        assert_eq!(normalize_model("claude-sonnet"), "sonnet");
        assert_eq!(normalize_model("claude_opus"), "opus");
        assert_eq!(normalize_model("claude-haiku-4-5"), "haiku");
    }

    #[test]
    fn normalize_case_insensitive() {
        assert_eq!(normalize_model("SONNET"), "sonnet");
        assert_eq!(normalize_model("Claude-Opus"), "opus");
        assert_eq!(normalize_model("HAIKU"), "haiku");
    }

    #[test]
    fn normalize_empty_defaults_to_sonnet() {
        assert_eq!(normalize_model(""), "sonnet");
    }

    #[test]
    fn normalize_unknown_passthrough() {
        assert_eq!(normalize_model("gpt-4o"), "gpt-4o");
        assert_eq!(normalize_model("llama3"), "llama3");
    }

    proptest! {
        #[test]
        fn normalize_never_panics(s in ".*") {
            let _ = normalize_model(&s);
        }

        #[test]
        fn normalize_always_lowercase(s in "[a-zA-Z-_]{1,50}") {
            let result = normalize_model(&s);
            assert_eq!(result, result.to_lowercase());
        }

        #[test]
        fn normalize_known_prefix_resolves(base in prop_oneof!["haiku", "sonnet", "opus"]) {
            let with_prefix = format!("claude-{base}");
            assert_eq!(normalize_model(&with_prefix), base);
            let with_underscore = format!("claude_{base}");
            assert_eq!(normalize_model(&with_underscore), base);
        }
    }

    // ========== deserialize_content ==========

    #[test]
    fn content_string() {
        let msg: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        assert_eq!(msg.content, "hello");
    }

    #[test]
    fn content_blocks_array() {
        let msg: ChatMessage = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}"#
        ).unwrap();
        assert_eq!(msg.content, "hello\nworld");
    }

    #[test]
    fn content_blocks_with_non_text_types() {
        let msg: ChatMessage = serde_json::from_str(
            r#"{"role":"user","content":[{"type":"image","url":"x"},{"type":"text","text":"hello"}]}"#
        ).unwrap();
        assert_eq!(msg.content, "hello");
    }

    #[test]
    fn content_null() {
        let msg: ChatMessage = serde_json::from_str(r#"{"role":"user","content":null}"#).unwrap();
        assert_eq!(msg.content, "");
    }

    #[test]
    fn content_missing() {
        let msg: ChatMessage = serde_json::from_str(r#"{"role":"user"}"#).unwrap();
        assert_eq!(msg.content, "");
    }

    #[test]
    fn content_empty_blocks() {
        let msg: ChatMessage = serde_json::from_str(r#"{"role":"user","content":[]}"#).unwrap();
        assert_eq!(msg.content, "");
    }

    proptest! {
        #[test]
        fn content_string_roundtrips(s in "[a-zA-Z0-9 ]{0,100}") {
            let json = serde_json::json!({"role": "user", "content": s});
            let msg: ChatMessage = serde_json::from_value(json).unwrap();
            assert_eq!(msg.content, s);
        }

        #[test]
        fn content_blocks_extracts_all_text(texts in prop::collection::vec("[a-zA-Z0-9 ]{1,50}", 1..5)) {
            let blocks: Vec<Value> = texts.iter()
                .map(|t| serde_json::json!({"type": "text", "text": t}))
                .collect();
            let json = serde_json::json!({"role": "user", "content": blocks});
            let msg: ChatMessage = serde_json::from_value(json).unwrap();
            assert_eq!(msg.content, texts.join("\n"));
        }
    }

    // ========== parse_chat_messages ==========

    #[test]
    fn parse_user_only() {
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: "hello".into(),
        }];
        let p = parse_chat_messages(&msgs);
        assert_eq!(p.system_prompt, "");
        assert_eq!(p.user_prompt, "hello\n");
    }

    #[test]
    fn parse_system_and_user() {
        let msgs = vec![
            ChatMessage {
                role: "system".into(),
                content: "be helpful".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            },
        ];
        let p = parse_chat_messages(&msgs);
        assert_eq!(p.system_prompt, "be helpful");
        assert_eq!(p.user_prompt, "hello\n");
    }

    #[test]
    fn parse_conversation_with_assistant() {
        let msgs = vec![
            ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "hi".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "bye".into(),
            },
        ];
        let p = parse_chat_messages(&msgs);
        assert_eq!(p.user_prompt, "hello\n[Previous response: hi]\nbye\n");
    }

    #[test]
    fn parse_multiple_system_messages_joined() {
        let msgs = vec![
            ChatMessage {
                role: "system".into(),
                content: "rule 1".into(),
            },
            ChatMessage {
                role: "system".into(),
                content: "rule 2".into(),
            },
        ];
        let p = parse_chat_messages(&msgs);
        assert_eq!(p.system_prompt, "rule 1\n\nrule 2");
    }

    #[test]
    fn parse_unknown_role_ignored() {
        let msgs = vec![
            ChatMessage {
                role: "tool".into(),
                content: "result".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "hello".into(),
            },
        ];
        let p = parse_chat_messages(&msgs);
        assert_eq!(p.user_prompt, "hello\n");
    }

    #[test]
    fn parse_empty_messages() {
        let p = parse_chat_messages(&[]);
        assert_eq!(p.system_prompt, "");
        assert_eq!(p.user_prompt, "");
    }

    proptest! {
        #[test]
        fn parse_system_content_preserved(content in "[a-zA-Z0-9 ]{1,100}") {
            let msgs = vec![ChatMessage { role: "system".into(), content: content.clone() }];
            let p = parse_chat_messages(&msgs);
            assert_eq!(p.system_prompt, content);
        }

        #[test]
        fn parse_user_content_preserved(content in "[a-zA-Z0-9 ]{1,100}") {
            let msgs = vec![ChatMessage { role: "user".into(), content: content.clone() }];
            let p = parse_chat_messages(&msgs);
            assert!(p.user_prompt.contains(&content));
        }
    }

    // ========== parse_responses_input ==========

    #[test]
    fn responses_input_string() {
        let p = parse_responses_input(&Value::String("hello".into()));
        assert_eq!(p.user_prompt, "hello");
        assert_eq!(p.system_prompt, "");
    }

    #[test]
    fn responses_input_messages_array() {
        let input = serde_json::json!([
            {"role": "system", "content": "be helpful"},
            {"role": "user", "content": "hello"}
        ]);
        let p = parse_responses_input(&input);
        assert_eq!(p.system_prompt, "be helpful");
        assert_eq!(p.user_prompt, "hello\n");
    }

    #[test]
    fn responses_input_content_blocks() {
        let input = serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "hello"}]}
        ]);
        let p = parse_responses_input(&input);
        assert_eq!(p.user_prompt, "hello\n");
    }

    #[test]
    fn responses_input_null() {
        let p = parse_responses_input(&Value::Null);
        assert_eq!(p.system_prompt, "");
        assert_eq!(p.user_prompt, "");
    }

    proptest! {
        #[test]
        fn responses_string_input_preserved(s in "[a-zA-Z0-9 ]{1,100}") {
            let p = parse_responses_input(&Value::String(s.clone()));
            assert_eq!(p.user_prompt, s);
        }
    }

    // ========== check_auth ==========

    #[test]
    fn auth_valid_key() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer test-key".parse().unwrap());
        assert!(check_auth(&h, "test-key").is_ok());
    }

    #[test]
    fn auth_wrong_key() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer wrong".parse().unwrap());
        assert!(check_auth(&h, "test-key").is_err());
    }

    #[test]
    fn auth_missing() {
        assert!(check_auth(&HeaderMap::new(), "test-key").is_err());
    }

    #[test]
    fn auth_no_bearer_prefix() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "Basic test-key".parse().unwrap());
        assert!(check_auth(&h, "test-key").is_err());
    }

    proptest! {
        #[test]
        fn auth_correct_key_always_passes(key in "[a-zA-Z0-9]{1,50}") {
            let mut h = HeaderMap::new();
            h.insert("authorization", format!("Bearer {key}").parse().unwrap());
            assert!(check_auth(&h, &key).is_ok());
        }

        #[test]
        fn auth_wrong_key_always_fails(key in "[a-zA-Z0-9]{1,50}", wrong in "[a-zA-Z0-9]{1,50}") {
            prop_assume!(key != wrong);
            let mut h = HeaderMap::new();
            h.insert("authorization", format!("Bearer {wrong}").parse().unwrap());
            assert!(check_auth(&h, &key).is_err());
        }
    }

    // ========== HTTP endpoint tests ==========

    #[tokio::test]
    async fn health_returns_ok() {
        let resp = send_request(test_app(), "GET", "/health", "", false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn models_returns_list() {
        let json =
            response_json(send_request(test_app(), "GET", "/v1/models", "", false).await).await;
        assert_eq!(json["object"], "list");
        let models = json["data"].as_array().unwrap();
        assert!(models.len() >= 3);
        let ids: Vec<&str> = models.iter().filter_map(|m| m["id"].as_str()).collect();
        assert!(ids.contains(&"haiku"));
        assert!(ids.contains(&"sonnet"));
        assert!(ids.contains(&"opus"));
    }

    #[tokio::test]
    async fn models_without_v1_prefix() {
        let resp = send_request(test_app(), "GET", "/models", "", false).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn chat_completions_rejects_no_auth() {
        let resp = send_request(
            test_app(),
            "POST",
            "/v1/chat/completions",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn responses_rejects_no_auth() {
        let resp = send_request(
            test_app(),
            "POST",
            "/v1/responses",
            r#"{"input":"hi"}"#,
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_completions_rejects_bad_json() {
        let resp = send_request(test_app(), "POST", "/v1/chat/completions", "not json", true).await;
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn responses_rejects_bad_json() {
        let resp = send_request(test_app(), "POST", "/v1/responses", "not json", true).await;
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn chat_completions_without_v1_rejects_no_auth() {
        let resp = send_request(
            test_app(),
            "POST",
            "/chat/completions",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            false,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn responses_without_v1_rejects_no_auth() {
        let resp = send_request(test_app(), "POST", "/responses", r#"{"input":"hi"}"#, false).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // ========== extract_stream_text (Claude stream-json parsing) ==========

    #[test]
    fn stream_extracts_assistant_text() {
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_123","type":"message","role":"assistant","content":[{"type":"text","text":"Hi! How can I help you today?"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}},"session_id":"abc"}"#;
        let result = extract_stream_text(line);
        assert_eq!(result, Some(("Hi! How can I help you today?".into(), true)));
    }

    #[test]
    fn stream_extracts_result_text() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1877,"result":"Hi! How can I help you today?","stop_reason":"end_turn","session_id":"abc"}"#;
        let result = extract_stream_text(line);
        assert_eq!(
            result,
            Some(("Hi! How can I help you today?".into(), false))
        );
    }

    #[test]
    fn stream_skips_system_init() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/home/user","session_id":"abc","tools":["Bash"]}"#;
        assert_eq!(extract_stream_text(line), None);
    }

    #[test]
    fn stream_skips_rate_limit() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"},"session_id":"abc"}"#;
        assert_eq!(extract_stream_text(line), None);
    }

    #[test]
    fn stream_skips_empty_line() {
        assert_eq!(extract_stream_text(""), None);
    }

    #[test]
    fn stream_skips_invalid_json() {
        assert_eq!(extract_stream_text("not json"), None);
    }

    #[test]
    fn stream_extracts_multiblock_content() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello "},{"type":"text","text":"world!"}]}}"#;
        let result = extract_stream_text(line);
        assert_eq!(result, Some(("Hello world!".into(), true)));
    }

    #[test]
    fn stream_skips_empty_result() {
        let line = r#"{"type":"result","result":"","session_id":"abc"}"#;
        assert_eq!(extract_stream_text(line), None);
    }

    #[test]
    fn stream_skips_assistant_without_content() {
        let line = r#"{"type":"assistant","message":{}}"#;
        assert_eq!(extract_stream_text(line), None);
    }

    #[test]
    fn stream_real_claude_output() {
        // Real output from: claude --print --model opus --output-format stream-json --verbose "say hi"
        let lines = vec![
            r#"{"type":"system","subtype":"init","cwd":"/home/user","session_id":"dfe46515","tools":["Bash"],"model":"claude-opus-4-6","permissionMode":"default"}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-4-6","id":"msg_017yp","type":"message","role":"assistant","content":[{"type":"text","text":"Hi! How can I help you today?"}],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":3,"output_tokens":1}},"session_id":"dfe46515"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1773878400},"session_id":"dfe46515"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1877,"result":"Hi! How can I help you today?","stop_reason":"end_turn","session_id":"dfe46515"}"#,
        ];

        let results: Vec<(String, bool)> = lines
            .iter()
            .filter_map(|l| extract_stream_text(l))
            .collect();
        // assistant (true) + result fallback (false)
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1, true, "first should be from assistant");
        assert_eq!(results[1].1, false, "second should be from result");
        assert!(
            results[0].0.contains("Hi!"),
            "First text should contain 'Hi!', got: {}",
            results[0].0
        );
    }
}
