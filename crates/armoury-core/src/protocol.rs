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
    /// Enable or disable the killswitch. `false` is the config-level off: the toggle turns
    /// into a no-op and, if the killswitch is engaged at that moment, it releases first.
    SetKillswitch { enabled: bool },
    /// Install a pack from a local `pack.json` (or a folder holding one).
    InstallPack { source: String },
    /// Install a helper module from an archive the catalogue download verified and left
    /// in temp. The daemon re-verifies the file's bytes against the signed manifest's
    /// `sha256` and `size` after it reads them, then extracts into `modules/<id>/`;
    /// `entry` is the manifest's executable path inside the archive. A pack install and
    /// a helper install differ in where the bytes go, so they are different requests.
    InstallHelper {
        id: String,
        name: String,
        version: String,
        entry: String,
        file: String,
        sha256: String,
        size: Option<u64>,
    },
    /// Remove an installed pack (its folder under `<install>\packs`).
    RemovePack { id: String },
    /// Console launcher auto-open: `always` (daemon startup), `pad` (controller connect),
    /// `never`. The tray's manual open is never affected.
    SetConsoleOpen { mode: String },
    /// SteamGridDB API key for cover/hero art (empty string clears it).
    SetSgdbKey { key: String },
    /// Pin an app to the console launcher (not tracked).
    AddConsoleApp { name: String, exe: String },
    /// Unpin an app from the console launcher.
    RemoveConsoleApp { exe: String },
    /// Keep a tracked app out of the console band (it stays tracked and triggers as usual).
    HideFromConsole { exe: String },
    /// Launch an app from the console; the daemon does the spawning and logging.
    LaunchApp { exe: String },
    /// List the artwork SteamGridDB offers for one app and one slot (page 0 is the first).
    ListArt { exe: String, slot: ArtSlot, page: u32 },
    /// Choose the artwork for one slot of an app: a photo url, or an empty url for
    /// "use automatic again".
    SetArt { exe: String, slot: ArtSlot, url: String },
    /// Open the console launcher now (tray menu / settings preview), pad or not.
    ShowConsole,
    /// Quit the daemon: revert the packs, then exit. Same as the tray menu's "Quit".
    Quit,
}

/// The two artwork slots the band shows and the picker chooses between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtSlot {
    /// The 2:3 portrait tile.
    Icon,
    /// The wide band background.
    Background,
}

/// One artwork photo SteamGridDB offers for a game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtPhoto {
    /// The photo's id in the SteamGridDB catalogue.
    pub id: u32,
    /// The full-size image.
    pub url: String,
    /// The small preview image.
    pub thumb: String,
    pub width: u32,
    pub height: u32,
    /// The photo's style: "Alternate", "White Logo", "No Logo", "Material"…
    #[serde(default)]
    pub style: String,
}

/// A page of artwork photos for one slot of one game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtListing {
    /// Did the daemon get a listing at all?
    pub ok: bool,
    /// The slot the listing is for.
    pub slot: ArtSlot,
    /// The page (0 is the first).
    pub page: u32,
    /// How many photos the game has in this slot.
    pub total: u32,
    /// The photos of the page.
    pub items: Vec<ArtPhoto>,
    /// Why there is no listing (empty when `ok`).
    #[serde(default)]
    pub error: String,
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

/// The marker that turns a `ConsoleOp` into the answer to a launch request. The console-app
/// ops (`added`, `removed`, `hidden`) share the variant, so the band has to tell them apart.
pub const LAUNCH_ACK: &str = "launch: ";

/// The answer to a launch request: did the game start, and if not, why not.
///
/// It rides the `ConsoleOp` variant: the `Response` enum is matched exhaustively by the
/// dashboard, so a new variant would need a second change in a file the launcher does not
/// own. The marker keeps the two apart.
pub fn launch_ack(ok: bool, message: &str) -> Response {
    Response::ConsoleOp {
        ok,
        message: format!("{LAUNCH_ACK}{message}"),
    }
}

/// The launch answer inside a response, if this response is one.
pub fn as_launch_ack(resp: &Response) -> Option<(bool, &str)> {
    match resp {
        Response::ConsoleOp { ok, message } => message
            .strip_prefix(LAUNCH_ACK)
            .map(|text| (*ok, text)),
        _ => None,
    }
}

/// The marker that turns a `ConsoleOp` into the answer to an artwork-list request. The
/// photos travel as JSON in the message.
pub const ART_LIST: &str = "art: ";

/// The marker that turns a `ConsoleOp` into the answer to an artwork-set request.
pub const ART_SET: &str = "artset: ";

