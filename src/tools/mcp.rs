use super::traits::{Tool, ToolResult};
use crate::config::McpConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use rmcp::model::{CallToolRequestParam, RawContent};
use rmcp::service::RunningService;
use rmcp::ServiceExt;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

type McpClient = RunningService<rmcp::RoleClient, ()>;

struct McpClientHandle {
    client: McpClient,
    _child: tokio::process::Child,
    _filter_task: tokio::task::JoinHandle<()>,
}

/// Returns `true` if the line is a non-standard JSON-RPC notification that rmcp
/// would attempt to skip (triggering a codec bug in rmcp <=0.8.5 where the
/// response sitting in the buffer never gets processed).
///
/// Standard MCP notifications have methods like `notifications/...` or `$/...`.
/// Non-standard ones (e.g. `codex/event`) are filtered out before reaching rmcp.
fn is_non_standard_notification(line: &str) -> bool {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let Some(method) = val.get("method").and_then(|m| m.as_str()) else {
        return false;
    };
    // If it has an "id", it's a request, not a notification — keep it.
    if val.get("id").is_some() {
        return false;
    }
    // Standard MCP notification prefixes
    if method.starts_with("notifications/") || method.starts_with("$/") {
        return false;
    }
    // Standard MCP lifecycle methods
    if matches!(method, "initialized" | "cancelled" | "progress") {
        return false;
    }
    // Everything else is non-standard — filter it out.
    true
}

pub struct McpTool {
    security: Arc<SecurityPolicy>,
    config: McpConfig,
    description: String,
    /// Outer mutex: brief lock for HashMap lookup/insert.
    /// Inner mutex per server: held during tool calls (serializes per-server, concurrent across servers).
    clients: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<McpClientHandle>>>>,
}

impl McpTool {
    pub fn new(security: Arc<SecurityPolicy>, config: McpConfig) -> Self {
        let description = Self::build_description(&config);
        Self {
            security,
            config,
            description,
            clients: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    fn build_description(config: &McpConfig) -> String {
        let mut desc = String::from(
            "Call a tool on an MCP (Model Context Protocol) server. \
             You MUST specify the server name, the tool name, and the tool's arguments.",
        );
        if config.servers.is_empty() {
            return desc;
        }
        desc.push_str("\n\nAvailable servers and tools:");
        for server in &config.servers {
            let _ = write!(desc, "\n\n## Server: \"{}\"", server.name);
            if let Some(ref notes) = server.notes {
                let _ = write!(desc, "\n{}", notes);
            }
            if server.allowed_tools.is_empty() {
                desc.push_str("\nAll tools on this server are available.");
            } else {
                desc.push_str("\nAvailable tools:");
                for tool_name in &server.allowed_tools {
                    let _ = write!(desc, "\n- \"{}\"", tool_name);
                }
            }
        }
        desc
    }

    fn resolve_server(&self, name: &str) -> Result<&crate::config::McpServerConfig, String> {
        self.config
            .servers
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                let available: Vec<&str> = self
                    .config
                    .servers
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect();
                format!(
                    "Unknown MCP server '{}'. Available servers: {:?}",
                    name, available
                )
            })
    }

    fn is_tool_allowed(server: &crate::config::McpServerConfig, tool_name: &str) -> bool {
        server.allowed_tools.is_empty() || server.allowed_tools.iter().any(|t| t == tool_name)
    }

