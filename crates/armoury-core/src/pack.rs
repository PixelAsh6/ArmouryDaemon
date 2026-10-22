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
use std::collections::{HashMap, HashSet};

fn default_spec() -> u32 {
    1
}
fn default_order() -> i32 {
    100
}
fn default_true() -> bool {
    true
}

/// The default for a pack that does not declare its capabilities: everything.
///
/// The three shipped packs predate the field, and a pack that asked for nothing would
/// lose the host functions it uses. The user's own grant list (the permission window)
/// comes later and narrows this down.
fn default_capabilities() -> CapSet {
    CapSet::ALL
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
    /// What this pack asks the host for: the capabilities it needs. A pack that declares
    /// none gets the whole host API (see [`default_capabilities`]).
    #[serde(default = "default_capabilities")]
    pub capabilities: CapSet,
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

// ---------------------------------------------------------------------------
// The capabilities
//
// A pack declares the capabilities it needs and the daemon grants only those. A call to
// a host function of a capability that was **not** granted fails as an *unknown
// function*: the denial is structural, not a check a call site could forget.

/// One capability: a group of host functions a pack may ask the host for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Capability {
    /// Read-only context about the app the pack drives.
    Context,
    /// The pack's own journal: what it logs and what it captures.
    Journal,
    /// Read a file anywhere on the machine.
    Files,
    /// Write a file — only inside the folders a pack may write in.
    FilesWrite,
    /// Look at, kill and start processes.
    Process,
    /// Run a program.
    Exec,
    /// Inject keystrokes.
    Input,
    /// Send a message to another app's window.
    Ipc,
    /// Hide and show the windows of other apps.
    Windows,
    /// Mute and unmute audio.
    Audio,
    /// The idle overlay: black screens over the monitors.
    Overlay,
    /// Wait. A flow-control primitive: it grants nothing.
    Time,
}

/// Every capability, in the order of the table.
pub const ALL_CAPABILITIES: [Capability; 12] = [
    Capability::Context,
    Capability::Journal,
    Capability::Files,
    Capability::FilesWrite,
    Capability::Process,
    Capability::Exec,
    Capability::Input,
    Capability::Ipc,
    Capability::Windows,
    Capability::Audio,
    Capability::Overlay,
    Capability::Time,
];

impl Capability {
    /// The name a pack writes in its `capabilities` list.
    pub fn name(self) -> &'static str {
        match self {
            Capability::Context => "context",
            Capability::Journal => "journal",
            Capability::Files => "files",
            Capability::FilesWrite => "files-write",
            Capability::Process => "process",
            Capability::Exec => "exec",
            Capability::Input => "input",
            Capability::Ipc => "ipc",
            Capability::Windows => "windows",
            Capability::Audio => "audio",
            Capability::Overlay => "overlay",
            Capability::Time => "time",
        }
    }

    /// The host functions this capability carries.
    pub fn functions(self) -> &'static [&'static str] {
        match self {
            Capability::Context => &[
                "target_name",
                "target_exe",
                "target_dir",
                "setting",
                "captured",
            ],
            Capability::Journal => &["capture", "log"],
            Capability::Files => &["read_file", "read_json", "read_ini"],
            Capability::FilesWrite => &["write_file"],
            Capability::Process => &["process_running", "process_kill", "process_start"],
            Capability::Exec => &["run_cli"],
            Capability::Input => &["hotkey"],
            Capability::Ipc => &["window_message"],
            Capability::Windows => &["windows_of", "window_hide", "window_show"],
            Capability::Audio => &["audio_sessions", "audio_mute", "audio_mute_all"],
            Capability::Overlay => &["overlay_show", "overlay_hide"],
            Capability::Time => &["sleep"],
        }
    }

    /// What the capability does, in the words of a permission list: the effect, not the
    /// API. The permission window shows this, not the function names.
    pub fn effect(self) -> &'static str {
        match self {
            Capability::Context => "Read the app it drives and its own settings",
            Capability::Journal => "Write to its own journal",
            Capability::Files => "Read files",
            Capability::FilesWrite => "Write files",
            Capability::Process => "Look at, kill and start processes",
            Capability::Exec => "Run external programs",
            Capability::Input => "Send keystrokes",
            Capability::Ipc => "Talk to other apps",
            Capability::Windows => "Hide windows belonging to other apps",
            Capability::Audio => "Mute the audio of other apps",
            Capability::Overlay => "Take over the screen",
            Capability::Time => "Wait",
        }
    }

    /// The capability with this name, if the daemon knows one.
    pub fn from_name(name: &str) -> Option<Capability> {
        let name = name.trim().to_ascii_lowercase();
        ALL_CAPABILITIES.into_iter().find(|c| c.name() == name)
    }

    /// The capability a host function belongs to. `None` for a function that is not part
    /// of the host API at all (a typo in a pack's script).
    pub fn of_function(function: &str) -> Option<Capability> {
        ALL_CAPABILITIES
            .into_iter()
            .find(|c| c.functions().contains(&function))
    }

    /// The bit this capability occupies in a [`CapSet`].
    const fn bit(self) -> u32 {
        1u32 << (self as u8)
    }
}

