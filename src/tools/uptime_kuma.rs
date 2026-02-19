use super::traits::{Tool, ToolResult};
use crate::config::UptimeKumaConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::sync::Arc;

pub struct UptimeKumaTool {
    security: Arc<SecurityPolicy>,
    config: UptimeKumaConfig,
    description: String,
}

impl UptimeKumaTool {
    pub fn new(security: Arc<SecurityPolicy>, config: UptimeKumaConfig) -> Self {
        let description = Self::build_description(&config);
        Self {
            security,
            config,
            description,
        }
    }

    fn build_description(config: &UptimeKumaConfig) -> String {
        let mut desc =
            String::from("Query Uptime Kuma status pages or push heartbeats to monitors.");
        if config.targets.is_empty() {
            return desc;
        }
        desc.push_str(" Available targets:");
        for target in &config.targets {
            let _ = write!(
                desc,
                "\n- \"{}\" ({}, slug: {})",
                target.name, target.base_url, target.slug
            );
            if let Some(ref notes) = target.notes {
                let _ = write!(desc, " — {}", notes);
            }
        }
        desc
    }

    fn resolve_target(&self, name: &str) -> Result<&crate::config::UptimeKumaTarget, String> {
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
                    "Unknown uptime_kuma target '{}'. Available targets: {:?}",
                    name, available
                )
            })
    }

    async fn execute_status(
        &self,
        target: &crate::config::UptimeKumaTarget,
    ) -> anyhow::Result<ToolResult> {
        let url = format!(
            "{}/api/status-page/{}",
            target.base_url.trim_end_matches('/'),
            target.slug
        );

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.uptime_kuma",
            self.config.timeout_secs,
            self.config.connect_timeout_secs,
        );

        let response = client.get(&url).send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            Ok(ToolResult {
                success: true,
                output: format_status_response(&body),
                error: None,
            })
        } else {
            Ok(ToolResult {
                success: false,
                output: body,
                error: Some(format!("Uptime Kuma API returned status {}", status)),
            })
        }
    }

    async fn execute_push(
        &self,
        target: &crate::config::UptimeKumaTarget,
        push_token: &str,
        push_status: &str,
        push_message: Option<&str>,
        push_ping: Option<&str>,
    ) -> anyhow::Result<ToolResult> {
        let mut url = format!(
            "{}/api/push/{}?status={}",
            target.base_url.trim_end_matches('/'),
            push_token,
            push_status
        );

        if let Some(msg) = push_message {
            let _ = write!(url, "&msg={}", urlencoding::encode(msg));
        }

        if let Some(ping) = push_ping {
            let _ = write!(url, "&ping={}", urlencoding::encode(ping));
        }

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.uptime_kuma",
            self.config.timeout_secs,
            self.config.connect_timeout_secs,
        );

        let response = client.get(&url).send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.is_success() {
            Ok(ToolResult {
                success: true,
                output: format!("Heartbeat pushed successfully. Response: {}", body),
                error: None,
            })
        } else {
            Ok(ToolResult {
                success: false,
                output: body,
                error: Some(format!("Uptime Kuma push API returned status {}", status)),
            })
        }
    }
}

