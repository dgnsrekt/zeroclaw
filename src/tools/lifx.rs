use super::traits::{Tool, ToolResult};
use crate::config::LifxConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use lifx_core::{BuildOptions, Message, RawMessage, HSBK};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

const LIFX_PORT: u16 = 56700;

pub struct LifxTool {
    security: Arc<SecurityPolicy>,
    config: LifxConfig,
    description: String,
}

impl LifxTool {
    pub fn new(security: Arc<SecurityPolicy>, config: LifxConfig) -> Self {
        let description = String::from(
            "Control LIFX smart lights on the local network via the LIFX LAN protocol. \
             Actions: \"discover\" (find lights), \"state\" (query a light), \
             \"power\" (turn on/off), \"color\" (set color/brightness/temperature). \
             Use discover first to find light IPs, then target them by IP address.",
        );
        Self {
            security,
            config,
            description,
        }
    }

    /// Build and pack a LIFX protocol message for sending.
    fn build_packet(msg: Message, target: Option<u64>) -> anyhow::Result<Vec<u8>> {
        let opts = BuildOptions {
            target,
            res_required: true,
            ..BuildOptions::default()
        };
        let raw = RawMessage::build(&opts, msg)
            .map_err(|e| anyhow::anyhow!("Failed to build LIFX packet: {:?}", e))?;
        raw.pack()
            .map_err(|e| anyhow::anyhow!("Failed to pack LIFX packet: {:?}", e))
    }

    /// Extract the 6-byte MAC target from a RawMessage as a u64.
    fn target_from_raw(raw: &RawMessage) -> u64 {
        raw.frame_addr.target
    }