/// The capabilities a pack was granted.
///
/// A bitmask: twelve capabilities fit in a u32, and "was this granted" is a bit test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CapSet(u32);

impl CapSet {
    /// Every capability — the default for a pack that declares none.
    pub const ALL: CapSet = CapSet(
        Capability::Context.bit()
            | Capability::Journal.bit()
            | Capability::Files.bit()
            | Capability::FilesWrite.bit()
            | Capability::Process.bit()
            | Capability::Exec.bit()
            | Capability::Input.bit()
            | Capability::Ipc.bit()
            | Capability::Windows.bit()
            | Capability::Audio.bit()
            | Capability::Overlay.bit()
            | Capability::Time.bit(),
    );

    /// The set of these capabilities.
    pub fn of(list: &[Capability]) -> CapSet {
        let mut set = CapSet::default();
        for capability in list {
            set = set.with(*capability);
        }
        set
    }

    /// Was this capability granted?
    pub fn grants(self, capability: Capability) -> bool {
        self.0 & capability.bit() != 0
    }

    /// The set with one more capability.
    pub fn with(self, capability: Capability) -> CapSet {
        CapSet(self.0 | capability.bit())
    }

    /// The set without one capability.
    pub fn without(self, capability: Capability) -> CapSet {
        CapSet(self.0 & !capability.bit())
    }

    /// Nothing granted.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// How many capabilities are granted.
    pub fn len(self) -> usize {
        self.iter().count()
    }

    /// The granted capabilities, in the order of the table.
    pub fn iter(self) -> impl Iterator<Item = Capability> {
        ALL_CAPABILITIES
            .into_iter()
            .filter(move |c| self.grants(*c))
    }

    /// The names of the granted capabilities: what a denial message lists.
    pub fn names(self) -> Vec<&'static str> {
        self.iter().map(|c| c.name()).collect()
    }
}

