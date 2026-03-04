use crate::config::{
    schema::{SseWatcherFeedConfig, SseWatcherHandlerConfig},
    Config,
};
use crate::memory::MemoryCategory;
use anyhow::Result;
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

const SSE_WATCHER_COMPONENT: &str = "sse_watcher";

pub async fn run(config: Config) -> Result<()> {
    crate::health::mark_component_ok(SSE_WATCHER_COMPONENT);
    info!(
        "SSE watcher starting: {} feed(s)",
        config.sse_watcher.feeds.len()
    );

    let handles: Vec<_> = config
        .sse_watcher
        .feeds
        .iter()
        .map(|feed| {
            let cfg = config.clone();
            let feed = feed.clone();
            #[allow(clippy::large_futures)]
            tokio::spawn(async move {
                run_feed_watcher(cfg, feed).await;
            })
        })
        .collect();

    futures_util::future::join_all(handles).await;
    Ok(())
}

async fn run_feed_watcher(config: Config, feed: SseWatcherFeedConfig) {
    let base_delay = feed.reconnect_delay_secs;
    let mut delay = base_delay;
    loop {
        info!(feed = %feed.name, url = %feed.url, "SSE feed connecting");
        match connect_and_process(&config, &feed).await {
            Ok(()) => {
                info!(feed = %feed.name, "SSE stream ended cleanly, reconnecting");
                delay = base_delay;
            }
            Err(e) => {
                warn!(
                    feed = %feed.name,
                    error = %e,
                    delay_secs = delay,
                    "SSE feed error, reconnecting"
                );
                delay = (delay * 2).min(60);
            }
        }
        sleep(Duration::from_secs(delay)).await;
    }
}

async fn connect_and_process(config: &Config, feed: &SseWatcherFeedConfig) -> Result<()> {
    let client = reqwest::Client::new();
    let response = client
        .get(&feed.url)
        .header("Accept", "text/event-stream")
        .send()
        .await?;

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut current_data_lines: Vec<String> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        let text = String::from_utf8_lossy(&bytes);
        buffer.push_str(&text);

        while let Some(newline_pos) = buffer.find('\n') {
            let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
            buffer.drain(..=newline_pos);

            if line.is_empty() {
                if !current_data_lines.is_empty() {
                    let data = current_data_lines.join("\n");
                    current_data_lines.clear();
                    handle_event(config, feed, data);
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                current_data_lines.push(rest.trim_start().to_string());
            }
            // Ignore event:, id:, retry:, comment lines
        }
    }

    Ok(())
}

fn handle_event(config: &Config, feed: &SseWatcherFeedConfig, data: String) {
    let json: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(e) => {
            warn!(feed = %feed.name, error = %e, "SSE event JSON parse error");
            return;
        }
    };

    let msg_type = json
        .pointer("/text/content/m")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let payload = match json.pointer("/text/content/p") {
        Some(p) => p.clone(),
        None => {
            warn!(feed = %feed.name, "SSE event missing /text/content/p");
            return;
        }
    };

    let message = payload
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let raw_symbol = payload
        .get("symbol")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let symbol = decode_symbol(&raw_symbol);

    for handler in &feed.handlers {
        if handler_matches(handler, &msg_type, &symbol, &message) {
            let cfg = config.clone();
            let h = handler.clone();
            let p = payload.clone();
            let msg = message.clone();
            let sym = symbol.clone();
            #[allow(clippy::large_futures)]
            tokio::spawn(async move {
                if let Err(e) = fire_handler(cfg, h, p, msg, sym).await {
                    warn!("SSE handler fire error: {e}");
                }
            });
        }
    }
}

