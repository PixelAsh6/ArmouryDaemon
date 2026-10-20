//! The pack format: spec 1 (data only) and spec 2 (data plus a sandboxed script). The
//! schema is enforced here; see the daemon's README for the format, `armoury::packs` for
//! how a pack is loaded and installed, and the catalogue repository for examples.
//!
//! A pack is JSON: which process it targets, up to four dropdown settings, what to
//! capture before acting, and the ordered steps to apply and to revert. All mechanics
//! live in the daemon; a spec 2 script is interpreted by the daemon against a whitelisted
//! host API, so installing a pack can never execute arbitrary code.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
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
    /// The smallest daemon that can run this pack, e.g. "1.1.0". No minimum means every
    /// daemon may run it (a pack that needs a host function the daemon does not have says
    /// so here, so an older daemon refuses it instead of failing at apply).
    #[serde(default)]
    pub min_engine: Option<String>,
    /// Apply ascending, revert descending.
    #[serde(default = "default_order")]
    pub order: i32,
    pub targets: Targets,
    /// Up to four dropdowns (the first one is the primary action).
    #[serde(default)]
    pub settings: Vec<Setting>,
    #[serde(default)]
    pub capture: Vec<CaptureStep>,
    /// Spec 2: a script per block, run by the daemon after the steps (Rhai, sandboxed).
    #[serde(default)]
    pub script: PackScript,
    pub apply: StepBlock,
    pub revert: StepBlock,
}

/// Scripted blocks (spec 2). The pack carries the source; the daemon interprets it against a
/// whitelisted host API — a pack never executes anything by itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackScript {
    #[serde(default)]
    pub apply: Option<String>,
    #[serde(default)]
    pub revert: Option<String>,
}

impl PackScript {
    pub fn is_empty(&self) -> bool {
        self.apply.as_deref().map_or(true, |s| s.trim().is_empty())
            && self.revert.as_deref().map_or(true, |s| s.trim().is_empty())
    }
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
    /// Skip the whole block unless the target was running when we applied
    /// (captured `was_running == "1"`). Keeps revert from starting apps that were off,
    /// while still restoring an app we deliberately killed.
    #[serde(rename = "capturedRunning", default)]
    pub captured_running: bool,
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            target_running: true,
            captured_running: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// "cli" | "hotkey" | "process_kill" | "process_start" | "registry_set"
    pub mechanism: String,
    #[serde(default)]
    pub exe: Option<String>,
    /// Pick the exe from a dropdown instead of `exe` (e.g. which binary of an app to drive).
    /// The chosen value is looked up in `exe_map` and then template-substituted.
    #[serde(default)]
    pub exe_from_setting: Option<String>,
    /// Dropdown value -> exe template, used with `exe_from_setting`.
    #[serde(default)]
    pub exe_map: HashMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
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
    /// Only run this step when a condition matches (data-only branching).
    #[serde(default)]
    pub when: Option<When>,
    /// Wait this long before running the step (lets a relaunched app come up).
    #[serde(default)]
    pub sleep_ms: Option<u64>,
    /// For registry steps: what to write.
    #[serde(default)]
    pub registry: Option<RegistrySet>,
}

/// A REG_DWORD write, e.g. clearing Wallpaper Engine's `faultyShutdown` flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrySet {
    /// Full key path, e.g. `HKEY_CURRENT_USER\Software\WallpaperEngine`.
    pub path: String,
    pub name: String,
    pub dword: u32,
}

/// A simple equality condition used to branch on the pack's dropdown or a captured value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct When {
    /// "setting" (the pack's dropdown) or "captured".
    pub source: String,
    /// For `setting`: the setting key (defaults to the pack's only setting).
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub not_equals: Option<String>,
}

impl When {
    /// Does this step apply, given the current template context?
    pub fn matches(&self, ctx: &TemplateCtx, setting_key: Option<&str>) -> bool {
        let value = match self.source.as_str() {
            "setting" => {
                let key = self.key.as_deref().or(setting_key).unwrap_or_default();
                ctx.settings.get(key).cloned().unwrap_or_default()
            }
            "captured" => ctx
                .captured
                .get(self.key.as_deref().unwrap_or_default())
                .cloned()
                .unwrap_or_default(),
            _ => return false,
        };
        if let Some(want) = &self.equals {
            if !value.eq_ignore_ascii_case(want) {
                return false;
            }
        }
        if let Some(no) = &self.not_equals {
            if value.eq_ignore_ascii_case(no) {
                return false;
            }
        }
        true
    }
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
    /// The running target's folder (so a pack can call a sibling binary, e.g. the 32-bit
    /// variant of an app that has both).
    pub target_dir: String,
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
            "target.dir" => Some(ctx.target_dir.clone()),
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

const ALLOWED_MECHANISMS: [&str; 5] = [
    "cli",
    "hotkey",
    "process_kill",
    "process_start",
    "registry_set",
];
const ALLOWED_CAPTURES: [&str; 3] = ["process_running", "json_file", "target_path"];

/// A version split into its numbers ("1.2.3" -> [1, 2, 3]). Only digits and dots are
/// allowed, the shape of a `Cargo.toml` version: "1", "1.0" and "1.0.0" are versions,
/// "1.0.0-beta", "v1.0", "1..0" and "" are not. A malformed version is an error, never a
/// panic.
fn version_parts(version: &str) -> Result<Vec<u64>, String> {
    let text = version.trim();
    if text.is_empty() {
        return Err("the version is empty".to_string());
    }
    let mut parts = Vec::new();
    for part in text.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("'{version}' is not a dotted numeric version"));
        }
        let number = part
            .parse::<u64>()
            .map_err(|_| format!("'{version}' has a part that is too large to be a number"))?;
        parts.push(number);
    }
    Ok(parts)
}

