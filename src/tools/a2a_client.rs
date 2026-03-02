//! A2A (Agent-to-Agent) client tool.
//!
//! Wraps a remote A2A agent endpoint as a zeroclaw [`Tool`] so the agent loop
//! can delegate tasks to other A2A-compatible agents (e.g. dscraper, picobot)
//! via standard JSON-RPC 2.0 `message/send` calls.

use std::time::Duration;

use async_trait::async_trait;

use crate::tools::traits::{Tool, ToolResult};

/// A zeroclaw [`Tool`] that delegates to a remote A2A agent.
///
/// The tool name is `a2a__{name}__delegate` (e.g. `a2a__dscraper__delegate`).
/// A single `message` parameter (string) is forwarded as a `message/send`
/// JSON-RPC POST to the remote agent's `/a2a` endpoint.
pub struct A2aClientTool {
    /// Prefixed name: `a2a__<agent_name>__delegate`.
    name: String,
    /// Static description built at construction time.
    description: String,
    /// Base URL of the remote agent (trailing slash stripped).
    base_url: String,
    /// Pre-built reqwest client with per-tool timeout.
    client: reqwest::Client,
}

impl A2aClientTool {
    /// Construct a new `A2aClientTool`.
    ///
    /// Validates `agent_name` (alphanumeric, `_`, `-` only) and `base_url`
    /// (must parse as an `http` or `https` URL) before building the client.
    pub fn new(agent_name: &str, base_url: &str, timeout_secs: u64) -> anyhow::Result<Self> {
        // Validate URL
        let parsed = reqwest::Url::parse(base_url)
            .map_err(|e| anyhow::anyhow!("a2a client '{}': invalid url: {e}", agent_name))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            anyhow::bail!(
                "a2a client '{}': url must use http or https scheme",
                agent_name
            );
        }

        // Validate name
        if agent_name.is_empty()
            || !agent_name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            anyhow::bail!(
                "a2a client: invalid agent name '{}' (only alphanumeric, _ and - allowed)",
                agent_name
            );
        }

        let timeout = Duration::from_secs(timeout_secs.max(1));
        let client = reqwest::Client::builder().timeout(timeout).build()?;

        Ok(Self {
            name: format!("a2a__{agent_name}__delegate"),
            description: format!("Delegate to A2A agent '{agent_name}' at {base_url}"),
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        })
    }
}

#[async_trait]
impl Tool for A2aClientTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Message to send to the remote A2A agent"
                }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let id = uuid::Uuid::new_v4().to_string();
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "message/send",
            "params": {
                "message": {
                    "role": "user",
                    "parts": [{"type": "text", "text": msg}]
                }
            }
        });

        let resp = self
            .client
            .post(format!("{}/a2a", self.base_url))
            .json(&payload)
            .send()
            .await;

        match resp {
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
            Ok(r) => {
                let body: serde_json::Value = r
                    .json()
                    .await
                    .unwrap_or(serde_json::Value::Null);

                // JSON-RPC error field takes priority
                if let Some(err) = body.get("error") {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(err.to_string()),
                    });
                }

                let result = body.get("result");
                let text = extract_a2a_text(&result).unwrap_or_else(|| {
                    result
                        .map(|v| v.to_string())
                        .unwrap_or_default()
                });

                Ok(ToolResult {
                    success: true,
                    output: text,
                    error: None,
                })
            }
        }
    }
}

/// Walk an A2A response result looking for the first text part.
///
/// Handles both Task (`result.artifacts[].parts[].text`) and Message
/// (`result.parts[].text`) response shapes.
fn extract_a2a_text(result: &Option<&serde_json::Value>) -> Option<String> {
    let v = (*result)?;

    // Task path: result.artifacts[n].parts[n].text
    if let Some(artifacts) = v.get("artifacts").and_then(|a| a.as_array()) {
        for artifact in artifacts {
            if let Some(text) = find_text_part(artifact.get("parts")) {
                return Some(text);
            }
        }
    }

    // Message path: result.parts[n].text
    if let Some(text) = find_text_part(v.get("parts")) {
        return Some(text);
    }

    // Bare string result
    v.as_str().map(|s| s.to_string())
}

fn find_text_part(parts: Option<&serde_json::Value>) -> Option<String> {
    parts?.as_array()?.iter().find_map(|p| {
        if p.get("type")?.as_str()? == "text" {
            p.get("text")?.as_str().map(|s| s.to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_is_prefixed() {
        let tool = A2aClientTool::new("foo", "http://localhost:8000", 60).unwrap();
        assert_eq!(tool.name(), "a2a__foo__delegate");
    }

    #[test]
    fn extract_a2a_text_task_path() {
        let result = serde_json::json!({
            "artifacts": [{
                "parts": [{"type": "text", "text": "task reply"}]
            }]
        });
        assert_eq!(
            extract_a2a_text(&Some(&result)),
            Some("task reply".to_string())
        );
    }

    #[test]
    fn extract_a2a_text_message_path() {
        let result = serde_json::json!({
            "parts": [{"type": "text", "text": "message reply"}]
        });
        assert_eq!(
            extract_a2a_text(&Some(&result)),
            Some("message reply".to_string())
        );
    }

    #[test]
    fn extract_a2a_text_missing_returns_none() {
        assert_eq!(extract_a2a_text(&None), None);
        let empty = serde_json::json!({});
        assert_eq!(extract_a2a_text(&Some(&empty)), None);
    }

    #[test]
    fn invalid_scheme_rejected() {
        let err = A2aClientTool::new("agent", "ftp://example.com", 30);
        assert!(err.is_err());
    }

    #[test]
    fn invalid_name_rejected() {
        let err = A2aClientTool::new("bad name!", "http://localhost:8000", 30);
        assert!(err.is_err());
    }

    #[test]
    fn empty_name_rejected() {
        let err = A2aClientTool::new("", "http://localhost:8000", 30);
        assert!(err.is_err());
    }
}
