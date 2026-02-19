use super::traits::{Tool, ToolResult};
use crate::config::NtfyConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

const NTFY_CONNECT_TIMEOUT_SECS: u64 = 10;

pub struct NtfyTool {
    security: Arc<SecurityPolicy>,
    config: NtfyConfig,
}

impl NtfyTool {
    pub fn new(security: Arc<SecurityPolicy>, config: NtfyConfig) -> Self {
        Self { security, config }
    }

    fn resolve_target(
        &self,
        name: Option<&str>,
    ) -> Result<&crate::config::NtfyTargetConfig, String> {
        let target_name = name
            .or(self.config.default_target.as_deref())
            .ok_or_else(|| "No target specified and no default_target configured".to_string())?;

        self.config
            .targets
            .iter()
            .find(|t| t.name == target_name)
            .ok_or_else(|| {
                let available: Vec<&str> = self
                    .config
                    .targets
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect();
                format!(
                    "Unknown ntfy target '{}'. Available targets: {:?}",
                    target_name, available
                )
            })
    }
}

#[async_trait]
impl Tool for NtfyTool {
    fn name(&self) -> &str {
        "ntfy"
    }

    fn description(&self) -> &str {
        "Send a push notification via ntfy to a configured topic. Supports multiple named targets with different hosts and topics."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The notification message body"
                },
                "target": {
                    "type": "string",
                    "description": "Named target from config (falls back to default_target if omitted)"
                },
                "title": {
                    "type": "string",
                    "description": "Optional notification title"
                },
                "priority": {
                    "type": "integer",
                    "description": "Message priority: 1 (min), 2 (low), 3 (default), 4 (high), 5 (urgent)",
                    "minimum": 1,
                    "maximum": 5
                },
                "tags": {
                    "type": "string",
                    "description": "Comma-separated tags or emoji shortcodes (e.g. 'warning,skull')"
                },
                "markdown": {
                    "type": "boolean",
                    "description": "Enable markdown rendering in the notification body"
                }
            },
            "required": ["message"]
        })
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

        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?
            .to_string();

        let target_name = args.get("target").and_then(|v| v.as_str());

        let target = match self.resolve_target(target_name) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };

        // Validate priority range (1-5)
        let priority = match args.get("priority").and_then(|v| v.as_i64()) {
            Some(value) if (1..=5).contains(&value) => Some(value),
            Some(value) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Invalid 'priority': {value}. Expected integer in range 1..=5"
                    )),
                });
            }
            None => None,
        };

        let title = args.get("title").and_then(|v| v.as_str());
        let tags = args.get("tags").and_then(|v| v.as_str());
        let markdown = args
            .get("markdown")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Build URL: host/topic (host may be http:// or https://)
        let url = format!("{}/{}", target.host.trim_end_matches('/'), target.topic);

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.ntfy",
            self.config.timeout_secs,
            NTFY_CONNECT_TIMEOUT_SECS,
        );

        let mut request = client.post(&url).body(message);

        if let Some(title) = title {
            request = request.header("Title", title);
        }

        if let Some(priority) = priority {
            request = request.header("Priority", priority.to_string());
        }

        if let Some(tags) = tags {
            request = request.header("Tags", tags);
        }

        if markdown {
            request = request.header("Markdown", "yes");
        }

        let response = request.send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            Ok(ToolResult {
                success: true,
                output: format!("ntfy notification sent successfully. Response: {}", body),
                error: None,
            })
        } else {
            Ok(ToolResult {
                success: false,
                output: body,
                error: Some(format!("ntfy API returned status {}", status)),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NtfyTargetConfig;
    use crate::security::AutonomyLevel;

    fn test_security(level: AutonomyLevel, max_actions_per_hour: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_config() -> NtfyConfig {
        NtfyConfig {
            enabled: true,
            default_target: Some("alerts".to_string()),
            timeout_secs: 15,
            targets: vec![
                NtfyTargetConfig {
                    name: "alerts".to_string(),
                    host: "https://ntfy.sh".to_string(),
                    topic: "test-alerts".to_string(),
                },
                NtfyTargetConfig {
                    name: "logs".to_string(),
                    host: "http://nas1-oryx.lan:2586".to_string(),
                    topic: "build-logs".to_string(),
                },
            ],
        }
    }

    #[test]
    fn ntfy_tool_name() {
        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        assert_eq!(tool.name(), "ntfy");
    }

    #[test]
    fn ntfy_tool_has_parameters_schema() {
        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("message").is_some());
        assert!(schema["properties"].get("target").is_some());
        assert!(schema["properties"].get("title").is_some());
        assert!(schema["properties"].get("priority").is_some());
        assert!(schema["properties"].get("tags").is_some());
        assert!(schema["properties"].get("markdown").is_some());
    }

    #[test]
    fn ntfy_tool_requires_message() {
        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::Value::String("message".to_string())));
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let tool = NtfyTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());

        let result = tool.execute(json!({"message": "hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_rate_limit() {
        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 0), test_config());

        let result = tool.execute(json!({"message": "hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn execute_rejects_unknown_target() {
        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({"message": "hello", "target": "nonexistent"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown ntfy target"));
    }

    #[tokio::test]
    async fn execute_rejects_priority_out_of_range() {
        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({"message": "hello", "priority": 6}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("1..=5"));
    }

    #[tokio::test]
    async fn execute_uses_default_target() {
        // This test verifies target resolution — it won't actually send
        // because there's no real ntfy server, but it should get past
        // target resolution and fail at the HTTP level, not with a target error.
        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool.execute(json!({"message": "hello"})).await;
        // Should not fail with a "no target" error — the default_target resolves
        match result {
            Ok(r) => {
                // If it fails, it should be a network error, not a target resolution error
                if let Some(ref err) = r.error {
                    assert!(!err.contains("No target specified"));
                    assert!(!err.contains("Unknown ntfy target"));
                }
            }
            Err(_) => {
                // Network error is expected in tests (no real server)
            }
        }
    }

    #[tokio::test]
    async fn execute_fails_without_target_or_default() {
        let config = NtfyConfig {
            enabled: true,
            default_target: None,
            timeout_secs: 15,
            targets: vec![NtfyTargetConfig {
                name: "alerts".to_string(),
                host: "https://ntfy.sh".to_string(),
                topic: "test-alerts".to_string(),
            }],
        };

        let tool = NtfyTool::new(test_security(AutonomyLevel::Full, 100), config);

        let result = tool.execute(json!({"message": "hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("No target specified"));
    }
}