fn format_status_response(body: &str) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return format!("Raw response:\n{}", body),
    };

    let mut output = String::new();

    // Parse heartbeatList: { "monitor_id": [ { status, msg, ping, ... }, ... ] }
    if let Some(heartbeat_list) = parsed.get("heartbeatList").and_then(|v| v.as_object()) {
        let _ = writeln!(output, "=== Monitor Status ===");
        for (monitor_id, beats) in heartbeat_list {
            if let Some(latest) = beats.as_array().and_then(|a| a.last()) {
                let status_code = latest.get("status").and_then(|v| v.as_i64()).unwrap_or(-1);
                let status_label = match status_code {
                    0 => "DOWN",
                    1 => "UP",
                    2 => "PENDING",
                    3 => "MAINTENANCE",
                    _ => "UNKNOWN",
                };
                let msg = latest.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                let ping = latest.get("ping").and_then(|v| v.as_i64());

                let _ = write!(output, "\n[{}] Monitor {}", status_label, monitor_id);
                if !msg.is_empty() {
                    let _ = write!(output, " — {}", msg);
                }
                if let Some(p) = ping {
                    let _ = write!(output, " ({}ms)", p);
                }
            }
        }
    }

    // Parse uptimeList: { "monitor_id_24": 0.99, "monitor_id_720": 0.98 }
    if let Some(uptime_list) = parsed.get("uptimeList").and_then(|v| v.as_object()) {
        if !uptime_list.is_empty() {
            let _ = write!(output, "\n\n=== Uptime ===");
            for (key, value) in uptime_list {
                let pct = value.as_f64().unwrap_or(0.0) * 100.0;
                let _ = write!(output, "\n  {}: {:.2}%", key, pct);
            }
        }
    }

    if output.is_empty() {
        format!("Raw response:\n{}", body)
    } else {
        output
    }
}

