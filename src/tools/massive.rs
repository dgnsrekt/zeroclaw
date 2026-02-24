use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

const MASSIVE_BASE_URL: &str = "https://api.massive.com";
const MASSIVE_REQUEST_TIMEOUT_SECS: u64 = 15;

pub struct MassiveMarketStatusTool {
    workspace_dir: PathBuf,
}

impl MassiveMarketStatusTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }

    fn parse_env_value(raw: &str) -> String {
        let raw = raw.trim();

        let unquoted = if raw.len() >= 2
            && ((raw.starts_with('"') && raw.ends_with('"'))
                || (raw.starts_with('\'') && raw.ends_with('\'')))
        {
            &raw[1..raw.len() - 1]
        } else {
            raw
        };

        unquoted
            .split_once(" #")
            .map_or_else(|| unquoted.trim().to_string(), |(v, _)| v.trim().to_string())
    }

    fn get_api_key(&self) -> anyhow::Result<String> {
        // ~/.zeroclaw/.env is loaded into the process environment at startup
        if let Ok(key) = std::env::var("MASSIVE_API_KEY") {
            if !key.is_empty() {
                return Ok(key);
            }
        }

        // Fall back to workspace .env
        let env_path = self.workspace_dir.join(".env");
        let content = std::fs::read_to_string(&env_path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", env_path.display(), e))?;

        for line in content.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
            if let Some((key, value)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("MASSIVE_API_KEY") {
                    let v = Self::parse_env_value(value);
                    if !v.is_empty() {
                        return Ok(v);
                    }
                }
            }
        }

        anyhow::bail!("MASSIVE_API_KEY not set. Add it to ~/.zeroclaw/.env or workspace .env")
    }
}

#[async_trait]
impl Tool for MassiveMarketStatusTool {
    fn name(&self) -> &str {
        "massive_market_status"
    }

    fn description(&self) -> &str {
        "Query Massive.com for US stock market status. \
         Use query=\"now\" to get current open/closed/extended-hours status per exchange. \
         Use query=\"upcoming\" to get upcoming market holidays and early-close dates. \
         Requires MASSIVE_API_KEY in the workspace .env file."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "enum": ["now", "upcoming"],
                    "description": "\"now\" = current market status; \"upcoming\" = upcoming holidays and early closes"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = match args.get("query").and_then(|v| v.as_str()) {
            Some("now") => "now",
            Some("upcoming") => "upcoming",
            Some(other) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Invalid query \"{other}\". Expected \"now\" or \"upcoming\"."
                    )),
                });
            }
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required parameter \"query\".".into()),
                });
            }
        };

        let api_key = match self.get_api_key() {
            Ok(k) => k,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "{e}. Add MASSIVE_API_KEY=<key> to your workspace .env file."
                    )),
                });
            }
        };

        let url = format!("{MASSIVE_BASE_URL}/v1/marketstatus/{query}");

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.massive",
            MASSIVE_REQUEST_TIMEOUT_SECS,
            10,
        );

        let response = client
            .get(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Ok(ToolResult {
                success: false,
                output: body,
                error: Some(format!("Massive API returned status {status}")),
            });
        }

        Ok(ToolResult {
            success: true,
            output: body,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn tool_name() {
        let tool = MassiveMarketStatusTool::new(PathBuf::from("/tmp"));
        assert_eq!(tool.name(), "massive_market_status");
    }

    #[test]
    fn tool_has_description() {
        let tool = MassiveMarketStatusTool::new(PathBuf::from("/tmp"));
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn schema_requires_query() {
        let tool = MassiveMarketStatusTool::new(PathBuf::from("/tmp"));
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::Value::String("query".into())));
    }

    #[test]
    fn api_key_parsed_from_env_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".env"), "MASSIVE_API_KEY=testkey123\n").unwrap();
        let tool = MassiveMarketStatusTool::new(tmp.path().to_path_buf());
        assert_eq!(tool.get_api_key().unwrap(), "testkey123");
    }

    #[test]
    fn api_key_fails_when_env_missing() {
        let tmp = TempDir::new().unwrap();
        let tool = MassiveMarketStatusTool::new(tmp.path().to_path_buf());
        assert!(tool.get_api_key().is_err());
    }

    #[test]
    fn api_key_fails_when_key_absent() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".env"), "OTHER_KEY=something\n").unwrap();
        let tool = MassiveMarketStatusTool::new(tmp.path().to_path_buf());
        assert!(tool.get_api_key().is_err());
    }

    #[test]
    fn api_key_supports_quoted_and_export() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join(".env"),
            "export MASSIVE_API_KEY=\"quotedkey\"\n",
        )
        .unwrap();
        let tool = MassiveMarketStatusTool::new(tmp.path().to_path_buf());
        assert_eq!(tool.get_api_key().unwrap(), "quotedkey");
    }

    #[tokio::test]
    async fn execute_rejects_invalid_query() {
        let tmp = TempDir::new().unwrap();
        let tool = MassiveMarketStatusTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(json!({"query": "invalid"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid query"));
    }

    #[tokio::test]
    async fn execute_requires_query_param() {
        let tmp = TempDir::new().unwrap();
        let tool = MassiveMarketStatusTool::new(tmp.path().to_path_buf());
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Missing required parameter"));
    }

    #[tokio::test]
    async fn execute_fails_with_missing_api_key() {
        let tmp = TempDir::new().unwrap();
        let tool = MassiveMarketStatusTool::new(tmp.path().to_path_buf());
        let result = tool.execute(json!({"query": "now"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("MASSIVE_API_KEY"));
    }
}
