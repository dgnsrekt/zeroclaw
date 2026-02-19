use super::traits::{Tool, ToolResult};
use crate::config::RssFeedConfig;
use async_trait::async_trait;
use regex::Regex;
use serde_json::json;
use std::fmt::Write as _;

pub struct RssFeedTool {
    config: RssFeedConfig,
    description: String,
}

impl RssFeedTool {
    pub fn new(config: RssFeedConfig) -> Self {
        let description = Self::build_description(&config);
        Self {
            config,
            description,
        }
    }

    fn build_description(config: &RssFeedConfig) -> String {
        let mut desc = String::from("Fetch and read RSS/Atom feeds. Returns recent items with titles, links, dates, and descriptions.");
        if config.feeds.is_empty() {
            return desc;
        }
        desc.push_str(" Available feeds:");
        for feed in &config.feeds {
            let _ = write!(desc, "\n- \"{}\" ({})", feed.name, feed.url);
            if let Some(ref notes) = feed.notes {
                let _ = write!(desc, " — {}", notes);
            }
        }
        desc
    }

    fn resolve_feed(&self, name: &str) -> Result<&crate::config::RssFeedEntry, String> {
        self.config
            .feeds
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| {
                let available: Vec<&str> =
                    self.config.feeds.iter().map(|f| f.name.as_str()).collect();
                format!("Unknown feed '{}'. Available feeds: {:?}", name, available)
            })
    }

    async fn fetch_feed(
        &self,
        feed: &crate::config::RssFeedEntry,
        max_items: usize,
    ) -> anyhow::Result<ToolResult> {
        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.rss_feed",
            self.config.timeout_secs,
            self.config.connect_timeout_secs,
        );

        let response = client.get(&feed.url).send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Feed '{}' returned HTTP {}", feed.name, status)),
            });
        }

        let items = parse_rss_items(&body, max_items);
        if items.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("Feed '{}' returned no items.", feed.name),
                error: None,
            });
        }

        let mut output = format!("Feed: {} ({})\n", feed.name, feed.url);
        let _ = writeln!(
            output,
            "Fetched at: {} UTC\n",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
        );

        for (i, item) in items.iter().enumerate() {
            let _ = writeln!(output, "{}. {}", i + 1, item.title);
            if !item.link.is_empty() {
                let _ = writeln!(output, "   Link: {}", item.link);
            }
            if !item.pub_date.is_empty() {
                let _ = writeln!(output, "   Date: {}", item.pub_date);
            }
            if !item.description.is_empty() {
                let _ = writeln!(output, "   {}", item.description);
            }
        }

        Ok(ToolResult {
            success: true,
            output: output.trim_end().to_string(),
            error: None,
        })
    }
}

struct RssItem {
    title: String,
    link: String,
    pub_date: String,
    description: String,
}

fn strip_cdata(s: &str) -> String {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix("<![CDATA[") {
        if let Some(inner) = inner.strip_suffix("]]>") {
            return inner.to_string();
        }
    }
    s.to_string()
}

fn strip_tags(content: &str) -> String {
    let re = Regex::new(r"<[^>]+>").unwrap();
    re.replace_all(content, "").to_string()
}

fn truncate_description(s: &str, max_len: usize) -> String {
    let s = s.trim();
    if s.len() <= max_len {
        return s.to_string();
    }
    let mut end = max_len;
    // Try to break at a word boundary
    if let Some(pos) = s[..max_len].rfind(' ') {
        end = pos;
    }
    format!("{}...", &s[..end])
}

fn extract_tag_content<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let start_idx = xml.find(&open)?;
    // Find the end of the opening tag (handle attributes)
    let after_open = start_idx + open.len();
    let tag_end = xml[after_open..].find('>')? + after_open + 1;
    let close_idx = xml[tag_end..].find(&close)? + tag_end;
    Some(&xml[tag_end..close_idx])
}

fn parse_rss_items(xml: &str, max_items: usize) -> Vec<RssItem> {
    let mut items = Vec::new();

    // Try RSS 2.0 <item> elements first
    let item_re = Regex::new(r"(?s)<item[^>]*>(.*?)</item>").unwrap();
    let matches: Vec<_> = item_re.captures_iter(xml).collect();

    if !matches.is_empty() {
        for caps in matches.iter().take(max_items) {
            let block = &caps[1];
            items.push(parse_item_block(block));
        }
        return items;
    }

    // Fall back to Atom <entry> elements
    let entry_re = Regex::new(r"(?s)<entry[^>]*>(.*?)</entry>").unwrap();
    for caps in entry_re.captures_iter(xml).take(max_items) {
        let block = &caps[1];
        items.push(parse_atom_entry_block(block));
    }

    items
}