/// The photos of one page, as the answer to an artwork-list request.
///
/// It rides the `ConsoleOp` variant for the same reason the launch answer does: the
/// `Response` enum is matched exhaustively by the dashboard, so a new variant would need
/// a second change in a file the launcher does not own. The marker keeps the two apart.
pub fn art_listing(listing: &ArtListing) -> Response {
    let json = serde_json::to_string(listing).unwrap_or_default();
    Response::ConsoleOp {
        ok: listing.ok,
        message: format!("{ART_LIST}{json}"),
    }
}

/// The listing inside a response, if this response is one.
pub fn as_art_listing(resp: &Response) -> Option<ArtListing> {
    match resp {
        Response::ConsoleOp { message, .. } => {
            let json = message.strip_prefix(ART_LIST)?;
            serde_json::from_str(json).ok()
        }
        _ => None,
    }
}

/// The answer to an artwork-set request: did the pick land?
pub fn art_set(ok: bool, message: &str) -> Response {
    Response::ConsoleOp {
        ok,
        message: format!("{ART_SET}{message}"),
    }
}

/// The set answer inside a response, if this response is one.
pub fn as_art_set(resp: &Response) -> Option<(bool, &str)> {
    match resp {
        Response::ConsoleOp { ok, message } => message.strip_prefix(ART_SET).map(|text| (*ok, text)),
        _ => None,
    }
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
        write_msg(
            &mut out,
            &Request::SetSgdbKey {
                key: "abc123".into(),
            },
        )
        .unwrap();
        write_msg(&mut out, &Request::SetKillswitch { enabled: false }).unwrap();
        let mut reader = &out[..];
        let back: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(back, Request::UiHello { pid: 42 }));
        let back: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(back, Request::SetSgdbKey { ref key } if key == "abc123"));
        let back: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(back, Request::SetKillswitch { enabled: false }));
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
            &Request::InstallHelper {
                id: "h-harness".into(),
                name: "Harness helper".into(),
                version: "0.99.0".into(),
                entry: "ArmouryConsole.exe".into(),
                file: "C:\\temp\\armoury-catalog-h-harness.zip".into(),
                sha256: "308aeb91".into(),
                size: Some(322),
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
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(
            req,
            Request::InstallHelper {
                ref id,
                ref sha256,
                size: Some(322),
            } if id == "h-harness" && sha256 == "308aeb91"
        ));
        let resp: Response = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(resp, Response::PackOp { ok: false, .. }));
    }

    #[test]
    fn roundtrip_response() {
        let resp = Response::Status {
            status: StatusSnapshot {
                daemon_version: "1.0.0".into(),
                uptime_secs: 7,
                state: RunState::Idle,
                trigger_source: None,
                cooldown_remaining_ms: None,
                cooldown_action: None,
                cooldown_seconds: 60,
                paused: false,
                killswitch_active: false,
                killswitch_count: 0,
                killswitch_enabled: false,
                idle_active: false,
                idle_monitors: 0,
                idle_devices: 0,
                idle_failsafe_enabled: true,
                idle_failsafe_minutes: 180,
                tracked_apps: vec![],
                tracked_apps_running: vec![],
                packs: vec![],
                console_open: "pad".into(),
                console_apps: vec![],
                console_hidden: vec![],
                last_played: Default::default(),
                sgdb_api_key_set: false,
                sgdb_api_key: String::new(),
                last_error: None,
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
            &Request::SetConsoleOpen {
                mode: "never".into(),
            },
        )
        .unwrap();
        write_msg(
            &mut out,
            &Request::HideFromConsole {
                exe: "C:\\Games\\doom.exe".into(),
            },
        )
        .unwrap();
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
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(req, Request::SetConsoleOpen { ref mode } if mode == "never"));
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(req, Request::HideFromConsole { .. }));
        let resp: Response = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        assert!(matches!(resp, Response::ConsoleOp { ok: true, .. }));
    }

    #[test]
    fn the_launch_answer_travels_as_a_console_op_with_a_marker() {
        let ack = launch_ack(false, "the game did not start");
        assert!(matches!(ack, Response::ConsoleOp { ok: false, .. }));
        let (ok, message) = as_launch_ack(&ack).expect("the launch answer");
        assert!(!ok);
        assert_eq!(message, "the game did not start");

        // a plain console op is not a launch answer
        let op = Response::ConsoleOp {
            ok: true,
            message: "hidden".into(),
        };
        assert!(as_launch_ack(&op).is_none());

        // and it survives the wire
        let mut out = Vec::new();
        write_msg(&mut out, &launch_ack(true, "started (pid 42)")).unwrap();
        let back: Response = read_msg(&mut &out[..], &mut String::new()).unwrap().unwrap();
        let (ok, message) = as_launch_ack(&back).expect("the launch answer");
        assert!(ok);
        assert_eq!(message, "started (pid 42)");
    }

    /// A listing to send over the wire.
    fn a_listing() -> ArtListing {
        ArtListing {
            ok: true,
            slot: ArtSlot::Icon,
            page: 0,
            total: 2,
            items: vec![ArtPhoto {
                id: 7,
                url: "https://cdn.example/grid.png".into(),
                thumb: "https://cdn.example/thumb.png".into(),
                width: 600,
                height: 900,
                style: "Alternate".into(),
            }],
            error: String::new(),
        }
    }

    #[test]
    fn the_artwork_listing_travels_as_a_console_op_with_a_marker() {
        let resp = art_listing(&a_listing());
        assert!(matches!(resp, Response::ConsoleOp { ok: true, .. }));
        let back = as_art_listing(&resp).expect("the listing");
        assert_eq!(back.slot, ArtSlot::Icon);
        assert_eq!(back.total, 2);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].width, 600);
        assert_eq!(back.items[0].style, "Alternate");

        // a plain console op is not a listing
        let op = Response::ConsoleOp {
            ok: true,
            message: "added".into(),
        };
        assert!(as_art_listing(&op).is_none());

        // and it survives the wire
        let mut out = Vec::new();
        write_msg(&mut out, &resp).unwrap();
        let back: Response = read_msg(&mut &out[..], &mut String::new()).unwrap().unwrap();
        let back = as_art_listing(&back).expect("the listing");
        assert_eq!(back.page, 0);
        assert_eq!(back.items[0].id, 7);
        assert_eq!(back.items[0].thumb, "https://cdn.example/thumb.png");
    }

    #[test]
    fn the_artwork_set_answer_travels_as_a_console_op_with_a_marker() {
        let resp = art_set(true, "saved");
        let (ok, message) = as_art_set(&resp).expect("the set answer");
        assert!(ok);
        assert_eq!(message, "saved");

        // a plain console op is not a set answer
        let op = Response::ConsoleOp {
            ok: true,
            message: "removed".into(),
        };
        assert!(as_art_set(&op).is_none());

        // and it survives the wire
        let mut out = Vec::new();
        write_msg(&mut out, &resp).unwrap();
        let back: Response = read_msg(&mut &out[..], &mut String::new()).unwrap().unwrap();
        let (ok, message) = as_art_set(&back).expect("the set answer");
        assert!(ok);
        assert_eq!(message, "saved");
    }

    #[test]
    fn the_markers_do_not_collide() {
        // the launch answer is not an artwork answer, and the other way round
        let launch = launch_ack(true, "started");
        assert!(as_art_listing(&launch).is_none());
        assert!(as_art_set(&launch).is_none());
        assert!(as_launch_ack(&art_listing(&a_listing())).is_none());
        assert!(as_launch_ack(&art_set(true, "saved")).is_none());
        // "art: " and "artset: " are not prefixes of one another
        assert!(as_art_listing(&art_set(true, "saved")).is_none());
        assert!(as_art_set(&art_listing(&a_listing())).is_none());
    }

    #[test]
    fn the_artwork_requests_round_trip() {
        let mut out = Vec::new();
        write_msg(
            &mut out,
            &Request::ListArt {
                exe: "C:\\Games\\doom.exe".into(),
                slot: ArtSlot::Background,
                page: 2,
            },
        )
        .unwrap();
        write_msg(
            &mut out,
            &Request::SetArt {
                exe: "C:\\Games\\doom.exe".into(),
                slot: ArtSlot::Icon,
                url: String::new(),
            },
        )
        .unwrap();
        let mut reader = &out[..];
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        match req {
            Request::ListArt { slot, page, .. } => {
                assert_eq!(slot, ArtSlot::Background);
                assert_eq!(page, 2);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let req: Request = read_msg(&mut reader, &mut String::new()).unwrap().unwrap();
        match req {
            Request::SetArt { slot, url, .. } => {
                assert_eq!(slot, ArtSlot::Icon);
                assert!(url.is_empty()); // an empty url means "automatic again"
            }
            other => panic!("wrong variant: {other:?}"),
        }
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

    #[test]
    fn quit_round_trips() {
        let mut out = Vec::new();
        write_msg(&mut out, &Request::Quit).unwrap();
        assert_eq!(String::from_utf8(out.clone()).unwrap(), "{\"type\":\"quit\"}\n");

        let back: Request = read_msg(&mut &out[..], &mut String::new()).unwrap().unwrap();
        assert!(matches!(back, Request::Quit));
    }

    #[test]
    fn quit_deserialises_from_its_json() {
        let back: Request = serde_json::from_str(r#"{"type":"quit"}"#).unwrap();
        assert!(matches!(back, Request::Quit));
        let text = serde_json::to_string(&Request::Quit).unwrap();
        assert_eq!(text, r#"{"type":"quit"}"#);
    }
}
