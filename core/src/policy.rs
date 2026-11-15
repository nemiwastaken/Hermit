use anyhow::{Result, bail};
use serde_json::{Value, json};

/// The only tools core is allowed to ask for.
const ALLOWED: [&str; 7] = [
    "bash",
    "read",
    "recall",
    "manage_schedule",
    "update_behavior",
    "log_metric",
    "query_metrics",
];

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
        },
        {
            "type": "function",
            "function": {
                "name": "manage_schedule",
                "description": "Create, list or delete reminders. The current local time is at the end of \
                     the system prompt; all times here are local. For create pass exactly one of at, \
                     daily_at, weekly_at. kind=prompt (default) sends text to you when due. \
                     kind=question sends the text to the user as is and records the numeric reply \
                     under key, so no model is involved. kind=report sends the text with {avg} and \
                     {count} filled from the key's metrics over the last days. Use question and \
                     report for trackers the user wants exactly as written.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "list", "delete"]},
                        "text": {"type": "string", "description": "the message to send, or a report template with {avg}/{count} and phrased at yourself only for kind=prompt"},
                        "at": {"type": "string", "description": "one-shot, local: YYYY-MM-DD HH:MM"},
                        "daily_at": {"type": "string", "description": "every day at HH:MM local"},
                        "weekly_at": {"type": "string", "description": "every week at \"mon HH:MM\" local"},
                        "kind": {"type": "string", "enum": ["prompt", "question", "report"], "description": "default prompt"},
                        "key": {"type": "string", "description": "metric key for question/report, e.g. weight"},
                        "days": {"type": "integer", "description": "report window in days, default 7"},
                        "id": {"type": "integer", "description": "schedule id for delete"}
                    },
                    "required": ["action"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "update_behavior",
                "description": "Replace one behavior file and commit it. Use it when the user asks you \
                     to remember something about them or to change your style or skills. path is \
                     user_profile.md or skills/<name>.md; content is the full new file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "user_profile.md or skills/<name>.md"},
                        "content": {"type": "string", "description": "the full new file"},
                        "note": {"type": "string", "description": "one-line reason for the commit"}
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "log_metric",
                "description": "Record a number or a short fact under a key, with an optional JSON object \
                     of extra fields. Use it when the user gives a measurable value or asks you to \
                     remember a dated fact.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string", "description": "short stable name, e.g. weight or mood"},
                        "value_num": {"type": "number"},
                        "value_text": {"type": "string"},
                        "meta": {"type": "object", "description": "extra fields, e.g. {\"unit\": \"kg\"}"}
                    },
                    "required": ["key"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "query_metrics",
                "description": "Read recorded metrics. With key: count/sum/avg/min/max/last plus recent \
                     rows; since_days limits the window. Without key: list known keys. Numbers come \
                     from SQLite, never estimate them yourself.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "since_days": {"type": "integer", "description": "window in days"},
                        "limit": {"type": "integer", "description": "max recent rows, default 10"}
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

/// Commands that can destroy data under /workspace. Heuristic on the whole
/// arguments blob, so it also sees inside `bash -lc "…"`; a false positive
/// only costs one confirmation.
const RISKY: [&str; 20] = [
    "rm -rf",
    "rm -fr",
    "rm -r -f",
    "rm -f -r",
    "rm --recursive",
    "mkfs",
    "wipefs",
    "shred",
    "dd if=",
    "dd of=",
    "fdisk",
    "parted",
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "chmod -r 777",
    "chown -r",
    "truncate -s 0",
    "git clean -fdx",
];

/// Some(danger) when the call must be confirmed by the user first.
pub fn approval_reason(name: &str, args: &Value) -> Option<&'static str> {
    if name != "bash" {
        return None;
    }
    // joining argv keeps both `["rm","-rf"]` and `bash -lc "rm -rf …"` visible
    let mut blob = args["cmd"]
        .as_array()
        .map(|argv| {
            argv.iter()
                .filter_map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    if blob.is_empty() {
        blob = args.to_string();
    }
    let blob = blob.to_lowercase();
    RISKY.iter().copied().find(|p| blob.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_unknown_and_safe_mode() {
        assert!(check("bash", false).is_ok());
        assert!(check("read", false).is_ok());
        assert!(check("recall", false).is_ok());
        assert!(check("manage_schedule", false).is_ok());
        assert!(check("update_behavior", false).is_ok());
        assert!(check("log_metric", false).is_ok());
        assert!(check("query_metrics", false).is_ok());
        assert!(check("write", false).is_err());
        assert!(check("bash", true).is_err());
    }

    #[test]
    fn flags_destructive_bash_only() {
        let risky = json!({"cmd": ["rm", "-rf", "/workspace/tmp"]});
        assert!(approval_reason("bash", &risky).is_some());
        let hidden = json!({"cmd": ["bash", "-lc", "echo hi && rm -rf /workspace"]});
        assert!(approval_reason("bash", &hidden).is_some());
        assert!(approval_reason("bash", &json!({"cmd": ["ls", "-la"]})).is_none());
        assert!(approval_reason("read", &json!({"path": "rm -rf"})).is_none());
    }
}
