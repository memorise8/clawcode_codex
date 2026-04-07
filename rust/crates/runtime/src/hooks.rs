use std::process::Command;

use crate::json::JsonValue;

/// Outcome of running a hook
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Hook(s) ran successfully or no hooks configured
    Allow,
    /// A hook blocked the operation
    Block { reason: String },
}

/// Runs shell commands configured as hooks in settings.json
#[derive(Debug, Clone)]
pub struct HookRunner {
    pre_tool_use: Vec<HookEntry>,
    post_tool_use: Vec<HookEntry>,
    stop: Vec<HookEntry>,
}

#[derive(Debug, Clone)]
struct HookEntry {
    command: String,
    /// Optional tool name filter — if set, hook only runs for matching tools
    tool_match: Option<String>,
}

impl HookRunner {
    /// Build a HookRunner from the merged config's "hooks" JSON value.
    #[must_use]
    pub fn from_config_value(hooks: Option<&JsonValue>) -> Self {
        let Some(hooks) = hooks.and_then(|v| v.as_object()) else {
            return Self::empty();
        };
        Self {
            pre_tool_use: parse_hook_entries(hooks.get("PreToolUse")),
            post_tool_use: parse_hook_entries(hooks.get("PostToolUse")),
            stop: parse_hook_entries(hooks.get("Stop")),
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            pre_tool_use: Vec::new(),
            post_tool_use: Vec::new(),
            stop: Vec::new(),
        }
    }

    #[must_use]
    pub fn has_hooks(&self) -> bool {
        !self.pre_tool_use.is_empty() || !self.post_tool_use.is_empty() || !self.stop.is_empty()
    }

    /// Run pre-tool-use hooks. Returns Block if any hook exits non-zero.
    pub fn run_pre_tool_use(&self, tool_name: &str, input: &str) -> HookOutcome {
        run_hooks(&self.pre_tool_use, tool_name, Some(input))
    }

    /// Run post-tool-use hooks. Returns Block if any hook exits non-zero.
    pub fn run_post_tool_use(&self, tool_name: &str, output: &str) -> HookOutcome {
        run_hooks(&self.post_tool_use, tool_name, Some(output))
    }

    /// Run stop hooks (session end). Ignores failures.
    pub fn run_stop(&self) {
        for entry in &self.stop {
            let _ = Command::new("sh").arg("-c").arg(&entry.command).status();
        }
    }
}

fn parse_hook_entries(value: Option<&JsonValue>) -> Vec<HookEntry> {
    let Some(arr) = value.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            if let Some(s) = entry.as_str() {
                Some(HookEntry {
                    command: s.to_string(),
                    tool_match: None,
                })
            } else if let Some(obj) = entry.as_object() {
                let command = obj.get("command")?.as_str()?.to_string();
                let tool_match = obj
                    .get("match")
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned);
                Some(HookEntry {
                    command,
                    tool_match,
                })
            } else {
                None
            }
        })
        .collect()
}

fn run_hooks(entries: &[HookEntry], tool_name: &str, context: Option<&str>) -> HookOutcome {
    for entry in entries {
        // Skip if tool_match is set and doesn't match
        if let Some(ref pattern) = entry.tool_match {
            if !tool_name.contains(pattern.as_str()) {
                continue;
            }
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&entry.command);
        cmd.env("TOOL_NAME", tool_name);
        if let Some(ctx) = context {
            // Truncate context for env var to avoid E2BIG
            let truncated = if ctx.len() > 4096 { &ctx[..4096] } else { ctx };
            cmd.env("TOOL_INPUT", truncated);
        }
        match cmd.output() {
            Ok(output) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let reason = if !stderr.is_empty() {
                        stderr.trim().to_string()
                    } else if !stdout.is_empty() {
                        stdout.trim().to_string()
                    } else {
                        format!("hook '{}' exited with {}", entry.command, output.status)
                    };
                    return HookOutcome::Block { reason };
                }
            }
            Err(error) => {
                return HookOutcome::Block {
                    reason: format!("hook '{}' failed to execute: {error}", entry.command),
                };
            }
        }
    }
    HookOutcome::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_hooks_config(pre: &[&str], post: &[&str], stop: &[&str]) -> JsonValue {
        let mut hooks = BTreeMap::new();
        let to_array = |cmds: &[&str]| -> JsonValue {
            JsonValue::Array(
                cmds.iter()
                    .map(|c| {
                        let mut obj = BTreeMap::new();
                        obj.insert("command".to_string(), JsonValue::String(c.to_string()));
                        JsonValue::Object(obj)
                    })
                    .collect(),
            )
        };
        if !pre.is_empty() {
            hooks.insert("PreToolUse".to_string(), to_array(pre));
        }
        if !post.is_empty() {
            hooks.insert("PostToolUse".to_string(), to_array(post));
        }
        if !stop.is_empty() {
            hooks.insert("Stop".to_string(), to_array(stop));
        }
        JsonValue::Object(hooks)
    }

    #[test]
    fn empty_config_produces_no_hooks() {
        let runner = HookRunner::from_config_value(None);
        assert!(!runner.has_hooks());
        assert_eq!(runner.run_pre_tool_use("bash", ""), HookOutcome::Allow);
    }

    #[test]
    fn successful_hook_allows() {
        let config = make_hooks_config(&["true"], &[], &[]);
        let runner = HookRunner::from_config_value(Some(&config));
        assert!(runner.has_hooks());
        assert_eq!(
            runner.run_pre_tool_use("bash", "echo hi"),
            HookOutcome::Allow
        );
    }

    #[test]
    fn failing_hook_blocks() {
        let config = make_hooks_config(&["false"], &[], &[]);
        let runner = HookRunner::from_config_value(Some(&config));
        match runner.run_pre_tool_use("bash", "") {
            HookOutcome::Block { .. } => {}
            HookOutcome::Allow => panic!("expected Block"),
        }
    }

    #[test]
    fn tool_match_filters_correctly() {
        let mut obj = BTreeMap::new();
        obj.insert(
            "command".to_string(),
            JsonValue::String("false".to_string()),
        );
        obj.insert(
            "match".to_string(),
            JsonValue::String("write".to_string()),
        );
        let mut hooks = BTreeMap::new();
        hooks.insert(
            "PreToolUse".to_string(),
            JsonValue::Array(vec![JsonValue::Object(obj)]),
        );
        let config = JsonValue::Object(hooks);
        let runner = HookRunner::from_config_value(Some(&config));
        // "bash" doesn't match "write", so hook is skipped
        assert_eq!(
            runner.run_pre_tool_use("bash", ""),
            HookOutcome::Allow
        );
        // "write_file" contains "write", so hook runs and blocks
        match runner.run_pre_tool_use("write_file", "") {
            HookOutcome::Block { .. } => {}
            HookOutcome::Allow => panic!("expected Block for write_file"),
        }
    }

    #[test]
    fn string_form_hook_entries() {
        let mut hooks = BTreeMap::new();
        hooks.insert(
            "PreToolUse".to_string(),
            JsonValue::Array(vec![JsonValue::String("true".to_string())]),
        );
        let config = JsonValue::Object(hooks);
        let runner = HookRunner::from_config_value(Some(&config));
        assert!(runner.has_hooks());
        assert_eq!(
            runner.run_pre_tool_use("bash", ""),
            HookOutcome::Allow
        );
    }
}