    async fn get_or_connect(
        &self,
        server: &crate::config::McpServerConfig,
    ) -> Result<Arc<tokio::sync::Mutex<McpClientHandle>>, String> {
        // Brief lock to check cache
        {
            let clients = self.clients.lock().await;
            if let Some(handle) = clients.get(&server.name) {
                let guard = handle.lock().await;
                if !guard.client.is_transport_closed() {
                    return Ok(Arc::clone(handle));
                }
                // Transport is closed; drop guard and reconnect below
                drop(guard);
            }
        }

        // Spawn child process manually so we can filter its stdout before rmcp
        // sees it. This works around an rmcp <=0.8.5 codec bug where
        // non-standard notifications (e.g. codex/event) stall response parsing.
        let mut child = Command::new(&server.command)
            .args(&server.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                format!(
                    "Failed to spawn MCP server '{}' (command: {} {}): {}",
                    server.name,
                    server.command,
                    server.args.join(" "),
                    e
                )
            })?;

        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("Failed to capture stdout for MCP server '{}'", server.name))?;
        let child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("Failed to capture stdin for MCP server '{}'", server.name))?;

        // Duplex pipe: filter task writes valid lines -> rmcp reads from it
        let (mut filter_writer, filter_reader) = tokio::io::duplex(65536);

        let server_name_log = server.name.clone();
        let filter_task = tokio::spawn(async move {
            let mut reader = BufReader::new(child_stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF — child closed stdout
                    Ok(_) => {
                        if is_non_standard_notification(&line) {
                            debug!(
                                server = %server_name_log,
                                line = line.trim(),
                                "Filtered non-standard MCP notification"
                            );
                            continue;
                        }
                        if filter_writer.write_all(line.as_bytes()).await.is_err() {
                            break; // rmcp side dropped
                        }
                    }
                    Err(e) => {
                        warn!(
                            server = %server_name_log,
                            error = %e,
                            "Error reading MCP server stdout"
                        );
                        break;
                    }
                }
            }
        });

        // rmcp reads filtered stdout, writes directly to child stdin
        let startup_timeout = std::time::Duration::from_secs(server.startup_timeout_secs);
        let client = tokio::time::timeout(startup_timeout, ().serve((filter_reader, child_stdin)))
            .await
            .map_err(|_| {
                format!(
                    "MCP server '{}' startup timed out after {}s",
                    server.name, server.startup_timeout_secs
                )
            })?
            .map_err(|e| format!("MCP server '{}' initialization failed: {}", server.name, e))?;

        let handle = Arc::new(tokio::sync::Mutex::new(McpClientHandle {
            client,
            _child: child,
            _filter_task: filter_task,
        }));

        // Insert into cache
        {
            let mut clients = self.clients.lock().await;
            clients.insert(server.name.clone(), Arc::clone(&handle));
        }

        Ok(handle)
    }

    fn extract_text_from_content(content: &[rmcp::model::Content]) -> String {
        content
            .iter()
            .filter_map(|c| match &c.raw {
                RawContent::Text(text_content) => Some(text_content.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "Name of the MCP server (e.g. \"codex\")"
                },
                "tool": {
                    "type": "string",
                    "description": "Name of the tool on that server. For codex: use \"codex\" to start a new session (requires {\"prompt\": \"...\"}), or \"codex-reply\" to continue an existing session (requires {\"threadId\": \"...\", \"prompt\": \"...\"})"
                },
                "arguments": {
                    "type": "object",
                    "description": "Arguments object passed to the tool. For the \"codex\" tool: {\"prompt\": \"your task\"} is required. For \"codex-reply\": {\"threadId\": \"...\", \"prompt\": \"follow-up\"} is required."
                }
            },
            "required": ["server", "tool", "arguments"]
        });
        if !self.config.servers.is_empty() {
            let names: Vec<&str> = self
                .config
                .servers
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            schema["properties"]["server"]["enum"] = json!(names);
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

        let server_name = args
            .get("server")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'server' parameter"))?;

        let tool_name = args
            .get("tool")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'tool' parameter"))?;

        let server = match self.resolve_server(server_name) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };

        if !Self::is_tool_allowed(server, tool_name) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Tool '{}' is not in the allowed_tools list for MCP server '{}'. Allowed: {:?}",
                    tool_name, server_name, server.allowed_tools
                )),
            });
        }

        let tool_timeout_secs = server.tool_timeout_secs;

        debug!(
            server = server_name,
            tool = tool_name,
            arguments = %args.get("arguments").unwrap_or(&json!(null)),
            "MCP tool call dispatching"
        );

        let handle = match self.get_or_connect(server).await {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };

        // Build arguments as JsonObject (serde_json::Map<String, Value>)
        let arguments = args.get("arguments").and_then(|v| v.as_object()).cloned();

        let params = CallToolRequestParam {
            name: tool_name.to_string().into(),
            arguments,
        };

        let timeout_duration = std::time::Duration::from_secs(tool_timeout_secs);
        let guard = handle.lock().await;
        let call_result =
            tokio::time::timeout(timeout_duration, guard.client.call_tool(params)).await;

        match call_result {
            Ok(Ok(result)) => {
                let is_error = result.is_error.unwrap_or(false);
                let text = Self::extract_text_from_content(&result.content);
                debug!(
                    server = server_name,
                    tool = tool_name,
                    is_error,
                    output_len = text.len(),
                    "MCP tool call completed"
                );
                let error = if is_error {
                    Some(if text.is_empty() {
                        format!("MCP tool '{}' returned an error", tool_name)
                    } else {
                        format!("MCP tool '{}' error: {}", tool_name, text)
                    })
                } else {
                    None
                };
                Ok(ToolResult {
                    success: !is_error,
                    output: text,
                    error,
                })
            }
            Ok(Err(e)) => {
                warn!(
                    server = server_name,
                    tool = tool_name,
                    error = %e,
                    "MCP tool call failed"
                );
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "MCP tool call '{}' on server '{}' failed: {}",
                        tool_name, server_name, e
                    )),
                })
            }
            Err(_) => {
                warn!(
                    server = server_name,
                    tool = tool_name,
                    timeout_secs = tool_timeout_secs,
                    "MCP tool call timed out"
                );
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "MCP tool call '{}' on server '{}' timed out after {}s",
                        tool_name, server_name, tool_timeout_secs
                    )),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;
    use crate::security::AutonomyLevel;

    fn test_security(level: AutonomyLevel, max_actions_per_hour: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_config() -> McpConfig {
        McpConfig {
            enabled: true,
            servers: vec![
                McpServerConfig {
                    name: "codex".to_string(),
                    command: "codex".to_string(),
                    args: vec!["mcp-server".to_string()],
                    allowed_tools: vec!["codex".to_string(), "codex-reply".to_string()],
                    tool_timeout_secs: 600,
                    startup_timeout_secs: 20,
                    notes: Some("OpenAI Codex coding agent".to_string()),
                },
                McpServerConfig {
                    name: "filesystem".to_string(),
                    command: "mcp-server-fs".to_string(),
                    args: vec![],
                    allowed_tools: vec![],
                    tool_timeout_secs: 120,
                    startup_timeout_secs: 30,
                    notes: None,
                },
            ],
        }
    }

    #[test]
    fn mcp_tool_name() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        assert_eq!(tool.name(), "mcp");
    }

    #[test]
    fn mcp_tool_has_parameters_schema() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("server").is_some());
        assert!(schema["properties"].get("tool").is_some());
        assert!(schema["properties"].get("arguments").is_some());
    }

    #[test]
    fn mcp_tool_requires_server_and_tool() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("server")));
        assert!(required.contains(&json!("tool")));
        assert!(required.contains(&json!("arguments")));
    }

    #[test]
    fn mcp_tool_schema_enumerates_servers() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let server_enum = schema["properties"]["server"]["enum"]
            .as_array()
            .expect("server should have enum");
        assert_eq!(server_enum, &vec![json!("codex"), json!("filesystem")]);
    }

    #[test]
    fn mcp_tool_description_lists_servers() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        assert!(desc.contains("Server: \"codex\""));
        assert!(desc.contains("\"codex-reply\""));
        assert!(desc.contains("OpenAI Codex coding agent"));
        assert!(desc.contains("Server: \"filesystem\""));
        assert!(desc.contains("All tools on this server are available"));
    }

    #[test]
    fn mcp_tool_description_omits_notes_when_none() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        // filesystem server has no notes — should not appear between header and tools line
        assert!(desc.contains("Server: \"filesystem\""));
        assert!(!desc.contains("mcp-server-fs"));
    }

    #[test]
    fn resolve_unknown_server() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let err = tool.resolve_server("nonexistent").unwrap_err();
        assert!(err.contains("Unknown MCP server"));
        assert!(err.contains("codex"));
        assert!(err.contains("filesystem"));
    }

    #[test]
    fn allowed_tools_empty_allows_all() {
        let server = McpServerConfig {
            name: "test".to_string(),
            command: "test".to_string(),
            args: vec![],
            allowed_tools: vec![],
            tool_timeout_secs: 120,
            startup_timeout_secs: 30,
            notes: None,
        };
        assert!(McpTool::is_tool_allowed(&server, "anything"));
        assert!(McpTool::is_tool_allowed(&server, "any_tool_name"));
    }

    #[test]
    fn allowed_tools_restricts_when_non_empty() {
        let server = McpServerConfig {
            name: "test".to_string(),
            command: "test".to_string(),
            args: vec![],
            allowed_tools: vec!["codex".to_string(), "codex-reply".to_string()],
            tool_timeout_secs: 120,
            startup_timeout_secs: 30,
            notes: None,
        };
        assert!(McpTool::is_tool_allowed(&server, "codex"));
        assert!(McpTool::is_tool_allowed(&server, "codex-reply"));
        assert!(!McpTool::is_tool_allowed(&server, "shell"));
        assert!(!McpTool::is_tool_allowed(&server, "file_read"));
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let tool = McpTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());
        let result = tool
            .execute(json!({"server": "codex", "tool": "codex"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_rate_limit() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 0), test_config());
        let result = tool
            .execute(json!({"server": "codex", "tool": "codex"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn execute_rejects_missing_server() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool.execute(json!({"tool": "codex"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_rejects_missing_tool() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool.execute(json!({"server": "codex"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_rejects_unknown_server() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({"server": "nonexistent", "tool": "codex"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown MCP server"));
    }

    #[tokio::test]
    async fn execute_rejects_disallowed_tool() {
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({"server": "codex", "tool": "shell"}))
            .await
            .unwrap();
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(err.contains("not in the allowed_tools list"));
        assert!(err.contains("codex"));
    }

    #[test]
    fn filter_non_standard_notifications() {
        // codex/event — non-standard, should be filtered
        assert!(is_non_standard_notification(
            r#"{"jsonrpc":"2.0","method":"codex/event","params":{"type":"progress"}}"#
        ));
        // custom/anything — non-standard notification, should be filtered
        assert!(is_non_standard_notification(
            r#"{"jsonrpc":"2.0","method":"custom/foo","params":{}}"#
        ));

        // Standard MCP notifications — should NOT be filtered
        assert!(!is_non_standard_notification(
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#
        ));
        assert!(!is_non_standard_notification(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#
        ));
        assert!(!is_non_standard_notification(
            r#"{"jsonrpc":"2.0","method":"$/logTrace","params":{}}"#
        ));

        // JSON-RPC response (has id) — should NOT be filtered
        assert!(!is_non_standard_notification(
            r#"{"jsonrpc":"2.0","id":3,"result":{"content":[]}}"#
        ));

        // JSON-RPC request (has id + method) — should NOT be filtered
        assert!(!is_non_standard_notification(
            r#"{"jsonrpc":"2.0","id":1,"method":"codex/event","params":{}}"#
        ));

        // Malformed / non-JSON — should NOT be filtered (pass through to rmcp)
        assert!(!is_non_standard_notification("not json at all"));
        assert!(!is_non_standard_notification(""));
    }

    #[tokio::test]
    async fn execute_graceful_spawn_failure() {
        let config = McpConfig {
            enabled: true,
            servers: vec![McpServerConfig {
                name: "bad".to_string(),
                command: "/nonexistent/path/to/mcp-server".to_string(),
                args: vec![],
                allowed_tools: vec![],
                tool_timeout_secs: 120,
                startup_timeout_secs: 5,
                notes: None,
            }],
        };
        let tool = McpTool::new(test_security(AutonomyLevel::Full, 100), config);
        let result = tool
            .execute(json!({"server": "bad", "tool": "anything"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Failed to spawn"));
    }
}