/// Compare two dotted numeric versions. A missing part counts as zero, so "1.0" and
/// "1.0.0" are the same version.
pub fn compare_versions(a: &str, b: &str) -> Result<Ordering, String> {
    let left = version_parts(a)?;
    let right = version_parts(b)?;
    for index in 0..left.len().max(right.len()) {
        let l = left.get(index).copied().unwrap_or(0);
        let r = right.get(index).copied().unwrap_or(0);
        if l != r {
            return Ok(l.cmp(&r));
        }
    }
    Ok(Ordering::Equal)
}

/// Is `daemon` at least `required`? A malformed version is an error, not a guess: the
/// caller turns it into a refusal with the reason.
pub fn version_at_least(daemon: &str, required: &str) -> Result<bool, String> {
    Ok(compare_versions(daemon, required)? != Ordering::Less)
}

impl Pack {
    /// Human-readable problems; an empty list means the pack is valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.spec != 1 && self.spec != 2 {
            errs.push(format!(
                "unsupported spec {} (expected 1 = data only, 2 = data + script)",
                self.spec
            ));
        }
        if !self.script.is_empty() && self.spec != 2 {
            errs.push("a pack with a script must declare \"spec\": 2".into());
        }
        if self.script.is_empty() && self.spec == 2 {
            errs.push("\"spec\": 2 needs a script block (apply and/or revert)".into());
        }
        if self.id.trim().is_empty() {
            errs.push("id must not be empty".into());
        }
        if self.name.trim().is_empty() {
            errs.push("name must not be empty".into());
        }
        if let Some(min) = &self.min_engine
            && version_parts(min).is_err()
        {
            errs.push(format!(
                "min_engine '{min}' is not a version like \"1.0.0\" (digits and dots only)"
            ));
        }
        if self.targets.process.is_empty() {
            errs.push("targets.process must list at least one exe file name".into());
        }
        if self.settings.len() > 4 {
            errs.push("a pack may expose at most 4 dropdowns".into());
        }
        for (i, s) in self.settings.iter().enumerate() {
            if self.settings[..i].iter().any(|other| other.key == s.key) {
                errs.push(format!("setting '{}': duplicate key", s.key));
            }
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
        for (block, steps, script) in [
            ("apply", &self.apply.steps, &self.script.apply),
            ("revert", &self.revert.steps, &self.script.revert),
        ] {
            if steps.is_empty() && script.as_deref().map_or(true, |s| s.trim().is_empty()) {
                errs.push(format!("{block}: needs steps or a script"));
            }
            for (i, step) in steps.iter().enumerate() {
                if !ALLOWED_MECHANISMS.contains(&step.mechanism.as_str()) {
                    errs.push(format!(
                        "{block} step {i}: unsupported mechanism '{}'",
                        step.mechanism
                    ));
                }
                if step.mechanism == "cli"
                    && step.exe.is_none()
                    && step.exe_from_setting.is_none()
                {
                    errs.push(format!("{block} step {i}: cli needs an exe"));
                }
                if step.mechanism == "process_start"
                    && step.exe.is_none()
                    && step.exe_from_setting.is_none()
                {
                    errs.push(format!("{block} step {i}: process_start needs an exe"));
                }
                if step.mechanism == "registry_set" && step.registry.is_none() {
                    errs.push(format!("{block} step {i}: registry_set needs a registry block"));
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
                if step.exe_from_setting.is_some() && step.exe_map.is_empty() {
                    errs.push(format!(
                        "{block} step {i}: exe_from_setting needs an exe_map"
                    ));
                }
                if let Some(key) = &step.exe_from_setting {
                    if !self.settings.iter().any(|s| &s.key == key) {
                        errs.push(format!(
                            "{block} step {i}: exe_from_setting '{key}' is not one of the pack's settings"
                        ));
                    }
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
            assert!(pack.spec == 1 || pack.spec == 2, "{label}: spec {}", pack.spec);
            assert!(!pack.targets.process.is_empty());
            assert!(!pack.settings.is_empty());
            assert!(!pack.apply.steps.is_empty() || pack.script.apply.is_some());
            assert!(!pack.revert.steps.is_empty() || pack.script.revert.is_some());
        }
    }

    #[test]
    fn the_rainmeter_pack_is_spec_2_and_valid() {
        let pack = Pack::parse(RM).expect("the Rainmeter pack must be valid");
        assert_eq!(pack.spec, 2);
        assert!(pack.script.apply.is_some());
        assert!(pack.script.revert.is_some());
        // the "kill" option still runs through the steps
        assert!(pack.apply.steps.iter().any(|s| s.mechanism == "process_kill"));
        assert!(pack.revert.steps.iter().any(|s| s.mechanism == "process_start"));
        assert!(pack.validate().is_empty());
    }

    #[test]
    fn shipped_packs_have_dropdowns_with_defaults() {
        for text in [WE, GH, RM] {
            let pack = Pack::parse(text).unwrap();
            for setting in &pack.settings {
                assert_eq!(setting.kind, "enum");
                assert!(
                    setting.default.is_some(),
                    "{}: setting '{}' needs a default",
                    pack.id,
                    setting.key
                );
                assert!(setting
                    .values
                    .contains(setting.default.as_ref().unwrap()));
            }
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
        // more than four dropdowns is a mistake
        let mut pack = Pack::parse(WE).unwrap();
        for key in ["extra1", "extra2", "extra3", "extra4"] {
            pack.settings.push(Setting {
                key: key.into(),
                label: key.into(),
                kind: "enum".into(),
                values: vec!["a".into()],
                default: None,
            });
        }
        let errs = pack.validate();
        assert!(errs.iter().any(|e| e.contains("at most 4 dropdowns")));

        // a duplicate setting key is refused
        let mut pack = Pack::parse(WE).unwrap();
        let first = pack.settings[0].clone();
        pack.settings.push(first);
        assert!(pack.validate().iter().any(|e| e.contains("duplicate key")));

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

        // exe_from_setting must name a real setting and have a map
        let mut pack = Pack::parse(WE).unwrap();
        pack.apply.steps[0].exe_from_setting = Some("nope".into());
        assert!(pack
            .validate()
            .iter()
            .any(|e| e.contains("is not one of the pack's settings")));
        let mut pack = Pack::parse(WE).unwrap();
        pack.apply.steps[0].exe_map.clear();
        assert!(pack
            .validate()
            .iter()
            .any(|e| e.contains("needs an exe_map")));
    }

    #[test]
    fn spec_2_packs_may_carry_a_script() {
        let text = r#"{ "spec": 2, "id": "s2", "name": "Scripted",
            "targets": {"process":["x.exe"]},
            "script": { "apply": "ctx.log(\"hi\");", "revert": "ctx.log(\"bye\");" },
            "apply": { "steps": [] }, "revert": { "steps": [] } }"#;
        let pack = Pack::parse(text).expect("a spec-2 pack with scripts is valid");
        assert!(!pack.script.is_empty());

        // a script on a spec-1 pack is refused
        let mut p = pack.clone();
        p.spec = 1;
        assert!(p.validate().iter().any(|e| e.contains("must declare")));

        // spec 2 without any script is pointless and refused
        let mut p = pack.clone();
        p.script = Default::default();
        assert!(p.validate().iter().any(|e| e.contains("needs a script block")));

        // every block needs steps or a script (build it from a valid pack: parse rejects invalid ones)
        let mut p = pack.clone();
        p.script.revert = None;
        p.revert.steps.clear();
        assert!(p
            .validate()
            .iter()
            .any(|e| e.contains("revert: needs steps or a script")));
    }

    #[test]
    fn bad_json_is_rejected() {
        assert!(Pack::parse("{ not json").is_err());
        assert!(Pack::parse(r#"{"spec":1}"#).is_err());
    }

    /// A minimal valid pack, with the `min_engine` line the tests vary.
    const MIN_ENGINE_PACK: &str = r#"{
        "spec": 1, "id": "min-engine", "name": "Min engine", "version": "1.0.0",
        "targets": { "process": ["min.exe"] },
        "settings": [
            { "key": "action", "label": "Action", "type": "enum", "values": ["on"], "default": "on" }
        ],
        "min_engine": "1.1.0",
        "apply": { "steps": [ { "mechanism": "cli", "exe": "min.exe", "args": [] } ] },
        "revert": { "steps": [ { "mechanism": "cli", "exe": "min.exe", "args": [] } ] }
    }"#;

    #[test]
    fn a_version_comparison_knows_equal_newer_older_and_missing_parts() {
        assert_eq!(compare_versions("1.0.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(compare_versions("1.0.1", "1.0.0").unwrap(), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0", "1.0.1").unwrap(), Ordering::Less);
        assert_eq!(compare_versions("2.0", "1.9.9").unwrap(), Ordering::Greater);
        assert_eq!(compare_versions("1.9.9", "2.0").unwrap(), Ordering::Less);
        // a missing part counts as zero, so "1.0" and "1.0.0" are the same version
        assert_eq!(compare_versions("1.0", "1.0.0").unwrap(), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "1.0").unwrap(), Ordering::Equal);
        assert_eq!(compare_versions("1.0.0", "1.0.0.0").unwrap(), Ordering::Equal);
        // the numbers are numbers, not text: 1.10 is newer than 1.9
        assert_eq!(compare_versions("1.10.0", "1.9.9").unwrap(), Ordering::Greater);

        assert!(version_at_least("1.0.0", "1.0.0").unwrap());
        assert!(version_at_least("1.0.1", "1.0.0").unwrap());
        assert!(!version_at_least("1.0.0", "1.0.1").unwrap());
        assert!(version_at_least("1.0", "1.0.0").unwrap());
        assert!(version_at_least("2.0", "1.9.9").unwrap());
        assert!(!version_at_least("1.9.9", "2.0").unwrap());
    }

    #[test]
    fn a_malformed_version_is_an_error_and_never_a_panic() {
        for bad in ["", " ", "1.0.0-beta", "v1.0", "1..0", "1.0.x", "one", ".1", "1."] {
            let err = version_at_least("1.0.0", bad).expect_err("a malformed minimum is refused");
            assert!(!err.is_empty(), "'{bad}' must say why");
            let err = version_at_least(bad, "1.0.0").expect_err("a malformed daemon is refused");
            assert!(!err.is_empty(), "'{bad}' must say why");
        }
        // a part too large to be a number is an error too, not a panic
        assert!(version_at_least("1.0.0", "99999999999999999999.0").is_err());
    }

    #[test]
    fn a_pack_may_state_the_minimum_daemon_it_needs() {
        let pack = Pack::parse(MIN_ENGINE_PACK).expect("a pack with a min_engine is valid");
        assert_eq!(pack.min_engine.as_deref(), Some("1.1.0"));
        assert!(pack.validate().is_empty());

        // a pack without one needs no minimum
        let plain = MIN_ENGINE_PACK.replace("\"min_engine\": \"1.1.0\",", "");
        let pack = Pack::parse(&plain).expect("a pack without a min_engine is valid");
        assert!(pack.min_engine.is_none());
        assert!(pack.validate().is_empty());

        // a malformed minimum is refused, and the message says which field is wrong
        for bad in ["1.0.0-beta", "v1.0", "1..0", "1.0.x", ""] {
            let mut pack = Pack::parse(MIN_ENGINE_PACK).unwrap();
            pack.min_engine = Some(bad.to_string());
            let errs = pack.validate();
            assert!(
                errs.iter().any(|e| e.contains("min_engine")),
                "'{bad}' must be refused: {errs:?}"
            );
        }
    }

    #[test]
    fn the_minimum_engine_survives_a_json_round_trip() {
        let pack = Pack::parse(MIN_ENGINE_PACK).unwrap();
        let json = serde_json::to_string(&pack).expect("a pack serialises");
        assert!(json.contains("\"min_engine\":\"1.1.0\""), "the name stays: {json}");
        let again: Pack = serde_json::from_str(&json).expect("the pack parses again");
        assert_eq!(again.min_engine, pack.min_engine);
        assert_eq!(again.min_engine.as_deref(), Some("1.1.0"));

        // a pack without the field stays without it (an absent minimum is written as null,
        // the way the other optional fields of a pack are)
        let plain = MIN_ENGINE_PACK.replace("\"min_engine\": \"1.1.0\",", "");
        let pack = Pack::parse(&plain).unwrap();
        let json = serde_json::to_string(&pack).unwrap();
        let again: Pack = serde_json::from_str(&json).unwrap();
        assert!(again.min_engine.is_none(), "no minimum means none: {json}");

        // unknown fields are not part of the schema: the struct drops them (the installed
        // file keeps the original text, so nothing is lost on disk)
        let with_extra = MIN_ENGINE_PACK.replace(
            "\"min_engine\": \"1.1.0\",",
            "\"min_engine\": \"1.1.0\", \"something_new\": 7,",
        );
        let pack = Pack::parse(&with_extra).expect("an unknown field does not make a pack invalid");
        let json = serde_json::to_string(&pack).unwrap();
        assert!(!json.contains("something_new"), "{json}");
        assert_eq!(pack.min_engine.as_deref(), Some("1.1.0"));
    }
}