impl Serialize for CapSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.names().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CapSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<CapSet, D::Error> {
        let names = Vec::<String>::deserialize(deserializer)?;
        let mut set = CapSet::default();
        for name in names {
            let capability = Capability::from_name(&name).ok_or_else(|| {
                <D::Error as serde::de::Error>::custom(format!("unknown capability '{name}'"))
            })?;
            set = set.with(capability);
        }
        Ok(set)
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
        // The keys are collected in a set: comparing every key with every earlier one made
        // the check quadratic, and a pack with thousands of dropdowns (a hostile file) kept
        // the daemon busy for minutes before it could refuse it.
        let mut seen_keys: HashSet<&str> = HashSet::new();
        for s in &self.settings {
            if !seen_keys.insert(s.key.as_str()) {
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
                // A `when` that names something the pack does not have would never match, and
                // the step would vanish silently while the pack reports success.
                if let Some(when) = &step.when {
                    match when.source.as_str() {
                        "setting" => {
                            if let Some(key) = &when.key {
                                if !self.settings.iter().any(|s| &s.key == key) {
                                    errs.push(format!(
                                        "{block} step {i}: when.key '{key}' is not one of the pack's settings"
                                    ));
                                }
                            } else if self.settings.is_empty() {
                                errs.push(format!(
                                    "{block} step {i}: when.source 'setting' needs a key — the pack has no settings"
                                ));
                            }
                        }
                        "captured" => match &when.key {
                            Some(key) if self.capture.iter().any(|c| &c.key == key) => {}
                            Some(key) => errs.push(format!(
                                "{block} step {i}: when.key '{key}' is not one of the pack's captures"
                            )),
                            None => errs.push(format!(
                                "{block} step {i}: when.source 'captured' needs the key of a capture"
                            )),
                        },
                        other => errs.push(format!(
                            "{block} step {i}: when.source '{other}' is not 'setting' or 'captured'"
                        )),
                    }
                }
                // The same for a hotkey step: the map key comes from one of the pack's own
                // settings or captures, so a typo there would send nothing at all.
                if step.mechanism == "hotkey" {
                    if let Some(expr) = &step.keys_from_mode {
                        for key in settings_in(expr) {
                            if !self.settings.iter().any(|s| s.key == key) {
                                errs.push(format!(
                                    "{block} step {i}: keys_from_mode names the setting '{key}', which the pack does not have"
                                ));
                            }
                        }
                    }
                    if let Some(key) = &step.keys_from_captured
                        && !self.capture.iter().any(|c| &c.key == key)
                    {
                        errs.push(format!(
                            "{block} step {i}: keys_from_captured '{key}' is not one of the pack's captures"
                        ));
                    }
                }
            }
        }
        errs
    }

    /// Parse a pack and reject it if invalid.
    pub fn parse(text: &str) -> Result<Pack, String> {
        let pack: Pack = serde_json::from_str(text).map_err(|e| explain_json_error(text, &e))?;
        let errs = pack.validate();
        if errs.is_empty() {
            Ok(pack)
        } else {
            Err(errs.join("; "))
        }
    }
}

/// The message for a JSON that does not fit the schema.
///
/// serde_json says what is wrong and where, but not *which field* is wrong: a pack author
/// has to look for it. The field is the key the offending value belongs to, and it is found
/// by walking back from the position over the token to the key that opens it.
fn explain_json_error(text: &str, err: &serde_json::Error) -> String {
    let detail = err.to_string();
    // serde names a missing field itself.
    if detail.starts_with("missing field") {
        return format!("invalid JSON: {detail}");
    }
    match field_at(text, err.line(), err.column()) {
        Some(field) => format!("invalid JSON: the field '{field}': {detail}"),
        None => format!("invalid JSON: {detail}"),
    }
}

/// The name of the field the error at `line`/`column` is about, if the text
/// around it says so. `None` when the position does not sit in a value of a named field.
///
/// `line` is serde_json's line, 1-based. `column` is a *byte* offset inside that line —
/// serde_json counts bytes, not characters — and it is not always 1-based: the errors
/// built from `position()` count from 0, the ones built from `peek_position()` from 1.
/// The column is used as the 1-based offset serde_json documents it to be, which is the
/// place the old character index pointed at as well.
fn field_at(text: &str, line: usize, column: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut pos = offset_of(text, line, column)?;
    loop {
        // Walk back over the whitespace to the last character of the offending token.
        while pos > 0 && bytes.get(pos).is_some_and(|b| (*b as char).is_whitespace()) {
            pos -= 1;
        }
        if bytes.get(pos) != Some(&b'"') {
            // Not a quoted token: keep walking back.
            if pos == 0 {
                return None;
            }
            pos -= 1;
            continue;
        }
        // A quoted token: a key is followed by a colon, a value is not.
        let mut after = pos + 1;
        while bytes.get(after).is_some_and(|b| (*b as char).is_whitespace()) {
            after += 1;
        }
        if bytes.get(after) == Some(&b':') {
            // The key: from its opening quote to the closing one.
            let mut start = pos;
            while start > 0 {
                start -= 1;
                if bytes[start] == b'"' && (start == 0 || bytes[start - 1] != b'\\') {
                    break;
                }
            }
            if bytes.get(start) != Some(&b'"') {
                return None; // no opening quote: the text is not what it looks like
            }
            let key = text.get(start + 1..pos)?;
            if key.is_empty() || key.contains('\\') {
                return None; // an escaped key: the raw text would not be the name
            }
            return Some(key);
        }
        if pos == 0 {
            return None;
        }
        pos -= 1;
    }
}