fn parse_item_block(block: &str) -> RssItem {
    let title = extract_tag_content(block, "title")
        .map(strip_cdata)
        .map(|s| strip_tags(&s))
        .unwrap_or_default();

    let link = extract_tag_content(block, "link")
        .map(strip_cdata)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let pub_date = extract_tag_content(block, "pubDate")
        .or_else(|| extract_tag_content(block, "dc:date"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let description = extract_tag_content(block, "description")
        .map(strip_cdata)
        .map(|s| strip_tags(&s))
        .map(|s| truncate_description(&s, 200))
        .unwrap_or_default();

    RssItem {
        title,
        link,
        pub_date,
        description,
    }
}

fn parse_atom_entry_block(block: &str) -> RssItem {
    let title = extract_tag_content(block, "title")
        .map(strip_cdata)
        .map(|s| strip_tags(&s))
        .unwrap_or_default();

    // Atom uses <link href="..." /> — extract href attribute
    let link = extract_atom_link(block).unwrap_or_default();

    let pub_date = extract_tag_content(block, "updated")
        .or_else(|| extract_tag_content(block, "published"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let description = extract_tag_content(block, "summary")
        .or_else(|| extract_tag_content(block, "content"))
        .map(strip_cdata)
        .map(|s| strip_tags(&s))
        .map(|s| truncate_description(&s, 200))
        .unwrap_or_default();

    RssItem {
        title,
        link,
        pub_date,
        description,
    }
}

fn extract_atom_link(block: &str) -> Option<String> {
    // Match <link ... href="..." ... /> or <link ... href="..." ...>
    let re = Regex::new(r#"<link[^>]*\bhref="([^"]+)"[^>]*/?\s*>"#).unwrap();
    // Prefer alternate link, fall back to first link
    let mut first_href = None;
    for caps in re.captures_iter(block) {
        let href = caps[1].to_string();
        // Check if this is rel="alternate"
        let full_match = &caps[0];
        if full_match.contains(r#"rel="alternate""#) {
            return Some(href);
        }
        if first_href.is_none() {
            first_href = Some(href);
        }
    }
    first_href
}

#[async_trait]
impl Tool for RssFeedTool {
    fn name(&self) -> &str {
        "rss_feed"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "feed": {
                    "type": "string",
                    "description": "Name of the RSS feed to fetch"
                },
                "max_items": {
                    "type": "integer",
                    "description": "Maximum number of items to return (1-50, default from config)",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["feed"]
        });
        if !self.config.feeds.is_empty() {
            let names: Vec<&str> = self.config.feeds.iter().map(|f| f.name.as_str()).collect();
            schema["properties"]["feed"]["enum"] = json!(names);
        }
        schema
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let feed_name = args
            .get("feed")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'feed' parameter"))?;

        let feed = match self.resolve_feed(feed_name) {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e),
                });
            }
        };

        // Three-level max_items resolution: call param -> per-feed config -> global config
        let max_items = args
            .get("max_items")
            .and_then(|v| v.as_u64())
            .map(|v| usize::try_from(v).unwrap_or(50).clamp(1, 50))
            .or(feed.max_items)
            .unwrap_or(self.config.max_items)
            .clamp(1, 50);

        self.fetch_feed(feed, max_items).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RssFeedConfig, RssFeedEntry};

    fn test_config() -> RssFeedConfig {
        RssFeedConfig {
            enabled: true,
            timeout_secs: 30,
            connect_timeout_secs: 10,
            max_items: 10,
            feeds: vec![
                RssFeedEntry {
                    name: "lmstudio".to_string(),
                    url: "https://lmstudio.ai/rss.xml".to_string(),
                    notes: Some("LM Studio blog posts".to_string()),
                    max_items: None,
                },
                RssFeedEntry {
                    name: "rust_blog".to_string(),
                    url: "https://blog.rust-lang.org/feed.xml".to_string(),
                    notes: None,
                    max_items: Some(5),
                },
            ],
        }
    }

    #[test]
    fn tool_name() {
        let tool = RssFeedTool::new(test_config());
        assert_eq!(tool.name(), "rss_feed");
    }

    #[test]
    fn tool_has_parameters_schema() {
        let tool = RssFeedTool::new(test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("feed").is_some());
        assert!(schema["properties"].get("max_items").is_some());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("feed")));
    }

    #[test]
    fn schema_enumerates_feed_names() {
        let tool = RssFeedTool::new(test_config());
        let schema = tool.parameters_schema();
        let feed_enum = schema["properties"]["feed"]["enum"]
            .as_array()
            .expect("feed should have enum");
        assert_eq!(feed_enum, &vec![json!("lmstudio"), json!("rust_blog")]);
    }

    #[test]
    fn description_lists_feeds_with_notes() {
        let tool = RssFeedTool::new(test_config());
        let desc = tool.description();
        assert!(desc.contains("\"lmstudio\""));
        assert!(desc.contains("https://lmstudio.ai/rss.xml"));
        assert!(desc.contains("LM Studio blog posts"));
        assert!(desc.contains("\"rust_blog\""));
        assert!(desc.contains("https://blog.rust-lang.org/feed.xml"));
    }

    #[test]
    fn description_omits_notes_when_none() {
        let tool = RssFeedTool::new(test_config());
        let desc = tool.description();
        let rust_line = desc.lines().find(|l| l.contains("\"rust_blog\"")).unwrap();
        assert!(!rust_line.contains(" — "));
    }

    #[test]
    fn resolve_unknown_feed() {
        let tool = RssFeedTool::new(test_config());
        let err = tool.resolve_feed("nonexistent").unwrap_err();
        assert!(err.contains("Unknown feed 'nonexistent'"));
        assert!(err.contains("lmstudio"));
        assert!(err.contains("rust_blog"));
    }

    #[test]
    fn parse_rss2_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Test Feed</title>
    <item>
      <title>First Post</title>
      <link>https://example.com/first</link>
      <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
      <description>This is the first post description.</description>
    </item>
    <item>
      <title>Second Post</title>
      <link>https://example.com/second</link>
      <pubDate>Tue, 02 Jan 2024 00:00:00 GMT</pubDate>
      <description>This is the second post.</description>
    </item>
  </channel>
