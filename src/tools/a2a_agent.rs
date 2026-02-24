use super::traits::{Tool, ToolResult};
use crate::config::A2aConfig;
use crate::security::SecurityPolicy;
use a2a_client::A2AClient;
use a2a_types::{
    Message, MessageRole, MessageSendParams, Part, SendMessageResponse, SendMessageResult,
    SendMessageSuccessResponse, TaskState,
};
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::sync::Arc;

pub struct A2aAgentTool {
    security: Arc<SecurityPolicy>,
    config: A2aConfig,
    description: String,
}

impl A2aAgentTool {
    pub fn new(security: Arc<SecurityPolicy>, config: A2aConfig) -> Self {
        let description = Self::build_description(&config);
        Self {
            security,
            config,
            description,
        }
    }

    fn build_description(config: &A2aConfig) -> String {
        let mut desc = String::from(
            "Send a message to a remote A2A (Agent-to-Agent) protocol agent and receive its response.",
        );
        if config.targets.is_empty() {
            return desc;
        }
        desc.push_str(" Available agents:");
        for target in &config.targets {
            let _ = write!(desc, "\n- \"{}\" ({})", target.name, target.base_url);
            if let Some(ref notes) = target.notes {
                let _ = write!(desc, " — {}", notes);
            }
        }
        desc
    }