/// The byte offset of the error in `text`: the column of `line` is a byte offset
/// inside that line.
///
/// A byte offset is not a character position: with a character before the error it can
/// land in the middle of a character, and then it is not a place in the text at all.
/// The slice is taken with `get`, which gives `None` instead of panicking.
fn offset_of(text: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0usize;
    for (index, l) in text.split_inclusive('\n').enumerate() {
        if index + 1 == line {
            let l = l.strip_suffix('\n').unwrap_or(l);
            let l = l.strip_suffix('\r').unwrap_or(l);
            let at = column.saturating_sub(1);
            // The byte has to be a character boundary: a byte offset inside a
            // character is not a position in the text.
            let _ = l.get(at..)?;
            return Some(start + at);
        }
        start += l.len();
    }
    None
}

/// The setting keys a template refers to: `{{settings.mode}}` -> ["mode"].
fn settings_in(template: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            break;
        };
        if let Some(key) = after[..end].trim().strip_prefix("settings.") {
            keys.push(key.to_string());
        }
        rest = &after[end + 2..];
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
    fn a_when_clause_must_name_something_the_pack_has() {
        // A `when` that names a setting or a capture the pack does not have would never
        // match: the step would vanish silently while the pack reports success.
        let mut pack = Pack::parse(WE).unwrap();
        pack.apply.steps[0].when = Some(When {
            source: "setting".into(),
            key: Some("nope".into()),
            equals: Some("stop".into()),
            not_equals: None,
        });
        let errs = pack.validate();
        assert!(
            errs.iter()
                .any(|e| e.contains("when.key 'nope'") && e.contains("settings")),
            "{errs:?}"
        );

        // an unknown source never matches either
        let mut pack = Pack::parse(WE).unwrap();
        pack.apply.steps[0].when = Some(When {
            source: "weather".into(),
            key: None,
            equals: Some("sunny".into()),
            not_equals: None,
        });
        assert!(pack
            .validate()
            .iter()
            .any(|e| e.contains("when.source 'weather'")));

        // a captured value the pack never captures
        let mut pack = Pack::parse(GH).unwrap();
        pack.revert.steps[0].when = Some(When {
            source: "captured".into(),
            key: Some("prev_moide".into()),
            equals: Some("1".into()),
            not_equals: None,
        });
        assert!(pack
            .validate()
            .iter()
            .any(|e| e.contains("when.key 'prev_moide'") && e.contains("captures")));

        // a captured `when` without a key can never match
        let mut pack = Pack::parse(GH).unwrap();
        pack.revert.steps[0].when = Some(When {
            source: "captured".into(),
            key: None,
            equals: Some("1".into()),
            not_equals: None,
        });
        assert!(pack
            .validate()
            .iter()
            .any(|e| e.contains("when.source 'captured' needs the key")));
    }

    #[test]
    fn a_hotkey_step_must_take_its_key_from_the_packs_own_settings_or_captures() {
        // keys_from_mode names a setting: a typo there sends nothing at all
        let mut pack = Pack::parse(GH).unwrap();
        pack.apply.steps[0].keys_from_mode = Some("{{settings.mod}}".into());
        let errs = pack.validate();
        assert!(
            errs.iter()
                .any(|e| e.contains("keys_from_mode") && e.contains("'mod'")),
            "{errs:?}"
        );

        // keys_from_captured names a capture
        let mut pack = Pack::parse(GH).unwrap();
        pack.revert.steps[0].keys_from_captured = Some("prev_moide".into());
        let errs = pack.validate();
        assert!(
            errs.iter()
                .any(|e| e.contains("keys_from_captured 'prev_moide'")),
            "{errs:?}"
        );

        // the shipped packs still pass
        for text in [WE, GH, RM] {
            let pack = Pack::parse(text).unwrap();
            assert!(pack.validate().is_empty(), "{}", pack.id);
        }
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

    #[test]
    fn a_json_error_names_the_field_even_after_a_non_ascii_value() {
        // The column of a JSON error is a byte offset, not a character index: a
        // non-ASCII value before the error shifts it, and the field the error is
        // about still has to be named. The last field here is a decoy: the error is
        // about "name", and the walk back from the end of the line finds "v".
        let text = r#"{"id": "日本語日本語", "name": 42, "v": 1}"#;
        let err = Pack::parse(text).expect_err("the name is not a string");
        assert!(
            err.contains("the field 'name'"),
            "the error has to name the field it is about: {err}"
        );
        assert!(
            !err.contains("the field 'v'"),
            "the error must not name another field: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // The validator against hostile input
    //
    // `Pack::parse` is the daemon's front door: every pack file and every pack of the
    // catalogue goes through it. It has to be total — a panic here takes the daemon down
    // while it loads a pack — and it has to refuse what it should refuse.

    /// One mutation of a good pack: what was changed and what has to come out of it.
    struct Mutation {
        label: &'static str,
        text: String,
        /// The message has to name this: the field, the value or the limit.
        names: &'static str,
        /// true = the pack has to be refused, false = it may be accepted.
        must_refuse: bool,
    }

    /// A pack as a tree, so a mutation does not have to be a text surgery.
    fn tree_of(text: &str) -> serde_json::Value {
        serde_json::from_str(text).expect("the pack is JSON")
    }

    /// The pack after a change to its tree.
    fn mutated(base: &str, change: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut tree = tree_of(base);
        change(&mut tree);
        serde_json::to_string(&tree).expect("the tree is JSON")
    }

    /// Set one field of the pack tree. The path names objects and arrays:
    /// `["apply", "steps", "0", "mechanism"]` is the mechanism of the first apply step.
    fn set(tree: &mut serde_json::Value, path: &[&str], value: serde_json::Value) {
        let mut node = tree;
        for key in &path[..path.len() - 1] {
            node = match key.parse::<usize>() {
                Ok(index) => node.get_mut(index).expect("the path exists"),
                Err(_) => node.get_mut(*key).expect("the path exists"),
            };
        }
        let last = path[path.len() - 1];
        match last.parse::<usize>() {
            Ok(index) => *node.get_mut(index).expect("the path exists") = value,
            Err(_) => {
                node.as_object_mut()
                    .expect("an object")
                    .insert(last.to_string(), value);
            }
        }
    }

    /// Remove one field of the pack tree.
    fn remove(tree: &mut serde_json::Value, path: &[&str]) {
        let mut node = tree;
        for key in &path[..path.len() - 1] {
            node = match key.parse::<usize>() {
                Ok(index) => node.get_mut(index).expect("the path exists"),
                Err(_) => node.get_mut(*key).expect("the path exists"),
            };
        }
        let last = path[path.len() - 1];
        match last.parse::<usize>() {
            Ok(index) => {
                node.as_array_mut().expect("an array").remove(index);
            }
            Err(_) => {
                node.as_object_mut().expect("an object").remove(last);
            }
        }
    }

    /// A small pack with the numeric fields the shipped ones do not have.
    const NUMERIC_PACK: &str = r#"{
        "spec": 1, "id": "numeric", "name": "Numeric", "version": "1.0.0",
        "targets": { "process": ["numeric.exe"] },
        "apply": { "steps": [ { "mechanism": "registry_set", "sleep_ms": 5,
            "registry": { "path": "HKEY_CURRENT_USER\\Software\\Numeric", "name": "flag", "dword": 1 } } ] },
        "revert": { "steps": [ { "mechanism": "cli", "exe": "numeric.exe", "args": [], "expect_exit": 0 } ] }
    }"#;

    /// Every way a pack author (or a hostile file) can get the Rainmeter pack wrong.
    fn mutations() -> Vec<Mutation> {
        let mut list: Vec<Mutation> = Vec::new();
        macro_rules! mutation {
            ($label:literal, $text:expr, $names:literal, $refuse:literal) => {
                list.push(Mutation {
                    label: $label,
                    text: $text,
                    names: $names,
                    must_refuse: $refuse,
                });
            };
        }

        // a wrong JSON type where a string is expected
        mutation!(
            "the id is a number",
            mutated(RM, |t| set(t, &["id"], serde_json::json!(42))),
            "id",
            true
        );
        // a list where a string is expected
        mutation!(
            "the id is a list",
            mutated(RM, |t| set(t, &["id"], serde_json::json!(["rainmeter"]))),
            "id",
            true
        );
        // a missing field
        mutation!(
            "the name is missing",
            mutated(RM, |t| remove(t, &["name"])),
            "name",
            true
        );
        // a null
        mutation!(
            "the name is null",
            mutated(RM, |t| set(t, &["name"], serde_json::Value::Null)),
            "name",
            true
        );
        // an empty string
        mutation!(
            "the id is empty",
            mutated(RM, |t| set(t, &["id"], serde_json::json!(""))),
            "id",
            true
        );
        // a string where a number is expected
        mutation!(
            "the order is a string",
            mutated(RM, |t| set(t, &["order"], serde_json::json!("30"))),
            "order",
            true
        );
        // a huge number
        mutation!(
            "the order does not fit a number",
            RM.replace(r#""order": 30"#, r#""order": 1e400"#),
            "order",
            true
        );
        // a negative number: the order is a sort key, and any integer is one
        mutation!(
            "the order is negative",
            mutated(RM, |t| set(t, &["order"], serde_json::json!(-30))),
            "",
            false
        );
        // a list that is a string
        mutation!(
            "the process list is a string",
            mutated(RM, |t| set(
                t,
                &["targets", "process"],
                serde_json::json!("Rainmeter.exe")
            )),
            "process",
            true
        );
        // a list that is null
        mutation!(
            "the process list is null",
            mutated(RM, |t| set(t, &["targets", "process"], serde_json::Value::Null)),
            "process",
            true
        );
        // a boolean that is a string
        mutation!(
            "a guard flag is a string",
            mutated(RM, |t| set(
                t,
                &["apply", "guard", "targetRunning"],
                serde_json::json!("yes")
            )),
            "targetRunning",
            true
        );
        // a deeply nested value
        mutation!(
            "the id is a deeply nested value",
            RM.replace(
                r#""id": "rainmeter""#,
                &format!(r#""id": {}{}"#, "[".repeat(300), "]".repeat(300))
            ),
            "id",
            true
        );
        // an absurdly long string
        mutation!(
            "the name is an absurdly long string",
            mutated(RM, |t| set(
                t,
                &["name"],
                serde_json::json!("A".repeat(200_000))
            )),
            "",
            false
        );
        // a min_engine that is not a dotted version
        mutation!(
            "the min_engine is not a dotted version",
            mutated(RM, |t| set(t, &["min_engine"], serde_json::json!("one.two"))),
            "min_engine",
            true
        );
        // a min_engine with a part that does not fit a number
        mutation!(
            "the min_engine has a part that is too large",
            mutated(
                RM,
                |t| set(t, &["min_engine"], serde_json::json!("99999999999999999999.0"))
            ),
            "min_engine",
            true
        );
        // a setting value that is not in the dropdown's list
        mutation!(
            "the default is not one of the values",
            mutated(RM, |t| set(
                t,
                &["settings", "0", "default"],
                serde_json::json!("Nope")
            )),
            "layout",
            true
        );
        // more than four dropdowns
        mutation!(
            "there are five dropdowns",
            mutated(RM, |t| {
                let settings = t
                    .get_mut("settings")
                    .and_then(|s| s.as_array_mut())
                    .expect("the dropdowns");
                for i in 0..4 {
                    let mut extra = settings[0].clone();
                    extra.as_object_mut().expect("an object").insert(
                        "key".into(),
                        serde_json::json!(format!("extra{i}")),
                    );
                    settings.push(extra);
                }
            }),
            "dropdown",
            true
        );
        // a spec the daemon does not understand
        mutation!(
            "the spec is one the daemon does not know",
            mutated(RM, |t| set(t, &["spec"], serde_json::json!(3))),
            "spec",
            true
        );
        // a script on a spec-1 pack
        mutation!(
            "a script on a spec-1 pack",
            mutated(RM, |t| set(t, &["spec"], serde_json::json!(1))),
            "spec",
            true
        );
        // a spec-2 pack with no script
        mutation!(
            "a spec-2 pack without a script",
            mutated(RM, |t| remove(t, &["script"])),
            "spec",
            true
        );
        // a script that is not an object
        mutation!(
            "the script is not an object",
            mutated(RM, |t| set(t, &["script"], serde_json::json!("hello"))),
            "script",
            true
        );
        // a duplicated key: serde refuses the second one
        mutation!(
            "the id is given twice",
            RM.replace(r#""id": "rainmeter""#, r#""id": "rainmeter", "id": "other""#),
            "id",
            true
        );
        // a mechanism the daemon does not know
        mutation!(
            "a step mechanism that does not exist",
            mutated(RM, |t| set(
                t,
                &["apply", "steps", "0", "mechanism"],
                serde_json::json!("exec_shell")
            )),
            "mechanism",
            true
        );
        // a step without a mechanism at all
        mutation!(
            "a step without a mechanism",
            mutated(RM, |t| remove(
                t,
                &["apply", "steps", "0", "mechanism"]
            )),
            "mechanism",
            true
        );
        // a capture mechanism the daemon does not know
        mutation!(
            "a capture mechanism that does not exist",
            mutated(RM, |t| set(
                t,
                &["capture", "0", "mechanism"],
                serde_json::json!("read_the_registry")
            )),
            "mechanism",
            true
        );
        // a null where a list of dropdowns is expected
        mutation!(
            "the dropdowns are null",
            mutated(RM, |t| set(t, &["settings"], serde_json::Value::Null)),
            "settings",
            true
        );

        // the numbers the shipped packs do not have
        mutation!(
            "the dword is negative",
            mutated(NUMERIC_PACK, |t| set(
                t,
                &["apply", "steps", "0", "registry", "dword"],
                serde_json::json!(-1)
            )),
            "dword",
            true
        );
        mutation!(
            "the dword does not fit",
            mutated(NUMERIC_PACK, |t| set(
                t,
                &["apply", "steps", "0", "registry", "dword"],
                serde_json::json!(4_294_967_296u64)
            )),
            "dword",
            true
        );
        mutation!(
            "the sleep is negative",
            mutated(NUMERIC_PACK, |t| set(
                t,
                &["apply", "steps", "0", "sleep_ms"],
                serde_json::json!(-1)
            )),
            "sleep_ms",
            true
        );
        mutation!(
            "the expected exit code does not fit",
            mutated(NUMERIC_PACK, |t| set(
                t,
                &["revert", "steps", "0", "expect_exit"],
                serde_json::json!(-2_147_483_649i64)
            )),
            "expect_exit",
            true
        );

        list
    }

    #[test]
    fn the_validator_survives_a_hostile_pack() {
        let list = mutations();
        assert!(list.len() >= 30, "the battery is complete: {}", list.len());
        for m in &list {
            let outcome = std::panic::catch_unwind(|| Pack::parse(&m.text));
            let parsed = match outcome {
                Ok(parsed) => parsed,
                Err(_) => panic!("'{}' took the validator down", m.label),
            };
            match parsed {
                Ok(pack) => {
                    assert!(
                        !m.must_refuse,
                        "'{}' has to be refused, but it was accepted",
                        m.label
                    );
                    assert!(
                        pack.validate().is_empty(),
                        "'{}' was accepted but is not valid",
                        m.label
                    );
                }
                Err(message) => {
                    assert!(
                        m.must_refuse,
                        "'{}' has to be refused, but it was accepted: {message}",
                        m.label
                    );
                    assert!(!message.is_empty(), "'{}' was refused without a reason", m.label);
                    if !m.names.is_empty() {
                        assert!(
                            message.contains(m.names),
                            "'{}' has to name '{}': {message}",
                            m.label,
                            m.names
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_pack_with_thousands_of_dropdowns_is_refused_without_hanging() {
        // The duplicate-key check compared every key with every earlier one, so a pack with
        // thousands of dropdowns (a hostile file) kept the daemon busy for minutes before it
        // could refuse it. The check is linear now; this is the margin that proves it.
        let mut pack = Pack::parse(RM).expect("the shipped pack is valid");
        for i in 0..200_000 {
            pack.settings.push(Setting {
                key: format!("extra{i:06}"),
                label: "Extra".into(),
                kind: "enum".into(),
                values: vec!["a".into()],
                default: None,
            });
        }
        let started = Instant::now();
        let errs = pack.validate();
        let elapsed = started.elapsed();
        assert!(errs.iter().any(|e| e.contains("at most 4 dropdowns")));
        assert!(
            elapsed < Duration::from_secs(5),
            "the validator took {elapsed:?} on a pack with 200 000 dropdowns"
        );
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

    // -----------------------------------------------------------------------
    // The capabilities

    /// A pack with a `capabilities` list and nothing else that matters.
    const CAPABILITY_PACK: &str = r#"{
        "spec": 2, "id": "caps", "name": "Caps", "version": "1.0.0",
        "targets": { "process": ["caps.exe"] },
        "capabilities": ["files", "exec"],
        "script": { "apply": "ctx.log(\"hi\");", "revert": "ctx.log(\"bye\");" },
        "apply": { "steps": [] }, "revert": { "steps": [] }
    }"#;

    #[test]
    fn a_pack_that_declares_nothing_gets_the_whole_host_api() {
        // The three shipped packs predate the field: they must keep working unchanged.
        let pack = Pack::parse(RM).expect("the Rainmeter pack must be valid");
        assert_eq!(pack.capabilities, CapSet::ALL);
        assert!(pack.capabilities.grants(Capability::Ipc));
        assert!(pack.capabilities.grants(Capability::Time));
        for text in [WE, GH] {
            let pack = Pack::parse(text).unwrap();
            assert_eq!(pack.capabilities, CapSet::ALL);
        }
    }

    #[test]
    fn a_pack_declares_the_capabilities_it_asks_for() {
        let pack = Pack::parse(CAPABILITY_PACK).expect("the pack must be valid");
        assert!(pack.capabilities.grants(Capability::Files));
        assert!(pack.capabilities.grants(Capability::Exec));
        assert!(!pack.capabilities.grants(Capability::Process));
        assert!(!pack.capabilities.grants(Capability::Audio));
        assert_eq!(pack.capabilities.len(), 2);
        assert_eq!(pack.capabilities.names(), vec!["files", "exec"]);
    }

    #[test]
    fn an_unknown_capability_is_a_named_error() {
        let text = CAPABILITY_PACK.replace("\"files\", \"exec\"", "\"files\", \"teleport\"");
        let err = Pack::parse(&text).expect_err("an unknown capability must be refused");
        assert!(err.contains("teleport"), "{err}");
        assert!(err.contains("capabilit"), "{err}");
    }

    #[test]
    fn the_capabilities_survive_a_json_round_trip() {
        let pack = Pack::parse(CAPABILITY_PACK).unwrap();
        let json = serde_json::to_string(&pack).expect("a pack serialises");
        assert!(json.contains("\"capabilities\":[\"files\",\"exec\"]"), "{json}");
        let again: Pack = serde_json::from_str(&json).expect("the pack parses again");
        assert_eq!(again.capabilities, pack.capabilities);
    }

    #[test]
    fn every_capability_names_its_own_functions() {
        // The table is the contract between the schema and the host API: a function that
        // belongs to no capability could never be granted, and a function that belongs to
        // two could be denied by one and granted by the other.
        let mut seen: Vec<&str> = Vec::new();
        for capability in ALL_CAPABILITIES {
            assert!(!capability.functions().is_empty(), "{capability:?} has no functions");
            for function in capability.functions() {
                assert!(
                    !seen.contains(function),
                    "'{function}' is in two capabilities"
                );
                seen.push(function);
                assert_eq!(
                    Capability::of_function(function),
                    Some(capability),
                    "'{function}' does not point back at its capability"
                );
            }
            assert_eq!(Capability::from_name(capability.name()), Some(capability));
        }
        // the host API has exactly these functions
        assert_eq!(seen.len(), 26, "{seen:?}");
        assert!(!seen.contains(&"no_such_host_function"));
    }

    #[test]
    fn a_capability_set_is_a_set() {
        let one = CapSet::default().with(Capability::Files);
        let two = one.with(Capability::Files);
        assert_eq!(one, two, "the same capability twice is the same set");
        assert!(one.grants(Capability::Files));
        assert!(!one.grants(Capability::Exec));
        assert!(!one.without(Capability::Files).grants(Capability::Files));
        assert!(CapSet::default().is_empty());
        assert!(!CapSet::ALL.is_empty());
        assert_eq!(CapSet::ALL.len(), ALL_CAPABILITIES.len());
    }
}