</rss>"#;

        let items = parse_rss_items(xml, 10);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "First Post");
        assert_eq!(items[0].link, "https://example.com/first");
        assert!(items[0].pub_date.contains("01 Jan 2024"));
        assert_eq!(items[0].description, "This is the first post description.");
        assert_eq!(items[1].title, "Second Post");
    }

    #[test]
    fn parse_atom_xml() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Test Atom Feed</title>
  <entry>
    <title>Atom Entry One</title>
    <link href="https://example.com/atom-one" rel="alternate" />
    <updated>2024-01-15T10:00:00Z</updated>
    <summary>Summary of atom entry one.</summary>
  </entry>
  <entry>
    <title>Atom Entry Two</title>
    <link href="https://example.com/atom-two" />
    <published>2024-01-16T10:00:00Z</published>
    <content type="html">Content of atom entry two.</content>
  </entry>
</feed>"#;

        let items = parse_rss_items(xml, 10);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Atom Entry One");
        assert_eq!(items[0].link, "https://example.com/atom-one");
        assert!(items[0].pub_date.contains("2024-01-15"));
        assert_eq!(items[0].description, "Summary of atom entry one.");
        assert_eq!(items[1].title, "Atom Entry Two");
        assert_eq!(items[1].link, "https://example.com/atom-two");
        assert_eq!(items[1].description, "Content of atom entry two.");
    }

    #[test]
    fn parse_empty_feed() {
        let xml = r#"<?xml version="1.0"?><rss><channel><title>Empty</title></channel></rss>"#;
        let items = parse_rss_items(xml, 10);
        assert!(items.is_empty());
    }

    #[test]
    fn max_items_limits_results() {
        let xml = r#"<rss><channel>
            <item><title>A</title><link>https://a.com</link></item>
            <item><title>B</title><link>https://b.com</link></item>
            <item><title>C</title><link>https://c.com</link></item>
            <item><title>D</title><link>https://d.com</link></item>
        </channel></rss>"#;
        let items = parse_rss_items(xml, 2);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "A");
        assert_eq!(items[1].title, "B");
    }

    #[test]
    fn strip_cdata_unwraps() {
        assert_eq!(strip_cdata("<![CDATA[Hello World]]>"), "Hello World");
        assert_eq!(strip_cdata("plain text"), "plain text");
        assert_eq!(strip_cdata("  <![CDATA[trimmed]]>  "), "trimmed");
    }

    #[test]
    fn strip_tags_removes_html() {
        assert_eq!(strip_tags("<p>Hello <b>World</b></p>"), "Hello World");
        assert_eq!(strip_tags("no tags"), "no tags");
    }

    #[test]
    fn truncation_at_200_chars() {
        let long_text = "a ".repeat(150); // 300 chars
        let truncated = truncate_description(&long_text, 200);
        assert!(truncated.len() <= 204); // 200 + "..."
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn truncation_preserves_short_text() {
        let short = "Short description.";
        assert_eq!(truncate_description(short, 200), short);
    }

    #[test]
    fn parse_cdata_in_rss_items() {
        let xml = r#"<rss><channel>
            <item>
                <title><![CDATA[CDATA Title]]></title>
                <link>https://example.com</link>
                <description><![CDATA[<p>HTML inside CDATA</p>]]></description>
            </item>
        </channel></rss>"#;
        let items = parse_rss_items(xml, 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "CDATA Title");
        assert_eq!(items[0].description, "HTML inside CDATA");
    }

    #[tokio::test]
    async fn execute_rejects_unknown_feed() {
        let tool = RssFeedTool::new(test_config());
        let result = tool.execute(json!({"feed": "nonexistent"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown feed"));
    }

    #[tokio::test]
    async fn execute_rejects_missing_feed() {
        let tool = RssFeedTool::new(test_config());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }
}