    fn resolve_target(&self, name: &str) -> Result<&crate::config::A2aAgentTarget, String> {
        self.config
            .targets
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| {
                let available: Vec<&str> = self
                    .config
                    .targets
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect();
                format!(
                    "Unknown a2a agent '{}'. Available agents: {:?}",
                    name, available
                )
            })
    }

    fn extract_text_from_parts(parts: &[Part]) -> String {
        parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl Tool for A2aAgentTool {
    fn name(&self) -> &str {
        "a2a_agent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Name of the remote A2A agent to call"
                },
                "message": {
                    "type": "string",
                    "description": "The message to send to the remote agent"
                },
                "context_id": {
                    "type": "string",
                    "description": "Optional context ID to group related interactions with the same agent"
                }
            },
            "required": ["agent", "message"]
        });
        if !self.config.targets.is_empty() {
            let names: Vec<&str> = self
                .config
                .targets
                .iter()
                .map(|t| t.name.as_str())
                .collect();
            schema["properties"]["agent"]["enum"] = json!(names);
        }
        schema
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'agent' parameter"))?;

        let message_text = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty());

        let message_text = match message_text {
            Some(text) => text.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing or empty 'message' parameter".into()),
                });
            }
        };

        let context_id = args
            .get("context_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let target = match self.resolve_target(agent_name) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };

        // Build HTTP client with proxy and timeout support
        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.a2a",
            self.config.timeout_secs,
            self.config.connect_timeout_secs,
        );

        // Fetch agent card to resolve the service endpoint URL
        let a2a_client =
            match A2AClient::from_card_url_with_client(&target.base_url, client.clone()).await {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Failed to connect to A2A agent '{}' at {}: {}",
                            agent_name, target.base_url, e
                        )),
                    });
                }
            };

        // Build the A2A message
        let message = Message {
            kind: "message".to_string(),
            message_id: uuid::Uuid::new_v4().to_string(),
            role: MessageRole::User,
            parts: vec![Part::Text {
                text: message_text,
                metadata: None,
            }],
            context_id,
            task_id: None,
            reference_task_ids: Vec::new(),
            extensions: Vec::new(),
            metadata: None,
        };

        let params = MessageSendParams {
            message,
            configuration: None,
            metadata: None,
        };

        // Send with overall timeout.
        // a2a-client 0.1.2 send_message() uses the wrong inner type (SendMessageResponse
        // instead of SendMessageResult), causing deserialization to fail against spec-compliant
        // servers that return Task/Message directly in the result field. We make the raw HTTP
        // call ourselves and handle both the standard format and the double-wrapped format used
        // by some servers.
        let service_url = a2a_client.agent_card().url.clone();
        let auth_token = target.auth_token.clone();
        let timeout_duration = std::time::Duration::from_secs(self.config.timeout_secs);

        let send_result = tokio::time::timeout(timeout_duration, async {
            let params_json = serde_json::to_value(&params)
                .map_err(|e| anyhow::anyhow!("Failed to serialize params: {}", e))?;

            let mut req = client
                .post(&service_url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .json(&json!({
                    "jsonrpc": "2.0",
                    "method": "message/send",
                    "id": 1,
                    "params": params_json
                }));

            if let Some(ref token) = auth_token {
                req = req.bearer_auth(token);
            }

            let http_resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Network error: {}", e))?;

            if !http_resp.status().is_success() {
                let status = http_resp.status();
                let body = http_resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("HTTP error {}: {}", status, body));
            }

            let raw: serde_json::Value = http_resp
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to decode response: {}", e))?;

            let result_value = raw
                .get("result")
                .ok_or_else(|| anyhow::anyhow!("missing 'result' field in response"))?;

            // Handle both A2A 0.3 spec format (result is Task/Message directly) and
            // double-wrapped format where result is {jsonrpc, id, result: Task/Message}.
            let send_result: SendMessageResult = if result_value.get("jsonrpc").is_some() {
                let inner = result_value.get("result").ok_or_else(|| {
                    anyhow::anyhow!("missing inner 'result' in double-wrapped response")
                })?;
                serde_json::from_value(inner.clone())
                    .map_err(|e| anyhow::anyhow!("Failed to parse inner result: {}", e))?
            } else {
                serde_json::from_value(result_value.clone())
                    .map_err(|e| anyhow::anyhow!("Failed to parse result: {}", e))?
            };

            Ok::<SendMessageResponse, anyhow::Error>(SendMessageResponse::Success(Box::new(
                SendMessageSuccessResponse {
                    jsonrpc: "2.0".to_string(),
                    result: send_result,
                    id: None,
                },
            )))
        })
        .await;

        let response = match send_result {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("A2A agent '{}' returned error: {}", agent_name, e)),
                });
            }
            Err(_) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "A2A agent '{}' timed out after {}s",
                        agent_name, self.config.timeout_secs
                    )),
                });
            }
        };

        // Extract text from response
        match response {
            SendMessageResponse::Success(success) => match success.result {
                SendMessageResult::Message(msg) => {
                    let text = Self::extract_text_from_parts(&msg.parts);
                    Ok(ToolResult {
                        success: true,
                        output: text,
                        error: None,
                    })
                }
                SendMessageResult::Task(task) => {
                    let mut output = String::new();
                    let _ = write!(
                        output,
                        "Task ID: {}\nState: {:?}",
                        task.id, task.status.state
                    );

                    // Extract text from status message if present
                    if let Some(ref status_msg) = task.status.message {
                        let status_text = Self::extract_text_from_parts(&status_msg.parts);
                        if !status_text.is_empty() {
                            let _ = write!(output, "\nStatus: {}", status_text);
                        }
                    }

                    // Extract text from history
                    for msg in &task.history {
                        if msg.role == MessageRole::Agent {
                            let text = Self::extract_text_from_parts(&msg.parts);
                            if !text.is_empty() {
                                let _ = write!(output, "\n\n{}", text);
                            }
                        }
                    }

                    // Extract text from artifacts
                    for artifact in &task.artifacts {
                        let text = Self::extract_text_from_parts(&artifact.parts);
                        if !text.is_empty() {
                            let _ = write!(output, "\n\n{}", text);
                        }
                    }

                    let success = matches!(
                        task.status.state,
                        TaskState::Completed | TaskState::Working | TaskState::Submitted
                    );
                    Ok(ToolResult {
                        success,
                        output,
                        error: if success {
                            None
                        } else {
                            Some(format!("Task state: {:?}", task.status.state))
                        },
                    })
                }
            },
            SendMessageResponse::Error(err) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "A2A agent '{}' JSON-RPC error {}: {}",
                    agent_name, err.error.code, err.error.message
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::A2aAgentTarget;
    use crate::security::AutonomyLevel;

    fn test_security(level: AutonomyLevel, max_actions_per_hour: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_config() -> A2aConfig {
        A2aConfig {
            enabled: true,
            timeout_secs: 120,
            connect_timeout_secs: 10,
            targets: vec![
                A2aAgentTarget {
                    name: "researcher".to_string(),
                    base_url: "https://researcher.example.com".to_string(),
                    auth_token: None,
                    notes: Some("Deep research agent".to_string()),
                },
                A2aAgentTarget {
                    name: "coder".to_string(),
                    base_url: "https://coder.example.com".to_string(),
                    auth_token: Some("test-token".to_string()),
                    notes: None,
                },
            ],
        }
    }

    #[test]
    fn a2a_agent_tool_name() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        assert_eq!(tool.name(), "a2a_agent");
    }

    #[test]
    fn a2a_agent_tool_has_parameters_schema() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("agent").is_some());
        assert!(schema["properties"].get("message").is_some());
        assert!(schema["properties"].get("context_id").is_some());
    }

    #[test]
    fn a2a_agent_tool_requires_agent_and_message() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("agent")));
        assert!(required.contains(&json!("message")));
    }

    #[test]
    fn a2a_agent_tool_description_lists_targets() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        assert!(desc.contains("\"researcher\""));
        assert!(desc.contains("https://researcher.example.com"));
        assert!(desc.contains("Deep research agent"));
        assert!(desc.contains("\"coder\""));
        assert!(desc.contains("https://coder.example.com"));
    }

    #[test]
    fn a2a_agent_tool_description_omits_notes_when_none() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        let coder_line = desc.lines().find(|l| l.contains("\"coder\"")).unwrap();
        assert!(!coder_line.contains(" — "));
    }

    #[test]
    fn a2a_agent_tool_schema_enumerates_targets() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let agent_enum = schema["properties"]["agent"]["enum"]
            .as_array()
            .expect("agent should have enum");
        assert_eq!(agent_enum, &vec![json!("researcher"), json!("coder")]);
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());

        let result = tool
            .execute(json!({"agent": "researcher", "message": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_rate_limit() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 0), test_config());

        let result = tool
            .execute(json!({"agent": "researcher", "message": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn execute_rejects_unknown_agent() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({"agent": "nonexistent", "message": "hello"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown a2a agent"));
    }

    #[tokio::test]
    async fn execute_rejects_empty_message() {
        let tool = A2aAgentTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({"agent": "researcher", "message": "   "}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("message"));
    }
}
