use anyhow::{Result, bail};
use serde_json::{Value, json};

/// The only tools core is allowed to ask for.
const ALLOWED: [&str; 3] = ["bash", "read", "recall"];

pub fn schema() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a command inside the sandbox container and return its output. \
                     cmd is an argv array, there is no shell: for pipes or redirects pass \
                     [\"bash\",\"-lc\",\"…\"] explicitly. Working dir defaults to /workspace.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "cmd": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "argv, first element is the executable"
                        },
                        "cwd": {"type": "string", "description": "working directory, default /workspace"},
                        "timeout_ms": {"type": "integer", "description": "max runtime, default 30000, max 600000"}
                    },
                    "required": ["cmd"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a UTF-8 text file by lines. Use offset/limit for big files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "integer", "description": "0-based first line, default 0"},
                        "limit": {"type": "integer", "description": "max lines, default 200, max 2000"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "recall",
                "description": "Read an earlier conversation from the archive. With session_id return its \
                     transcript; without arguments return the session mentioned in the recap, or the last \
                     10 sessions; with query search past sessions.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "integer", "description": "archived session id"},
                        "query": {"type": "string", "description": "words to search in past sessions"}
                    }
                }
            }
        }
    ])
}

/// Deny by default: a name that is not on the list never reaches the Sandbox.
pub fn check(tool: &str, safe_mode: bool) -> Result<()> {
    if safe_mode {
        bail!("safe mode: tools are disabled (send /safe off to re-enable)");
    }
    if !ALLOWED.contains(&tool) {
        bail!("tool `{tool}` is not allowed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_unknown_and_safe_mode() {
        assert!(check("bash", false).is_ok());
        assert!(check("read", false).is_ok());
        assert!(check("recall", false).is_ok());
        assert!(check("write", false).is_err());
        assert!(check("bash", true).is_err());
    }
}
