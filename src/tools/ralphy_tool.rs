use super::traits::{Tool, ToolResult};
use crate::config::RalphyConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::io::Write as _;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tracing::{debug, warn};

/// Maximum output bytes before truncation (1 MB).
const MAX_OUTPUT_BYTES: usize = 1_048_576;

pub struct RalphyTool {
    security: Arc<SecurityPolicy>,
    config: RalphyConfig,
    description: String,
}

impl RalphyTool {
    pub fn new(security: Arc<SecurityPolicy>, config: RalphyConfig) -> Self {
        let description = String::from(
            "Execute a PRD (Product Requirements Document) with ralphy to run multi-step coding tasks via Codex.\n\
             \n\
             WORKFLOW: First use the MCP \"codex\" tool to have Codex write an optimal task list, then call this tool to execute it.\n\
             \n\
             Each task should be:\n\
             - Atomic and testable (one clear action per task)\n\
             - Descriptive title (the agent sees this as its primary instruction)\n\
             - Optional description for additional context\n\
             \n\
             Use parallel_group to run independent tasks concurrently (same group number = run together).\n\
             Tasks without parallel_group run sequentially.\n\
             \n\
             In sequential mode, description enriches the agent prompt.\n\
             In parallel mode, only the title is sent to the agent — put critical info in the title.",
        );
        Self {
            security,
            config,
            description,
        }
    }
}