fn handler_matches(
    h: &SseWatcherHandlerConfig,
    msg_type: &str,
    symbol: &str,
    message: &str,
) -> bool {
    // Stage 1: event type allow-list
    if !h
        .event_types
        .iter()
        .any(|t| t.eq_ignore_ascii_case(msg_type))
    {
        return false;
    }

    let symbol_lc = symbol.to_ascii_lowercase();
    let message_lc = message.to_ascii_lowercase();

    // Stage 2: symbol allow/deny
    if !h.match_symbol.is_empty() {
        let allowed = h.match_symbol.iter().any(|s| {
            let s_lc = s.to_ascii_lowercase();
            symbol_lc.contains(&s_lc) || message_lc.contains(&s_lc)
        });
        if !allowed {
            return false;
        }
    }
    if h.ignore_symbol.iter().any(|s| {
        let s_lc = s.to_ascii_lowercase();
        symbol_lc.contains(&s_lc) || message_lc.contains(&s_lc)
    }) {
        return false;
    }

    // Stage 3: message allow/deny
    if !h.match_message.is_empty() {
        let allowed = h
            .match_message
            .iter()
            .any(|s| message_lc.contains(&s.to_ascii_lowercase()));
        if !allowed {
            return false;
        }
    }
    if h.ignore_message
        .iter()
        .any(|s| message_lc.contains(&s.to_ascii_lowercase()))
    {
        return false;
    }

    true
}

async fn fire_handler(
    config: Config,
    handler: SseWatcherHandlerConfig,
    payload: Value,
    message: String,
    symbol: String,
) -> Result<()> {
    let fire_time = payload
        .get("fire_time")
        .or_else(|| payload.get("fired_for_time"))
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();

    let bar_time = payload
        .get("bar_time")
        .map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();

    let alert_id = payload
        .get("plot_id")
        .or_else(|| payload.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let resolution = payload
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let kinds = payload
        .get("kinds")
        .map(|v| v.to_string())
        .unwrap_or_default();

    let agent_msg = format!(
        "[sse_alert handler:{name}]\n\n{prompt}\n\nAlert: {message}\nSymbol: {symbol}\nFired at: {fire_time}\nBar time: {bar_time}\nAlert ID: {alert_id}\nResolution: {resolution}m\nKinds: {kinds}",
        name = handler.name,
        prompt = handler.prompt,
    );

    let session_id = handler
        .session_id
        .as_deref()
        .unwrap_or(&handler.name)
        .to_string();

    let mut cfg = config;
    if let Some(ref model) = handler.model {
        cfg.default_model = Some(model.clone());
    }

    #[allow(clippy::large_futures)]
    let output =
        crate::agent::process_message_with_session(cfg.clone(), &agent_msg, Some(&session_id))
            .await?;

    if crate::cron::scheduler::is_no_reply_sentinel(&output) {
        return Ok(());
    }

    if let (Some(channel), Some(to)) = (
        handler.delivery_channel.as_deref(),
        handler.delivery_to.as_deref(),
    ) {
        crate::cron::scheduler::deliver_announcement(&cfg, channel, to, &output).await?;
    }

    // Store alert for recall via the memory_recall tool.
    // Timestamped entry scoped to handler session; rolling global entry (session_id=None)
    // found by memory_recall across all sessions regardless of who asks.
    if let Ok(mem) =
        crate::memory::create_memory(&cfg.memory, &cfg.workspace_dir, cfg.api_key.as_deref())
    {
        let safe_time = fire_time.replace([':', 'T', 'Z'], "_");
        let content = format!(
            "TradingView alert: {symbol} | Fired: {fire_time} | Bar: {bar_time} | Resolution: {resolution}m | Kinds: {kinds}",
        );
        let _ = mem
            .store(
                &format!("sse_alert_{}_{}", handler.name, safe_time),
                &content,
                MemoryCategory::Conversation,
                Some(&session_id),
            )
            .await;
        let _ = mem
            .store(
                &format!("last_alert_{}", handler.name),
                &content,
                MemoryCategory::Core,
                None,
            )
            .await;
    }

    Ok(())
}

fn decode_symbol(raw: &str) -> String {
    let stripped = raw.trim_start_matches('=');
    if stripped.is_empty() {
        return raw.to_string();
    }
    match serde_json::from_str::<Value>(stripped) {
        Ok(v) => v
            .get("symbol")
            .and_then(Value::as_str)
            .unwrap_or(raw)
            .to_string(),
        Err(_) => raw.to_string(),
    }
}
