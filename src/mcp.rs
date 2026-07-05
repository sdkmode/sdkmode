//! Minimal Model Context Protocol server speaking JSON-RPC 2.0 over stdio.
//!
//! The transport is newline-delimited JSON: one JSON-RPC message per line on
//! stdin, one response per line on stdout. The runtime's own stdout is
//! redirected to stderr (see [`crate::sandbox`]) so it cannot corrupt this
//! stream.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::sandbox;

/// The latest MCP protocol revision we advertise when a client does not request
/// a specific one.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

const TOOL_NAME: &str = "run_javascript";

/// Run the MCP server loop until stdin reaches EOF.
pub async fn serve() -> anyhow::Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            break; // EOF: client closed the connection.
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(_) => continue, // Ignore malformed lines rather than crashing.
        };

        if let Some(response) = handle_message(&message).await {
            let mut bytes = serde_json::to_vec(&response)?;
            bytes.push(b'\n');
            stdout.write_all(&bytes).await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

/// Dispatch a single JSON-RPC message, returning a response when one is owed.
/// Notifications (no `id`) and unknown notifications return `None`.
async fn handle_message(message: &Value) -> Option<Value> {
    let method = message.get("method")?.as_str()?;
    let id = message.get("id").cloned();

    match method {
        "initialize" => {
            let protocol_version = message
                .get("params")
                .and_then(|params| params.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION)
                .to_string();

            Some(success(
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "sdkmode",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            ))
        }
        "tools/list" => Some(success(id, json!({ "tools": [tool_schema()] }))),
        "tools/call" => Some(handle_tool_call(id, message.get("params")).await),
        "ping" => Some(success(id, json!({}))),
        _ if is_request(&id) => Some(error(id, -32601, &format!("Method not found: {method}"))),
        // Notifications (e.g. notifications/initialized) get no response.
        _ => None,
    }
}

/// Handle a `tools/call` request, running the requested JavaScript in the
/// sandbox and returning its output as MCP tool content.
async fn handle_tool_call(id: Option<Value>, params: Option<&Value>) -> Value {
    let name = params
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    if name != TOOL_NAME {
        return error(id, -32602, &format!("Unknown tool: {name}"));
    }

    let code = params
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("code"))
        .and_then(Value::as_str);

    let code = match code {
        Some(code) => code.to_string(),
        None => return error(id, -32602, "Missing required string argument: code"),
    };

    match sandbox::execute_code(code).await {
        Ok(result) => {
            let is_error = result.error.is_some();
            let mut text = result.output;
            if let Some(message) = result.error {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&message);
            }
            if text.is_empty() {
                text.push_str("(no output)");
            }
            tool_result(id, text, is_error)
        }
        Err(sandbox_error) => tool_result(id, format!("sandbox error: {sandbox_error}"), true),
    }
}

/// The tool description, with the import allowlist assembled from the SDK
/// registry (see [`crate::sdk::Docs::mcp_blurb`]). Built once, on first use.
static TOOL_DESCRIPTION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let sdks = crate::sdk::registry();
    let blurbs: Vec<&str> = sdks.iter().map(|sdk| sdk.docs().mcp_blurb).collect();
    format!(
        "Execute JavaScript in a sandboxed Deno runtime and return its captured \
    console output. The code runs as an ES module, so top-level `await` and `import` are supported. \
    Imports are restricted to a fixed allowlist of bare specifiers (not arbitrary npm): \
    {} — e.g. `import {{ Octokit }} from \"@octokit/rest\"`. \
    Requests to supported SDKs (e.g. the GitHub API) are transparently authenticated by the host. \
    The sandbox may read/write the working directory but cannot access env vars, spawn processes, or \
    reach private network hosts. Use console.log to surface results.",
        crate::sdk::oxford_join(&blurbs)
    )
});

fn tool_schema() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": TOOL_DESCRIPTION.as_str(),
        "inputSchema": {
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "JavaScript source executed as an ES module. Emit results with console.log."
                }
            },
            "required": ["code"]
        }
    })
}

/// Whether a parsed `id` represents a request (and therefore owes a response)
/// rather than a notification.
fn is_request(id: &Option<Value>) -> bool {
    matches!(id, Some(value) if !value.is_null())
}

fn success(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "result": result,
    })
}

fn error(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": { "code": code, "message": message },
    })
}

fn tool_result(id: Option<Value>, text: String, is_error: bool) -> Value {
    success(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "isError": is_error,
        }),
    )
}
