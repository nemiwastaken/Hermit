//! MCP servers are remote HTTP endpoints in the net container. Core only
//! knows their tool schemas; the call itself never touches core's network.

use serde_json::{Value, json};

use crate::sandbox::Sandbox;

#[derive(Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub server: String,
    pub tool: String,
    pub description: String,
    pub parameters: Value,
    /// The server's config can demand a Telegram confirmation per call.
    pub approval: bool,
}

/// Ask net for the configured servers and their tools. A broken server is
/// logged and skipped: one bad config must not stop Hermit from starting.
pub async fn load(net: &Sandbox) -> Vec<McpTool> {
    // One attempt with a short timeout: MCP must not delay the bot's start.
    let value = match net.call_once("mcp-load", "mcp_list", &json!({})).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mcp: {e:#}");
            return Vec::new();
        }
    };
    if value["ok"] != true {
        eprintln!(
            "mcp: {}",
            value["error"].as_str().unwrap_or("net refused mcp_list")
        );
        return Vec::new();
    }

    parse(&value)
}

fn parse(value: &Value) -> Vec<McpTool> {
    let mut out = Vec::new();
    for server in value["servers"].as_array().cloned().unwrap_or_default() {
        let server_name = server["name"].as_str().unwrap_or("");
        if let Some(error) = server["error"].as_str() {
            eprintln!("mcp {server_name}: {error}");
            continue;
        }
        let approval = server["approval"].as_bool().unwrap_or(false);
        for tool in server["tools"].as_array().cloned().unwrap_or_default() {
            let tool_name = tool["name"].as_str().unwrap_or("");
            let name = wire_name(server_name, tool_name);
            if name.is_empty() || tool_name.is_empty() {
                continue;
            }
            let description: String = tool["description"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(1_000)
                .collect();
            let parameters = tool["inputSchema"]
                .as_object()
                .map(|_| tool["inputSchema"].clone())
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            out.push(McpTool {
                name,
                server: server_name.to_string(),
                tool: tool_name.to_string(),
                description,
                parameters,
                approval,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// OpenAI tool names allow only [A-Za-z0-9_-], max 64 characters.
fn wire_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut name = format!("mcp__{}__{}", clean(server), clean(tool));
    name.truncate(64);
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skips_broken_servers_and_keeps_approval() {
        let value = json!({"ok": true, "servers": [
            {"name": "demo", "approval": true, "tools": [
                {"name": "echo", "description": "e",
                 "inputSchema": {"type": "object", "properties": {"x": {"type": "string"}}}}
            ]},
            {"name": "broken", "error": "no route", "tools": []}
        ]});
        let tools = parse(&value);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mcp__demo__echo");
        assert!(tools[0].approval);
        assert_eq!(tools[0].parameters["properties"]["x"]["type"], "string");
    }

    #[test]
    fn wire_names_are_safe_and_stable() {
        assert_eq!(
            wire_name("github", "list_issues"),
            "mcp__github__list_issues"
        );
        assert_eq!(wire_name("my.server", "a b"), "mcp__my_server__a_b");
        assert!(wire_name(&"x".repeat(80), "y").len() <= 64);
    }
}