#[async_trait]
impl Tool for RalphyTool {
    fn name(&self) -> &str {
        "ralphy"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "Array of task objects. Each has: title (required string), description (optional string), parallel_group (optional integer).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "Primary instruction for the coding agent. Be specific and actionable."
                            },
                            "description": {
                                "type": "string",
                                "description": "Additional context for the task. In parallel mode only the title is sent."
                            },
                            "parallel_group": {
                                "type": "integer",
                                "description": "Tasks with the same group number run concurrently."
                            }
                        },
                        "required": ["title"]
                    }
                },
                "parallel": {
                    "type": "boolean",
                    "description": "Run tasks in parallel mode (default: false)."
                }
            },
            "required": ["tasks"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Security gates
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

        // Validate working_dir is configured
        let working_dir = match &self.config.working_dir {
            Some(dir) if !dir.is_empty() => dir.clone(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        "Ralphy working_dir is not configured. Set [ralphy] working_dir in config.toml."
                            .into(),
                    ),
                });
            }
        };

        // Parse and validate tasks
        let tasks = args
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                anyhow::anyhow!("Missing or invalid 'tasks' parameter (expected array)")
            })?;

        if tasks.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Tasks array is empty. Provide at least one task.".into()),
            });
        }

        // Validate each task has a title
        for (i, task) in tasks.iter().enumerate() {
            let title = task
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if title.is_none() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Task at index {} is missing a non-empty 'title' field.",
                        i
                    )),
                });
            }
        }

        let parallel = args
            .get("parallel")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Build the PRD YAML structure
        let prd_tasks: Vec<serde_yaml::Value> = tasks
            .iter()
            .map(|t| {
                let mut map = serde_yaml::Mapping::new();
                if let Some(title) = t.get("title").and_then(|v| v.as_str()) {
                    map.insert(
                        serde_yaml::Value::String("title".into()),
                        serde_yaml::Value::String(title.into()),
                    );
                }
                if let Some(desc) = t.get("description").and_then(|v| v.as_str()) {
                    map.insert(
                        serde_yaml::Value::String("description".into()),
                        serde_yaml::Value::String(desc.into()),
                    );
                }
                if let Some(pg) = t.get("parallel_group").and_then(|v| v.as_i64()) {
                    map.insert(
                        serde_yaml::Value::String("parallel_group".into()),
                        serde_yaml::Value::Number(pg.into()),
                    );
                }
                serde_yaml::Value::Mapping(map)
            })
            .collect();

        let mut prd_root = serde_yaml::Mapping::new();
        prd_root.insert(
            serde_yaml::Value::String("tasks".into()),
            serde_yaml::Value::Sequence(prd_tasks),
        );

        let yaml_content = serde_yaml::to_string(&serde_yaml::Value::Mapping(prd_root))
            .map_err(|e| anyhow::anyhow!("Failed to serialize tasks to YAML: {}", e))?;

        // Write YAML to temp file
        let mut temp_file = tempfile::NamedTempFile::new()
            .map_err(|e| anyhow::anyhow!("Failed to create temp file: {}", e))?;
        temp_file
            .write_all(yaml_content.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write YAML to temp file: {}", e))?;
        temp_file
            .flush()
            .map_err(|e| anyhow::anyhow!("Failed to flush temp file: {}", e))?;

        let temp_path = temp_file.path().to_path_buf();

        // Build command
        let mut cmd = Command::new(&self.config.command);
        cmd.arg("--codex").arg("--yaml").arg(&temp_path);

        if parallel {
            cmd.arg("--parallel").arg("--max-parallel").arg("3");
        }

        cmd.current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        debug!(
            command = %self.config.command,
            working_dir = %working_dir,
            parallel,
            task_count = tasks.len(),
            "Ralphy PRD execution starting"
        );

        // Spawn the process
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Failed to spawn ralphy (command: {}): {}",
                        self.config.command, e
                    )),
                });
            }
        };

        // Wait with timeout
        let timeout = std::time::Duration::from_secs(self.config.timeout_secs);
        let result = tokio::time::timeout(timeout, child.wait_with_output()).await;

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                // Combine output
                if !stderr.is_empty() {
                    stdout.push_str("\n--- stderr ---\n");
                    stdout.push_str(&stderr);
                }

                // Truncate if over limit
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(MAX_OUTPUT_BYTES);
                    stdout.push_str("\n... [output truncated at 1MB]");
                }

                let success = output.status.success();
                debug!(
                    exit_code = output.status.code(),
                    output_len = stdout.len(),
                    "Ralphy PRD execution completed"
                );

                let error = if success {
                    None
                } else {
                    Some(format!(
                        "Ralphy exited with status: {}",
                        output
                            .status
                            .code()
                            .map_or("unknown".into(), |c| c.to_string())
                    ))
                };

                Ok(ToolResult {
                    success,
                    output: stdout,
                    error,
                })
            }
            Ok(Err(e)) => {
                warn!(error = %e, "Ralphy process I/O error");
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Ralphy process error: {}", e)),
                })
            }
            Err(_) => {
                warn!(
                    timeout_secs = self.config.timeout_secs,
                    "Ralphy PRD execution timed out"
                );
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Ralphy PRD execution timed out after {}s",
                        self.config.timeout_secs
                    )),
                })
            }
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

    fn test_config() -> RalphyConfig {
        RalphyConfig {
            enabled: true,
            working_dir: Some("/tmp".to_string()),
            timeout_secs: 60,
            command: "ralphy".to_string(),
        }
    }

    #[test]
    fn tool_name_is_ralphy() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        assert_eq!(tool.name(), "ralphy");
    }

    #[test]
    fn schema_has_tasks_required() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("tasks").is_some());
        assert!(schema["properties"].get("parallel").is_some());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("tasks")));
    }

    #[test]
    fn schema_tasks_is_array_with_title_required() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["properties"]["tasks"]["type"], "array");
        let item_required = schema["properties"]["tasks"]["items"]["required"]
            .as_array()
            .unwrap();
        assert!(item_required.contains(&json!("title")));
    }

    #[test]
    fn schema_parallel_is_optional_bool() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["properties"]["parallel"]["type"], "boolean");
        let required = schema["required"].as_array().unwrap();
        assert!(!required.contains(&json!("parallel")));
    }

    #[test]
    fn description_mentions_prd_codex_parallel() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        assert!(desc.contains("PRD"));
        assert!(desc.contains("codex"));
        assert!(desc.contains("parallel_group"));
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());
        let result = tool
            .execute(json!({"tasks": [{"title": "test"}]}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_rate_limit() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 0), test_config());
        let result = tool
            .execute(json!({"tasks": [{"title": "test"}]}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn execute_rejects_missing_tasks() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_rejects_empty_tasks() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool.execute(json!({"tasks": []})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("empty"));
    }

    #[tokio::test]
    async fn execute_rejects_task_without_title() {
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({"tasks": [{"description": "no title here"}]}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("title"));
    }

    #[tokio::test]
    async fn execute_rejects_missing_working_dir() {
        let config = RalphyConfig {
            enabled: true,
            working_dir: None,
            timeout_secs: 60,
            command: "ralphy".to_string(),
        };
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), config);
        let result = tool
            .execute(json!({"tasks": [{"title": "test"}]}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("working_dir"));
    }

    #[tokio::test]
    async fn execute_graceful_spawn_failure() {
        let config = RalphyConfig {
            enabled: true,
            working_dir: Some("/tmp".to_string()),
            timeout_secs: 60,
            command: "/nonexistent/path/to/ralphy".to_string(),
        };
        let tool = RalphyTool::new(test_security(AutonomyLevel::Full, 100), config);
        let result = tool
            .execute(json!({"tasks": [{"title": "test task"}]}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Failed to spawn"));
    }
}
