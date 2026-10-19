use crate::status::StatusSnapshot;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{self, BufRead, Write};

/// Messages sent from a client (UI / second daemon instance) to the daemon.
/// Wire format: one JSON object per line (JSON Lines, UTF-8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    UiHello { pid: u32 },
    /// The console launcher says hello. Kept apart from `UiHello` so the daemon never focuses or
    /// kills it as if it were the dashboard.
    ConsoleHello { pid: u32 },
    GetStatus,
    ShowUi,
    AddTrackedApp { name: String, exe: String },
    RemoveTrackedApp { exe: String },
    /// Pause / resume all pack switching.
    SetPaused { paused: bool },
    /// Enable or disable one pack.
    SetPackEnabled { id: String, enabled: bool },
    /// Set a pack's dropdown value.
    SetPackValue { id: String, key: String, value: String },
    /// Enter / leave idle mode (black overlay + mute every output device).
    SetIdle { active: bool },
    /// Configure the idle failsafe.
    SetIdleFailsafe { enabled: bool, minutes: u64 },
    /// Change the revert cooldown.
    SetCooldown { seconds: u64 },
    /// The user moved the dashboard: remember its top-left (logical points).
    SetUiPosition { x: f32, y: f32 },
    /// Install a pack from a local `pack.json` (or a folder holding one).
    InstallPack { source: String },
    /// Remove an installed pack (its folder under `<install>\packs`).
    RemovePack { id: String },
    /// Turn console mode on/off (a controller connect then opens the launcher).
    SetConsoleEnabled { enabled: bool },
    /// Pin an app to the console launcher (not tracked).
    AddConsoleApp { name: String, exe: String },
    /// Unpin an app from the console launcher.
    RemoveConsoleApp { exe: String },
    /// Launch an app from the console; the daemon does the spawning and logging.
    LaunchApp { exe: String },
    /// Open the console launcher now (tray menu / settings preview), pad or not.
    ShowConsole,
}

/// Messages sent from the daemon to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    HelloAck {
        daemon_version: String,
        status: StatusSnapshot,
    },
    Status {
        status: StatusSnapshot,
    },
    Focus,
    /// Result of an install/remove request, for the settings window.
    PackOp { ok: bool, message: String },
    /// Result of a console-app request, for the settings window.
    ConsoleOp { ok: bool, message: String },
}

/// Serialize `msg` as one JSON line and write it (flushing immediately).
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(msg).map_err(io::Error::other)?;
    buf.push(b'\n');
    w.write_all(&buf)?;
    w.flush()
}

/// Read one JSON line. Returns Ok(None) on clean EOF.
/// Blank lines are skipped; malformed JSON is an error (caller drops the connection).
pub fn read_msg<R: BufRead, T: DeserializeOwned>(
    r: &mut R,
    buf: &mut String,
) -> io::Result<Option<T>> {
    loop {
        buf.clear();
        let n = r.read_line(buf)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = buf.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        return serde_json::from_str(trimmed).map(Some).map_err(io::Error::other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::RunState;

    #[test]
    fn roundtrip_request() {
        let req = Request::UiHello { pid: 42 };
        let mut out = Vec::new();
        write_msg(&mut out, &req).unwrap();
        let back: Request = read_msg(&mut &out[..], &mut String::new()).unwrap().unwrap();
        assert!(matches!(back, Request::UiHello { pid: 42 }));
    }

    #[test]
    fn roundtrip_pack_ops() {
        let mut out = Vec::new();
        write_msg(
            &mut out,
            &Request::InstallPack {
                source: "C:\\x\\pack.json".into(),
            },
        )
        .unwrap();
        write_msg(
            &mut out,
            &Response::PackOp {
                ok: false,
                message: "nope".into(),
            },
        )
        .unwrap();
        let mut reader = &out[..];
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(req, Request::InstallPack { .. }));
        let resp: Response = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(resp, Response::PackOp { ok: false, .. }));
    }

    #[test]
    fn roundtrip_ui_position() {
        let mut out = Vec::new();
        write_msg(
            &mut out,
            &Request::SetUiPosition {
                x: 10.5,
                y: -20.0,
            },
        )
        .unwrap();
        let back: Request = read_msg(&mut &out[..], &mut String::new()).unwrap().unwrap();
        match back {
            Request::SetUiPosition { x, y } => {
                assert!((x - 10.5).abs() < 0.001 && (y + 20.0).abs() < 0.001);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_response() {
        let resp = Response::Status {
            status: StatusSnapshot {
                daemon_version: "0.1.0".into(),
                uptime_secs: 7,
                state: RunState::Idle,
                trigger_source: None,
                cooldown_remaining_ms: None,
                cooldown_action: None,
                cooldown_seconds: 60,
                paused: false,
                killswitch_active: false,
                killswitch_count: 0,
                idle_active: false,
                idle_monitors: 0,
                idle_devices: 0,
                idle_failsafe_enabled: true,
                idle_failsafe_minutes: 180,
                tracked_apps: vec![],
                tracked_apps_running: vec![],
                packs: vec![],
                console_enabled: false,
                console_apps: vec![],
            },
        };
        let mut out = Vec::new();
        write_msg(&mut out, &resp).unwrap();
        let back: Response = read_msg(&mut &out[..], &mut String::new()).unwrap().unwrap();
        match back {
            Response::Status { status } => {
                assert_eq!(status.uptime_secs, 7);
                assert_eq!(status.state, RunState::Idle);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_console_requests() {
        let mut out = Vec::new();
        write_msg(
            &mut out,
            &Request::AddConsoleApp {
                name: "Notepad".into(),
                exe: "C:\\Windows\\notepad.exe".into(),
            },
        )
        .unwrap();
        write_msg(&mut out, &Request::ShowConsole).unwrap();
        write_msg(
            &mut out,
            &Response::ConsoleOp {
                ok: true,
                message: "added".into(),
            },
        )
        .unwrap();
        let mut reader = &out[..];
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(req, Request::AddConsoleApp { .. }));
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(req, Request::ShowConsole));
        let resp: Response = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(resp, Response::ConsoleOp { ok: true, .. }));
    }

    #[test]
    fn eof_is_none() {
        let back: Option<Request> = read_msg(&mut &b""[..], &mut String::new()).unwrap();
        assert!(back.is_none());
    }

    #[test]
    fn blank_lines_skipped() {
        let data = b"\n\r\n{\"type\":\"get_status\"}\n";
        let back: Request = read_msg(&mut &data[..], &mut String::new()).unwrap().unwrap();
        assert!(matches!(back, Request::GetStatus));
    }
}
