use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

const TRADE_STUDIO_DEFAULT_URL: &str = "http://dev5-studio.lan:8080";
const TRADE_STUDIO_REQUEST_TIMEOUT_SECS: u64 = 15;

pub struct TradeSummaryTool;

impl TradeSummaryTool {
    pub fn new(_workspace_dir: PathBuf) -> Self {
        Self
    }

    fn base_url() -> String {
        std::env::var("TRADE_STUDIO_URL").unwrap_or_else(|_| TRADE_STUDIO_DEFAULT_URL.to_string())
    }
}

#[async_trait]
impl Tool for TradeSummaryTool {
    fn name(&self) -> &str {
        "trade_summary"
    }

    fn description(&self) -> &str {
        "Returns filled orders paired into round-trip trades with P&L calculations for a given \
         date from the trade studio service."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "date": {
                    "type": "string",
                    "description": "Date in YYYY-MM-DD format."
                }
            },
            "required": ["date"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let date = match args.get("date").and_then(|v| v.as_str()) {
            Some(d) => d.to_string(),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing required parameter \"date\".".into()),
                });
            }
        };

        let url = format!("{}/api/v1/trades/summary?date={date}", Self::base_url());

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.trade_summary",
            TRADE_STUDIO_REQUEST_TIMEOUT_SECS,
            10,
        );

        let response = client.get(&url).send().await?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Ok(ToolResult {
                success: false,
                output: body,
                error: Some(format!("Trade studio API returned status {status}")),
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

    #[test]
    fn tool_name() {
        let tool = TradeSummaryTool::new(PathBuf::from("/tmp"));
        assert_eq!(tool.name(), "trade_summary");
    }

    #[test]
    fn schema_date_required() {
        let tool = TradeSummaryTool::new(PathBuf::from("/tmp"));
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::Value::String("date".into())));
    }

    #[tokio::test]
    async fn execute_builds_url_without_date() {
        let tool = TradeSummaryTool::new(PathBuf::from("/tmp"));
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap()
            .contains("Missing required parameter \"date\""));
    }
}
