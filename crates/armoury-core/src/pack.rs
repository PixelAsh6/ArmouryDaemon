//! Data-only pack format (v1) — see PACK-SPEC-PROPOSAL.md.
//!
//! A pack is JSON: which process it targets, exactly one dropdown setting, what to
//! capture before acting, and the ordered steps to apply and to revert. All mechanics
//! live in the daemon; installing a pack can never execute arbitrary code.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

fn default_spec() -> u32 {
    1
}
fn default_order() -> i32 {
    100
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pack {
    #[serde(default = "default_spec")]
    pub spec: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// Apply ascending, revert descending.
    #[serde(default = "default_order")]
    pub order: i32,
    pub targets: Targets,
    /// Exactly one dropdown in v1.
    #[serde(default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub capture: Vec<CaptureStep>,
    pub apply: StepBlock,
    pub revert: StepBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Targets {
    /// Process exe file names (matched case-insensitively, per the user's choice).
    pub process: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Setting {
    pub key: String,
    pub label: String,
    /// "enum" in v1 (rendered as a dropdown).
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub values: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureStep {
    pub key: String,
    /// "process_running" | "json_file"
    pub mechanism: String,
    #[serde(default)]
    pub path: Option<String>,
    /// Simple JSON pointer, e.g. `/performance_mode`.
    #[serde(default)]
    pub pointer: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepBlock {
    #[serde(default)]
    pub guard: Guard,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guard {
    /// Skip the whole block when the target process is not running.
    #[serde(rename = "targetRunning", default = "default_true")]
    pub target_running: bool,
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            target_running: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// "cli" | "hotkey" | "rainmeter_bang"
    pub mechanism: String,
    #[serde(default)]
    pub exe: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub bang: Option<String>,
    /// For hotkey steps: value -> key chord.
    #[serde(default)]
    pub map: HashMap<String, String>,
    /// For hotkey steps: take the map key from this setting.
    #[serde(default)]
    pub keys_from_mode: Option<String>,
    /// For hotkey steps: take the map key from this captured value.
    #[serde(default)]
    pub keys_from_captured: Option<String>,
    /// For cli steps: expected exit code (otherwise any success is fine).
    #[serde(default)]
    pub expect_exit: Option<i32>,
}

/// Values available to `{{...}}` substitution in a pack.
#[derive(Debug, Default, Clone)]
pub struct TemplateCtx {
    /// The pack's current setting values.
    pub settings: HashMap<String, String>,
    /// Values captured before applying.
    pub captured: HashMap<String, String>,
    /// The running target: full image path.
    pub target_exe: String,
    /// The running target: exe file name.
    pub target_name: String,
}

/// Replace `{{settings.x}}`, `{{captured.x}}`, `{{target.exe}}`, `{{target.name}}`.
/// Unknown placeholders are left untouched (and reported in the returned list).
pub fn substitute(template: &str, ctx: &TemplateCtx) -> (String, Vec<String>) {
    let mut out = String::with_capacity(template.len());
    let mut unknown = Vec::new();
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return (out, unknown);
        };
        let expr = after[..end].trim();
        let value = match expr {
            "target.exe" => Some(ctx.target_exe.clone()),
            "target.name" => Some(ctx.target_name.clone()),
            _ => {
                let (scope, key) = expr.split_once('.').unwrap_or(("", expr));
                match scope {
                    "settings" => ctx.settings.get(key).cloned(),
                    "captured" => ctx.captured.get(key).cloned(),
                    _ => None,
                }
            }
        };
        match value {
            Some(v) => out.push_str(&v),
            None => {
                unknown.push(expr.to_string());
                out.push_str("{{");
                out.push_str(expr);
                out.push_str("}}");
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    (out, unknown)
}

const ALLOWED_MECHANISMS: [&str; 3] = ["cli", "hotkey", "rainmeter_bang"];
const ALLOWED_CAPTURES: [&str; 2] = ["process_running", "json_file"];

impl Pack {
    /// Human-readable problems; an empty list means the pack is valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.spec != 1 {
            errs.push(format!("unsupported spec {} (expected 1)", self.spec));
        }
        if self.id.trim().is_empty() {
            errs.push("id must not be empty".into());
        }
        if self.name.trim().is_empty() {
            errs.push("name must not be empty".into());
        }
        if self.targets.process.is_empty() {
            errs.push("targets.process must list at least one exe file name".into());
        }
        if self.settings.len() > 1 {
            errs.push("v1 packs expose exactly one setting (the dropdown)".into());
        }
        for s in &self.settings {
            if s.kind != "enum" {
                errs.push(format!("setting '{}': unsupported type '{}'", s.key, s.kind));
            }
            if s.values.is_empty() {
                errs.push(format!("setting '{}': needs values", s.key));
            }
            if let Some(default) = &s.default {
                if !s.values.iter().any(|v| v == default) {
                    errs.push(format!(
                        "setting '{}': default '{default}' is not one of the values",
                        s.key
                    ));
                }
            }
        }
        for c in &self.capture {
            if !ALLOWED_CAPTURES.contains(&c.mechanism.as_str()) {
                errs.push(format!(
                    "capture '{}': unsupported mechanism '{}'",
                    c.key, c.mechanism
                ));
            }
            if c.mechanism == "json_file" && c.path.is_none() {
                errs.push(format!("capture '{}': json_file needs a path", c.key));
            }
        }
        for (block, steps) in [("apply", &self.apply.steps), ("revert", &self.revert.steps)] {
            if steps.is_empty() {
                errs.push(format!("{block}: must have at least one step"));
            }
            for (i, step) in steps.iter().enumerate() {
                if !ALLOWED_MECHANISMS.contains(&step.mechanism.as_str()) {
                    errs.push(format!(
                        "{block} step {i}: unsupported mechanism '{}'",
                        step.mechanism
                    ));
                }
                if step.mechanism == "cli" && step.exe.is_none() {
                    errs.push(format!("{block} step {i}: cli needs an exe"));
                }
                if step.mechanism == "rainmeter_bang" && step.bang.is_none() {
                    errs.push(format!("{block} step {i}: rainmeter_bang needs a bang"));
                }
                if step.mechanism == "hotkey" && step.map.is_empty() {
                    errs.push(format!("{block} step {i}: hotkey needs a map"));
                }
                if step.mechanism == "hotkey"
                    && step.keys_from_mode.is_none()
                    && step.keys_from_captured.is_none()
                {
                    errs.push(format!(
                        "{block} step {i}: hotkey needs keys_from_mode or keys_from_captured"
                    ));
                }
            }
        }
        errs
    }

    /// The value of the pack's single dropdown for a given user config.
    pub fn setting_value(&self, values: &HashMap<String, String>) -> Option<(String, String)> {
        let setting = self.settings.first()?;
        let value = values
            .get(&setting.key)
            .cloned()
            .or_else(|| setting.default.clone())
            .unwrap_or_default();
        Some((setting.key.clone(), value))
    }

    /// Parse a pack and reject it if invalid.
    pub fn parse(text: &str) -> Result<Pack, String> {
        let pack: Pack = serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
        let errs = pack.validate();
        if errs.is_empty() {
            Ok(pack)
        } else {
            Err(errs.join("; "))
        }
    }
}

/// Parse a duration in seconds, at least 1 s.
pub fn cooldown_from_secs(secs: u64) -> Duration {
    Duration::from_secs(secs.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WE: &str = include_str!("../../../packs/wallpaper-engine/pack.json");
    const GH: &str = include_str!("../../../packs/g-helper/pack.json");
    const RM: &str = include_str!("../../../packs/rainmeter/pack.json");

    #[test]
    fn shipped_packs_are_valid() {
        for (label, text) in [("wallpaper-engine", WE), ("g-helper", GH), ("rainmeter", RM)] {
            let pack = Pack::parse(text).unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(pack.spec, 1);
            assert!(!pack.targets.process.is_empty());
            assert!(pack.settings.len() <= 1);
            assert!(!pack.apply.steps.is_empty());
            assert!(!pack.revert.steps.is_empty());
        }
    }

    #[test]
    fn shipped_packs_have_one_dropdown_with_a_default() {
        for text in [WE, GH, RM] {
            let pack = Pack::parse(text).unwrap();
            let setting = &pack.settings[0];
            assert_eq!(setting.kind, "enum");
            assert!(setting.default.is_some(), "{} needs a default", pack.id);
            assert!(setting.values.contains(setting.default.as_ref().unwrap()));
        }
    }

    #[test]
    fn substitution_handles_all_scopes() {
        let mut ctx = TemplateCtx {
            target_exe: "C:\\Games\\wallpaper64.exe".into(),
            target_name: "wallpaper64.exe".into(),
            ..Default::default()
        };
        ctx.settings.insert("action".into(), "stop".into());
        ctx.captured.insert("prev_mode".into(), "2".into());

        let (out, unknown) = substitute(
            "{{target.exe}} -control {{settings.action}} (was {{captured.prev_mode}})",
            &ctx,
        );
        assert_eq!(out, "C:\\Games\\wallpaper64.exe -control stop (was 2)");
        assert!(unknown.is_empty());
    }

    #[test]
    fn unknown_placeholders_are_reported_and_left_alone() {
        let ctx = TemplateCtx::default();
        let (out, unknown) = substitute("{{settings.nope}}!", &ctx);
        assert_eq!(out, "{{settings.nope}}!");
        assert_eq!(unknown, vec!["settings.nope".to_string()]);
    }

    #[test]
    fn validation_rejects_the_usual_mistakes() {
        let mut pack = Pack::parse(WE).unwrap();
        pack.settings.push(Setting {
            key: "extra".into(),
            label: "Extra".into(),
            kind: "enum".into(),
            values: vec!["a".into()],
            default: None,
        });
        let errs = pack.validate();
        assert!(errs.iter().any(|e| e.contains("exactly one setting")));

        let mut pack = Pack::parse(GH).unwrap();
        pack.apply.steps[0].mechanism = "exec_shell".into();
        assert!(pack
            .validate()
            .iter()
            .any(|e| e.contains("unsupported mechanism")));

        let mut pack = Pack::parse(RM).unwrap();
        pack.settings[0].default = Some("Nope".into());
        assert!(pack
            .validate()
            .iter()
            .any(|e| e.contains("is not one of the values")));
    }

    #[test]
    fn bad_json_is_rejected() {
        assert!(Pack::parse("{ not json").is_err());
        assert!(Pack::parse(r#"{"spec":1}"#).is_err());
    }
}