#[async_trait]
impl Tool for UptimeKumaTool {
    fn name(&self) -> &str {
        "uptime_kuma"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The action to perform",
                    "enum": ["status", "push"]
                },
                "host": {
                    "type": "string",
                    "description": "Name of the Uptime Kuma target from config"
                },
                "push_token": {
                    "type": "string",
                    "description": "Push token for the monitor (required for push action)"
                },
                "push_status": {
                    "type": "string",
                    "description": "Status to push: 'up' or 'down' (default: 'up')",
                    "enum": ["up", "down"]
                },
                "push_message": {
                    "type": "string",
                    "description": "Optional message to include with the push heartbeat"
                },
                "push_ping": {
                    "type": "string",
                    "description": "Optional response time in milliseconds"
                }
            },
            "required": ["action", "host"]
        });
        if !self.config.targets.is_empty() {
            let names: Vec<&str> = self
                .config
                .targets
                .iter()
                .map(|t| t.name.as_str())
                .collect();
            schema["properties"]["host"]["enum"] = json!(names);
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

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;

        let host = args
            .get("host")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'host' parameter"))?;

        let target = match self.resolve_target(host) {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };

        match action {
            "status" => self.execute_status(target).await,
            "push" => {
                let push_token = match args.get("push_token").and_then(|v| v.as_str()) {
                    Some(t) if !t.trim().is_empty() => t.trim(),
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Missing 'push_token' parameter (required for push action)".into(),
                            ),
                        });
                    }
                };

                let push_status = args
                    .get("push_status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("up");

                if push_status != "up" && push_status != "down" {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Invalid 'push_status': '{}'. Must be 'up' or 'down'",
                            push_status
                        )),
                    });
                }

                let push_message = args.get("push_message").and_then(|v| v.as_str());
                let push_ping = args.get("push_ping").and_then(|v| v.as_str());

                self.execute_push(target, push_token, push_status, push_message, push_ping)
                    .await
            }
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{}'. Must be 'status' or 'push'",
                    action
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UptimeKumaTarget;
    use crate::security::AutonomyLevel;

    fn test_security(level: AutonomyLevel, max_actions_per_hour: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_config() -> UptimeKumaConfig {
        UptimeKumaConfig {
            enabled: true,
            timeout_secs: 30,
            connect_timeout_secs: 10,
            targets: vec![
                UptimeKumaTarget {
                    name: "cerberus_gamma".to_string(),
                    base_url: "http://dev1-oryx.lan:3002".to_string(),
                    slug: "cerberus-gamma".to_string(),
                    notes: Some("Primary infrastructure".to_string()),
                },
                UptimeKumaTarget {
                    name: "xscraper".to_string(),
                    base_url: "http://dev2-mini.lan:3001".to_string(),
                    slug: "xscraper".to_string(),
                    notes: None,
                },
            ],
        }
    }

    #[test]
    fn uptime_kuma_tool_name() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        assert_eq!(tool.name(), "uptime_kuma");
    }

    #[test]
    fn uptime_kuma_tool_has_parameters_schema() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("action").is_some());
        assert!(schema["properties"].get("host").is_some());
        assert!(schema["properties"].get("push_token").is_some());
        assert!(schema["properties"].get("push_status").is_some());
        assert!(schema["properties"].get("push_message").is_some());
        assert!(schema["properties"].get("push_ping").is_some());
    }

    #[test]
    fn uptime_kuma_tool_requires_action_and_host() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("action")));
        assert!(required.contains(&json!("host")));
    }

    #[test]
    fn uptime_kuma_tool_description_lists_targets() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        assert!(desc.contains("\"cerberus_gamma\""));
        assert!(desc.contains("http://dev1-oryx.lan:3002"));
        assert!(desc.contains("cerberus-gamma"));
        assert!(desc.contains("Primary infrastructure"));
        assert!(desc.contains("\"xscraper\""));
        assert!(desc.contains("http://dev2-mini.lan:3001"));
        assert!(desc.contains("slug: xscraper"));
    }

    #[test]
    fn uptime_kuma_tool_description_omits_notes_when_none() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        let xscraper_line = desc.lines().find(|l| l.contains("\"xscraper\"")).unwrap();
        assert!(!xscraper_line.contains(" — "));
    }

    #[test]
    fn uptime_kuma_tool_schema_enumerates_hosts() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let host_enum = schema["properties"]["host"]["enum"]
            .as_array()
            .expect("host should have enum");
        assert_eq!(host_enum, &vec![json!("cerberus_gamma"), json!("xscraper")]);
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());

        let result = tool
            .execute(json!({"action": "status", "host": "cerberus_gamma"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_rate_limit() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 0), test_config());

        let result = tool
            .execute(json!({"action": "status", "host": "cerberus_gamma"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn execute_rejects_unknown_host() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({"action": "status", "host": "nonexistent"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown uptime_kuma target"));
    }

    #[tokio::test]
    async fn execute_rejects_unknown_action() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({"action": "delete", "host": "cerberus_gamma"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn push_rejects_missing_token() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({"action": "push", "host": "cerberus_gamma"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("push_token"));
    }

    #[tokio::test]
    async fn push_rejects_invalid_status() {
        let tool = UptimeKumaTool::new(test_security(AutonomyLevel::Full, 100), test_config());

        let result = tool
            .execute(json!({
                "action": "push",
                "host": "cerberus_gamma",
                "push_token": "abc123",
                "push_status": "maybe"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Must be 'up' or 'down'"));
    }

    #[test]
    fn format_status_response_parses_json() {
        let body = json!({
            "heartbeatList": {
                "1": [
                    {"status": 1, "msg": "200 - OK", "ping": 42}
                ],
                "2": [
                    {"status": 0, "msg": "Connection refused", "ping": null}
                ]
            },
            "uptimeList": {
                "1_24": 0.998,
                "1_720": 0.995
            }
        })
        .to_string();

        let output = format_status_response(&body);
        assert!(output.contains("[UP]"));
        assert!(output.contains("[DOWN]"));
        assert!(output.contains("200 - OK"));
        assert!(output.contains("42ms"));
        assert!(output.contains("Connection refused"));
        assert!(output.contains("99.80%"));
        assert!(output.contains("99.50%"));
    }

    #[test]
    fn format_status_response_handles_invalid_json() {
        let output = format_status_response("not json at all");
        assert!(output.contains("Raw response:"));
        assert!(output.contains("not json at all"));
    }

    #[test]
    fn format_status_response_handles_empty_heartbeats() {
        let body = json!({
            "heartbeatList": {},
            "uptimeList": {}
        })
        .to_string();

        let output = format_status_response(&body);
        assert!(output.contains("Monitor Status"));
    }
}
