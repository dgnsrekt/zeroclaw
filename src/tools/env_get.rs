use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Read an environment variable by name (only allowlisted variables).
pub struct EnvGetTool {
    security: Arc<SecurityPolicy>,
}

impl EnvGetTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for EnvGetTool {
    fn name(&self) -> &str {
        "env_get"
    }

    fn description(&self) -> &str {
        "Read an environment variable by name (only allowlisted variables)"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The environment variable name to read"
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'name' parameter"))?;

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        if !self.security.is_env_var_allowed(name) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Environment variable not allowed by security policy: {name}"
                )),
            });
        }

        match std::env::var(name) {
            Ok(value) => Ok(ToolResult {
                success: true,
                output: value,
                error: None,
            }),
            Err(std::env::VarError::NotPresent) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Environment variable not set: {name}")),
            }),
            Err(std::env::VarError::NotUnicode(_)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Environment variable contains invalid Unicode: {name}"
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security(allowed_env_vars: Vec<String>) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            allowed_env_vars,
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn env_get_name() {
        let tool = EnvGetTool::new(test_security(vec![]));
        assert_eq!(tool.name(), "env_get");
    }

    #[test]
    fn env_get_schema_has_name() {
        let tool = EnvGetTool::new(test_security(vec![]));
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["name"].is_object());
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("name")));
    }

    #[tokio::test]
    async fn env_get_blocks_unlisted_var() {
        let tool = EnvGetTool::new(test_security(vec![]));
        let result = tool.execute(json!({"name": "PATH"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn env_get_reads_allowed_var() {
        std::env::set_var("ZEROCLAW_TEST_ENV_GET", "test_value_42");
        let tool = EnvGetTool::new(test_security(vec!["ZEROCLAW_TEST_ENV_GET".into()]));
        let result = tool
            .execute(json!({"name": "ZEROCLAW_TEST_ENV_GET"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output, "test_value_42");
        std::env::remove_var("ZEROCLAW_TEST_ENV_GET");
    }

    #[tokio::test]
    async fn env_get_returns_not_set_for_missing_var() {
        let tool = EnvGetTool::new(test_security(vec!["ZEROCLAW_NONEXISTENT_VAR_XYZ".into()]));
        let result = tool
            .execute(json!({"name": "ZEROCLAW_NONEXISTENT_VAR_XYZ"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not set"));
    }

    #[tokio::test]
    async fn env_get_missing_name_param() {
        let tool = EnvGetTool::new(test_security(vec![]));
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn env_get_blocks_when_rate_limited() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            max_actions_per_hour: 0,
            allowed_env_vars: vec!["SOME_VAR".into()],
            ..SecurityPolicy::default()
        });
        let tool = EnvGetTool::new(security);
        let result = tool.execute(json!({"name": "SOME_VAR"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
    }
}