    /// Format a MAC address from a u64 target value.
    fn format_mac(target: u64) -> String {
        let bytes = target.to_le_bytes();
        format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
        )
    }

    /// Send a broadcast packet and collect all responses until timeout.
    async fn broadcast_and_collect(
        &self,
        packet: &[u8],
    ) -> anyhow::Result<Vec<(RawMessage, SocketAddr)>> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.set_broadcast(true)?;

        let broadcast_addr: SocketAddr =
            format!("{}:{}", self.config.broadcast_addr, LIFX_PORT).parse()?;
        socket.send_to(packet, broadcast_addr).await?;

        let timeout = Duration::from_secs(self.config.timeout_secs);
        let mut buf = [0u8; 1024];
        let mut results = Vec::new();

        while let Ok(Ok((len, addr))) =
            tokio::time::timeout(timeout, socket.recv_from(&mut buf)).await
        {
            if let Ok(raw) = RawMessage::unpack(&buf[..len]) {
                results.push((raw, addr));
            }
        }

        Ok(results)
    }

    /// Send a unicast packet to a specific light and wait for one response.
    async fn send_and_recv(
        &self,
        packet: &[u8],
        target_ip: &str,
    ) -> anyhow::Result<Option<RawMessage>> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        let addr: SocketAddr = format!("{}:{}", target_ip, LIFX_PORT).parse()?;
        socket.send_to(packet, addr).await?;

        let timeout = Duration::from_secs(self.config.timeout_secs);
        let mut buf = [0u8; 1024];

        match tokio::time::timeout(timeout, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _))) => Ok(RawMessage::unpack(&buf[..len]).ok()),
            Ok(Err(e)) => Err(e.into()),
            Err(_) => Ok(None), // timeout
        }
    }

    /// Execute the discover action: broadcast GetService, then follow up with LightGet
    /// for each responding device to get labels and state.
    async fn action_discover(&self) -> anyhow::Result<ToolResult> {
        let packet = Self::build_packet(Message::GetService, None)?;
        let responses = self.broadcast_and_collect(&packet).await?;

        if responses.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No LIFX lights found on the network.".to_string(),
                error: None,
            });
        }

        // Deduplicate by IP address
        let mut seen = std::collections::HashSet::new();
        let mut lights = Vec::new();

        for (raw, addr) in &responses {
            let ip = addr.ip().to_string();
            if !seen.insert(ip.clone()) {
                continue;
            }
            let mac = Self::format_mac(Self::target_from_raw(raw));
            let target = Self::target_from_raw(raw);

            // Follow up with LightGet to get label and state
            let label = match Self::build_packet(Message::LightGet, Some(target)) {
                Ok(pkt) => match self.send_and_recv(&pkt, &ip).await {
                    Ok(Some(resp)) => match Message::from_raw(&resp) {
                        Ok(Message::LightState {
                            label,
                            power,
                            color,
                            ..
                        }) => {
                            let power_str = if power == lifx_core::PowerLevel::Enabled {
                                "on"
                            } else {
                                "off"
                            };
                            Some(format!(
                                "{} (power: {}, hue: {:.0}, sat: {:.0}%, bri: {:.0}%, kelvin: {})",
                                label,
                                power_str,
                                f64::from(color.hue) / 65535.0 * 360.0,
                                f64::from(color.saturation) / 65535.0 * 100.0,
                                f64::from(color.brightness) / 65535.0 * 100.0,
                                color.kelvin,
                            ))
                        }
                        _ => None,
                    },
                    _ => None,
                },
                Err(_) => None,
            };

            let entry = if let Some(info) = label {
                format!("- {} [{}] {}", ip, mac, info)
            } else {
                format!("- {} [{}]", ip, mac)
            };
            lights.push(entry);
        }

        let output = format!(
            "Found {} LIFX light(s):\n{}",
            lights.len(),
            lights.join("\n")
        );
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }

    /// Execute the state action: query a single light's current state.
    async fn action_state(&self, target_ip: &str) -> anyhow::Result<ToolResult> {
        let packet = Self::build_packet(Message::LightGet, None)?;
        let response = self.send_and_recv(&packet, target_ip).await?;

        match response {
            Some(raw) => match Message::from_raw(&raw) {
                Ok(Message::LightState {
                    color,
                    power,
                    label,
                    ..
                }) => {
                    let power_str = if power == lifx_core::PowerLevel::Enabled {
                        "on"
                    } else {
                        "off"
                    };
                    let output = format!(
                        "Light: {}\nPower: {}\nHue: {:.1}\nSaturation: {:.1}%\nBrightness: {:.1}%\nKelvin: {}",
                        label,
                        power_str,
                        f64::from(color.hue) / 65535.0 * 360.0,
                        f64::from(color.saturation) / 65535.0 * 100.0,
                        f64::from(color.brightness) / 65535.0 * 100.0,
                        color.kelvin,
                    );
                    Ok(ToolResult {
                        success: true,
                        output,
                        error: None,
                    })
                }
                Ok(other) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Unexpected response type: {:?}", other)),
                }),
                Err(e) => Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to parse response: {}", e)),
                }),
            },
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No response from light at {}", target_ip)),
            }),
        }
    }

    /// Execute the power action: turn a light on or off.
    async fn action_power(
        &self,
        target_ip: &str,
        power: &str,
        duration_ms: u32,
    ) -> anyhow::Result<ToolResult> {
        let level: u16 = match power {
            "on" => 65535,
            "off" => 0,
            other => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Invalid power value '{}'. Expected 'on' or 'off'",
                        other
                    )),
                });
            }
        };

        let packet = Self::build_packet(
            Message::LightSetPower {
                level,
                duration: duration_ms,
            },
            None,
        )?;
        self.send_and_recv(&packet, target_ip).await?;

        Ok(ToolResult {
            success: true,
            output: format!("Light at {} powered {}", target_ip, power),
            error: None,
        })
    }

    /// Execute the color action: set a light's color.
    async fn action_color(
        &self,
        target_ip: &str,
        hue: f64,
        saturation: f64,
        brightness: f64,
        kelvin: u16,
        duration_ms: u32,
    ) -> anyhow::Result<ToolResult> {
        // Convert human-friendly values to u16 ranges.
        // Values are pre-validated to 0..=360 / 0..=100 so truncation and sign loss are safe.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let hue_u16 = ((hue / 360.0) * 65535.0).round() as u16;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sat_u16 = ((saturation / 100.0) * 65535.0).round() as u16;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bri_u16 = ((brightness / 100.0) * 65535.0).round() as u16;

        let color = HSBK {
            hue: hue_u16,
            saturation: sat_u16,
            brightness: bri_u16,
            kelvin,
        };

        let packet = Self::build_packet(
            Message::LightSetColor {
                reserved: 0,
                color,
                duration: duration_ms,
            },
            None,
        )?;
        self.send_and_recv(&packet, target_ip).await?;

        Ok(ToolResult {
            success: true,
            output: format!(
                "Light at {} set to hue={:.0}, saturation={:.0}%, brightness={:.0}%, kelvin={}",
                target_ip, hue, saturation, brightness, kelvin
            ),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for LifxTool {
    fn name(&self) -> &str {
        "lifx"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["discover", "state", "power", "color"],
                    "description": "Action to perform: discover (find lights), state (query light), power (on/off), color (set color)"
                },
                "target": {
                    "type": "string",
                    "description": "IP address of the target light (required for state/power/color, obtained from discover)"
                },
                "power": {
                    "type": "string",
                    "enum": ["on", "off"],
                    "description": "Power state (for power action)"
                },
                "hue": {
                    "type": "number",
                    "description": "Hue in degrees (0-360, for color action)",
                    "minimum": 0,
                    "maximum": 360
                },
                "saturation": {
                    "type": "number",
                    "description": "Saturation percentage (0-100, for color action)",
                    "minimum": 0,
                    "maximum": 100
                },
                "brightness": {
                    "type": "number",
                    "description": "Brightness percentage (0-100, for color action)",
                    "minimum": 0,
                    "maximum": 100
                },
                "kelvin": {
                    "type": "integer",
                    "description": "Color temperature in kelvin (1500-9000, default 3500, for color action)",
                    "minimum": 1500,
                    "maximum": 9000
                },
                "duration": {
                    "type": "integer",
                    "description": "Transition duration in milliseconds (default 0)",
                    "minimum": 0
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'action' parameter"))?;

        match action {
            "discover" => self.action_discover().await,
            "state" => {
                let target = args.get("target").and_then(|v| v.as_str()).ok_or_else(|| {
                    anyhow::anyhow!("Missing 'target' parameter for state action")
                })?;
                self.action_state(target).await
            }
            "power" => {
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

                let target = args.get("target").and_then(|v| v.as_str()).ok_or_else(|| {
                    anyhow::anyhow!("Missing 'target' parameter for power action")
                })?;
                let power = args
                    .get("power")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'power' parameter for power action"))?;
                #[allow(clippy::cast_possible_truncation)]
                let duration = args.get("duration").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                self.action_power(target, power, duration).await
            }
            "color" => {
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

                let target = args.get("target").and_then(|v| v.as_str()).ok_or_else(|| {
                    anyhow::anyhow!("Missing 'target' parameter for color action")
                })?;
                let hue = args
                    .get("hue")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| anyhow::anyhow!("Missing 'hue' parameter for color action"))?;
                let saturation =
                    args.get("saturation")
                        .and_then(|v| v.as_f64())
                        .ok_or_else(|| {
                            anyhow::anyhow!("Missing 'saturation' parameter for color action")
                        })?;
                let brightness =
                    args.get("brightness")
                        .and_then(|v| v.as_f64())
                        .ok_or_else(|| {
                            anyhow::anyhow!("Missing 'brightness' parameter for color action")
                        })?;
                #[allow(clippy::cast_possible_truncation)]
                let kelvin = args.get("kelvin").and_then(|v| v.as_u64()).unwrap_or(3500) as u16;
                #[allow(clippy::cast_possible_truncation)]
                let duration = args.get("duration").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

                // Validate ranges
                if !(0.0..=360.0).contains(&hue) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Invalid hue {:.1}: must be between 0 and 360", hue)),
                    });
                }
                if !(0.0..=100.0).contains(&saturation) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Invalid saturation {:.1}: must be between 0 and 100",
                            saturation
                        )),
                    });
                }
                if !(0.0..=100.0).contains(&brightness) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Invalid brightness {:.1}: must be between 0 and 100",
                            brightness
                        )),
                    });
                }
                if !(1500..=9000).contains(&kelvin) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Invalid kelvin {}: must be between 1500 and 9000",
                            kelvin
                        )),
                    });
                }

                self.action_color(target, hue, saturation, brightness, kelvin, duration)
                    .await
            }
            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{}'. Valid actions: discover, state, power, color",
                    other
                )),
            }),
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

    fn test_config() -> LifxConfig {
        LifxConfig {
            enabled: true,
            timeout_secs: 3,
            broadcast_addr: "255.255.255.255".to_string(),
        }
    }

    #[test]
    fn lifx_tool_name() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        assert_eq!(tool.name(), "lifx");
    }

    #[test]
    fn lifx_tool_description_mentions_actions() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let desc = tool.description();
        assert!(desc.contains("discover"));
        assert!(desc.contains("state"));
        assert!(desc.contains("power"));
        assert!(desc.contains("color"));
    }

    #[test]
    fn lifx_tool_has_parameters_schema() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("action").is_some());
        assert!(schema["properties"].get("target").is_some());
        assert!(schema["properties"].get("power").is_some());
        assert!(schema["properties"].get("hue").is_some());
        assert!(schema["properties"].get("saturation").is_some());
        assert!(schema["properties"].get("brightness").is_some());
        assert!(schema["properties"].get("kelvin").is_some());
        assert!(schema["properties"].get("duration").is_some());
    }

    #[test]
    fn lifx_tool_requires_action() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("action")));
    }

    #[tokio::test]
    async fn execute_rejects_unknown_action() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool.execute(json!({"action": "dance"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[tokio::test]
    async fn power_blocks_readonly_mode() {
        let tool = LifxTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());
        let result = tool
            .execute(json!({"action": "power", "target": "10.0.0.1", "power": "on"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn color_blocks_readonly_mode() {
        let tool = LifxTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());
        let result = tool
            .execute(json!({
                "action": "color",
                "target": "10.0.0.1",
                "hue": 120,
                "saturation": 100,
                "brightness": 50
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn power_blocks_rate_limit() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 0), test_config());
        let result = tool
            .execute(json!({"action": "power", "target": "10.0.0.1", "power": "on"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn color_blocks_rate_limit() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 0), test_config());
        let result = tool
            .execute(json!({
                "action": "color",
                "target": "10.0.0.1",
                "hue": 120,
                "saturation": 100,
                "brightness": 50
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn state_missing_target_errors() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool.execute(json!({"action": "state"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target"));
    }

    #[tokio::test]
    async fn power_missing_target_errors() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({"action": "power", "power": "on"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target"));
    }

    #[tokio::test]
    async fn power_missing_power_value_errors() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({"action": "power", "target": "10.0.0.1"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("power"));
    }

    #[tokio::test]
    async fn color_missing_hue_errors() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({
                "action": "color",
                "target": "10.0.0.1",
                "saturation": 100,
                "brightness": 50
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("hue"));
    }

    #[tokio::test]
    async fn color_invalid_hue_range() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({
                "action": "color",
                "target": "10.0.0.1",
                "hue": 400,
                "saturation": 50,
                "brightness": 50
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("hue"));
    }

    #[tokio::test]
    async fn color_invalid_saturation_range() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({
                "action": "color",
                "target": "10.0.0.1",
                "hue": 120,
                "saturation": 150,
                "brightness": 50
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("saturation"));
    }

    #[tokio::test]
    async fn color_invalid_brightness_range() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({
                "action": "color",
                "target": "10.0.0.1",
                "hue": 120,
                "saturation": 50,
                "brightness": -10
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("brightness"));
    }

    #[tokio::test]
    async fn color_invalid_kelvin_range() {
        let tool = LifxTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let result = tool
            .execute(json!({
                "action": "color",
                "target": "10.0.0.1",
                "hue": 120,
                "saturation": 50,
                "brightness": 50,
                "kelvin": 500
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("kelvin"));
    }

    #[tokio::test]
    async fn discover_readonly_allowed() {
        // Discover is read-only and should work even in ReadOnly mode.
        // It will fail at the network level (no real lights), but should not
        // be blocked by security policy.
        let tool = LifxTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());
        let result = tool.execute(json!({"action": "discover"})).await;
        // Should either succeed (no lights found) or fail with network error,
        // but NOT with a security error.
        match result {
            Ok(r) => {
                if let Some(ref err) = r.error {
                    assert!(!err.contains("read-only"));
                    assert!(!err.contains("rate limit"));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(!msg.contains("read-only"));
                assert!(!msg.contains("rate limit"));
            }
        }
    }

    #[tokio::test]
    async fn state_readonly_allowed() {
        // State is read-only and should not be blocked by security policy.
        let tool = LifxTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());
        let result = tool
            .execute(json!({"action": "state", "target": "10.0.0.1"}))
            .await;
        match result {
            Ok(r) => {
                if let Some(ref err) = r.error {
                    assert!(!err.contains("read-only"));
                    assert!(!err.contains("rate limit"));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(!msg.contains("read-only"));
                assert!(!msg.contains("rate limit"));
            }
        }
    }

    #[test]
    fn format_mac_produces_expected_format() {
        let mac = LifxTool::format_mac(0x00_00_aa_bb_cc_dd_ee_ff);
        // Little-endian bytes: ff, ee, dd, cc, bb, aa, 00, 00
        assert_eq!(mac, "ff:ee:dd:cc:bb:aa");
    }

    #[test]
    fn build_packet_get_service_succeeds() {
        let result = LifxTool::build_packet(Message::GetService, None);
        assert!(result.is_ok());
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn build_packet_light_set_power_succeeds() {
        let result = LifxTool::build_packet(
            Message::LightSetPower {
                level: 65535,
                duration: 0,
            },
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn build_packet_light_set_color_succeeds() {
        let color = HSBK {
            hue: 21845,
            saturation: 65535,
            brightness: 32768,
            kelvin: 3500,
        };
        let result = LifxTool::build_packet(
            Message::LightSetColor {
                reserved: 0,
                color,
                duration: 1000,
            },
            None,
        );
        assert!(result.is_ok());
    }
}
