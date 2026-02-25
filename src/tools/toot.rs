use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

const TOOT_TIMEOUT_SECS: u64 = 30;

pub struct TootTool {
    security: Arc<SecurityPolicy>,
}

impl TootTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }

    fn resolve_bin() -> String {
        std::env::var("TOOT_BIN").unwrap_or_else(|_| "toot".to_string())
    }
}

#[async_trait]
impl Tool for TootTool {
    fn name(&self) -> &str {
        "toot"
    }

    fn description(&self) -> &str {
        "Post a status to Mastodon using the toot CLI. \
         Requires toot to be installed and authenticated. \
         Set TOOT_BIN in .env to override the binary path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The status text to post"
                },
                "visibility": {
                    "type": "string",
                    "enum": ["public", "unlisted", "private", "direct"],
                    "description": "Post visibility: public, unlisted, private, or direct"
                },
                "scheduled_at": {
                    "type": "string",
                    "description": "ISO 8601 datetime to schedule the post, e.g. '2026-02-25T10:00:00'"
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

        let visibility = args
            .get("visibility")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let scheduled_at = args
            .get("scheduled_at")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        if let Some(ref vis) = visibility {
            match vis.as_str() {
                "public" | "unlisted" | "private" | "direct" => {}
                other => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Invalid 'visibility': \"{other}\". \
                             Expected one of: public, unlisted, private, direct"
                        )),
                    });
                }
            }
        }

        let bin = Self::resolve_bin();
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.arg("post");

        if let Some(vis) = visibility {
            cmd.arg("--visibility").arg(vis);
        }

        if let Some(sched) = scheduled_at {
            cmd.arg("--scheduled").arg(sched);
        }

        cmd.arg(&message);

        let timeout = tokio::time::Duration::from_secs(TOOT_TIMEOUT_SECS);
        let output = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "toot binary not found at \"{bin}\". \
                         Install toot or set TOOT_BIN in .env to the full path."
                    )),
                });
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("toot command timed out after {TOOT_TIMEOUT_SECS}s")),
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if output.status.success() {
            Ok(ToolResult {
                success: true,
                output: if stdout.is_empty() {
                    "Posted successfully.".to_string()
                } else {
                    stdout
                },
                error: None,
            })
        } else {
            let error_msg = if stderr.is_empty() {
                stdout.clone()
            } else {
                stderr
            };
            Ok(ToolResult {
                success: false,
                output: stdout,
                error: Some(format!("toot exited with error: {error_msg}")),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn test_security(level: AutonomyLevel, max_actions_per_hour: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn toot_tool_name() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 100));
        assert_eq!(tool.name(), "toot");
    }

    #[test]
    fn toot_tool_description() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 100));
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn toot_tool_has_parameters_schema() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 100));
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("message").is_some());
    }

    #[test]
    fn toot_tool_requires_message() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 100));
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::Value::String("message".to_string())));
    }

    #[test]
    fn toot_tool_schema_has_visibility() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 100));
        let schema = tool.parameters_schema();
        assert!(schema["properties"].get("visibility").is_some());
    }

    #[test]
    fn toot_tool_schema_has_scheduled_at() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 100));
        let schema = tool.parameters_schema();
        assert!(schema["properties"].get("scheduled_at").is_some());
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let tool = TootTool::new(test_security(AutonomyLevel::ReadOnly, 100));
        let result = tool.execute(json!({"message": "hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_rate_limit() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 0));
        let result = tool.execute(json!({"message": "hello"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn execute_rejects_invalid_visibility() {
        let tool = TootTool::new(test_security(AutonomyLevel::Full, 100));
        let result = tool
            .execute(json!({"message": "hello", "visibility": "world"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid 'visibility'"));
    }
}
