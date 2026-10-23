//! Commands invoked from the UI.
//!
//! Video frames reach the UI through a binary channel. Each message is a
//! 24-byte little-endian header followed by the H.264 Annex B access unit:
//! `u8 kind (1 = video) | u8 keyframe | u16 reserved | u32 monitor_id |
//! u64 frame_id | u64 capture_ts_us`.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use rc_proto::{mouse_event, quality_settings::Preset, KeyEvent, MouseEvent};
use rc_rendezvous::{parse_id, ServerInfo};
use rc_session::files::{TransferEvent, TransferState};
use rc_session::viewer::{self, Target, ViewerConfig, ViewerEvent};
use rc_session::{AuthMethod, PeerInfo};
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::State;

use crate::state::AppState;

pub(crate) type CmdResult<T> = Result<T, String>;

pub(crate) fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    name: String,
    fingerprint: String,
}

/// Device ID from a `remoteconn://` link that arrived before the UI listened.
#[tauri::command]
pub fn take_pending_link(state: State<'_, AppState>) -> Option<String> {
    state.pending_link.lock().unwrap().take()
}

#[tauri::command]
pub fn device_info(state: State<'_, AppState>) -> DeviceInfo {
    DeviceInfo {
        name: state.device_name(),
        fingerprint: state.identity.fingerprint(),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Resolves `host[:port]` (default port 21116).
pub(crate) async fn resolve_server(server: &str) -> CmdResult<SocketAddr> {
    let with_port = if server
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        server.to_string()
    } else {
        format!("{server}:{}", rc_rendezvous::DEFAULT_PORT)
    };
    let addr = tokio::net::lookup_host(with_port.as_str())
        .await
        .map_err(|e| format!("Nie można odnaleźć serwera „{server}”: {e}"))?
        .find(SocketAddr::is_ipv4);
    addr.ok_or_else(|| format!("Serwer „{server}” nie ma adresu IPv4."))
}

/// The configured rendezvous server with its access key and pinned
/// certificate, plus the address string it is stored under.
pub(crate) async fn configured_server(state: &AppState) -> CmdResult<Option<(String, ServerInfo)>> {
    let settings = state.settings();
    let server = settings.rendezvous_server;
    if server.is_empty() {
        return Ok(None);
    }
    let info = ServerInfo {
        addr: resolve_server(&server).await?,
        access_key: settings.server_access_key,
        pin: state.server_pin(&server),
    };
    Ok(Some((server, info)))
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerDto {
    session_id: u64,
    device_name: String,
    fingerprint: String,
    addr: String,
    relayed: bool,
    /// Host side: "oneTime" | "permanent" | "account".
    auth: &'static str,
    /// Host side, with account access: the account.
    account: Option<String>,
}

impl From<&PeerInfo> for PeerDto {
    fn from(p: &PeerInfo) -> Self {
        Self {
            session_id: p.session_id,
            device_name: p.device_name.clone(),
            fingerprint: p.fingerprint.clone(),
            addr: p.addr.to_string(),
            relayed: p.relayed,
            auth: match p.auth {
                AuthMethod::OneTimePassword => "oneTime",
                AuthMethod::PermanentPassword => "permanent",
                AuthMethod::Account => "account",
            },
            account: p.account.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MonitorDto {
    id: u32,
    name: String,
    width: u32,
    height: u32,
    primary: bool,
}

#[derive(Serialize, Clone)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "type"
)]
pub enum SessionEventDto {
    VideoConfig {
        codec: String,
        monitor_id: u32,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    },
    CursorShape {
        /// PNG data URL, usable as a CSS cursor or an image.
        url: String,
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
    },
    CursorPosition {
        monitor_id: u32,
        x: i32,
        y: i32,
        visible: bool,
    },
    Monitors {
        monitors: Vec<MonitorDto>,
    },
    Permissions {
        view_screen: bool,
        control_input: bool,
        clipboard: bool,
        file_transfer: bool,
    },
    Chat {
        text: String,
        sent_unix_ms: i64,
    },
    Transfer {
        transfer: TransferDto,
    },
    Stats {
        rtt_ms: u32,
        loss_permille: u32,
        received_kbps: u32,
        fps: u32,
    },
    Closed {
        reason: String,
    },
}

fn to_dto(ev: ViewerEvent) -> Option<SessionEventDto> {
    Some(match ev {
        ViewerEvent::VideoConfig(c) => SessionEventDto::VideoConfig {
            codec: c.codec,
            monitor_id: c.monitor_id,
            width: c.width,
            height: c.height,
            fps: c.fps,
            bitrate_kbps: c.bitrate_kbps,
        },
        ViewerEvent::CursorShape(s) => {
            let png = rc_clipboard::encode_png(s.width, s.height, &s.rgba).ok()?;
            SessionEventDto::CursorShape {
                url: format!("data:image/png;base64,{}", BASE64.encode(png)),
                width: s.width,
                height: s.height,
                hot_x: s.hotspot_x,
                hot_y: s.hotspot_y,
            }
        }
        ViewerEvent::CursorPosition(p) => SessionEventDto::CursorPosition {
            monitor_id: p.monitor_id,
            x: p.x,
            y: p.y,
            visible: p.visible,
        },
        ViewerEvent::Monitors(list) => SessionEventDto::Monitors {
            monitors: list
                .into_iter()
                .map(|m| MonitorDto {
                    id: m.id,
                    name: m.name,
                    width: m.width,
                    height: m.height,
                    primary: m.primary,
                })
                .collect(),
        },
        ViewerEvent::Permissions(p) => SessionEventDto::Permissions {
            view_screen: p.view_screen,
            control_input: p.control_input,
            clipboard: p.clipboard,
            file_transfer: p.file_transfer,
        },
        ViewerEvent::Chat(c) => SessionEventDto::Chat {
            text: c.text,
            sent_unix_ms: c.sent_unix_ms,
        },
        ViewerEvent::Stats(s) => SessionEventDto::Stats {
            rtt_ms: s.rtt_ms,
            loss_permille: s.loss_permille,
            received_kbps: s.received_kbps,
            fps: s.decoded_fps,
        },
        ViewerEvent::Closed(reason) => SessionEventDto::Closed { reason },
        ViewerEvent::Transfer(t) => SessionEventDto::Transfer {
            transfer: (&t).into(),
        },
        ViewerEvent::Video(_) => return None,
    })
}

fn frame_message(frame: viewer::VideoFrame) -> InvokeResponseBody {
    let mut buf = Vec::with_capacity(24 + frame.data.len());
    buf.push(1);
    buf.push(frame.keyframe as u8);
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&frame.monitor_id.to_le_bytes());
    buf.extend_from_slice(&frame.frame_id.to_le_bytes());
    buf.extend_from_slice(&frame.capture_ts_us.to_le_bytes());
    buf.extend_from_slice(&frame.data);
    InvokeResponseBody::Raw(buf)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedDto {
    peer: PeerDto,
}

#[tauri::command]
pub async fn viewer_connect(
    state: State<'_, AppState>,
    address: String,
    password: String,
    frames: Channel<InvokeResponseBody>,
    events: Channel<SessionEventDto>,
) -> CmdResult<ConnectedDto> {
    let mut server_name = None;
    let target = match parse_id(&address) {
        Some(device_id) => {
            let (name, server) = configured_server(&state)
                .await?
                .ok_or("Aby łączyć się po ID, ustaw serwer ID w ustawieniach.")?;
            server_name = Some(name);
            Target::Id { device_id, server }
        }
        None => Target::Address(parse_address(&address)?),
    };
    let own_id = state.device_id();
    let settings = state.settings();
    // Replace any previous session.
    state.viewer.write().await.take();

    let sink: viewer::EventSink = Arc::new(move |ev| match ev {
        ViewerEvent::Video(frame) => {
            let _ = frames.send(frame_message(frame));
        }
        other => {
            if let Some(dto) = to_dto(other) {
                let _ = events.send(dto);
            }
        }
    });
    let session = viewer::connect(
        ViewerConfig {
            identity: state.identity.clone(),
            device_name: state.device_name(),
            device_id: own_id,
            target,
            password,
            account_cert: state.account().cert_bytes(),
            force_relay: settings.force_relay,
            download_dir: settings.download_dir(),
        },
        sink,
    )
    .await
    .map_err(|e| connect_error_message(&e))?;
    if let (Some(name), Some(fingerprint)) = (server_name, session.server_fingerprint) {
        state.pin_server_if_new(&name, &fingerprint);
    }
    let peer = PeerDto::from(&session.peer);
    *state.viewer.write().await = Some(session);
    Ok(ConnectedDto { peer })
}

fn parse_address(input: &str) -> CmdResult<SocketAddr> {
    let input = input.trim();
    if let Ok(addr) = input.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = input.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, rc_transport::DEFAULT_PORT));
    }
    Err(format!(
        "Nieprawidłowy cel „{input}”. Wpisz 9-cyfrowe ID albo adres IP (opcjonalnie z portem)."
    ))
}

pub(crate) const PIN_MISMATCH_MESSAGE: &str =
    "Klucz serwera ID jest inny niż przy pierwszym połączeniu. \
     To może oznaczać podszywanie się pod serwer. Jeśli serwer został celowo przeinstalowany, \
     użyj „Zapomnij klucz serwera” w Ustawienia → Sieć.";
pub(crate) const ACCESS_DENIED_MESSAGE: &str =
    "Serwer ID odrzucił klucz dostępu. Sprawdź go w Ustawienia → Sieć.";

/// User-facing (Polish) description of a failed connection attempt.
fn connect_error_message(e: &viewer::ConnectError) -> String {
    use rc_rendezvous::RendezvousError as R;
    use viewer::ConnectError as C;
    match e {
        C::WrongPassword => "Nieprawidłowe hasło.".into(),
        C::Rejected => "Użytkownik zdalny odrzucił połączenie.".into(),
        C::TooManyAttempts { retry_after_secs } => {
            format!("Zbyt wiele nieudanych prób. Spróbuj ponownie za {retry_after_secs} s.")
        }
        C::UnsupportedVersion => "Zdalne urządzenie ma niezgodną wersję aplikacji.".into(),
        C::HostAuthFailed | C::HostKeyMismatch => {
            "Zdalne urządzenie nie przeszło weryfikacji tożsamości — połączenie przerwane.".into()
        }
        C::AccountNotTrusted => {
            "Ten komputer nie wpuszcza Twojego konta bez hasła. Wpisz hasło widoczne na komputerze zdalnym."
                .into()
        }
        C::Rendezvous(R::NotFound) => "Nie ma urządzenia o takim ID.".into(),
        C::Rendezvous(R::Offline) => "Urządzenie jest offline.".into(),
        C::Rendezvous(R::RateLimited) => "Zbyt wiele zapytań. Spróbuj ponownie za minutę.".into(),
        C::Rendezvous(R::PinMismatch) => PIN_MISMATCH_MESSAGE.into(),
        C::Rendezvous(R::AccessDenied(_)) => ACCESS_DENIED_MESSAGE.into(),
        C::Rendezvous(R::Transport(t)) => format!("Serwer ID jest nieosiągalny ({t})."),
        C::Timeout => "Przekroczono czas oczekiwania.".into(),
        other => format!("Nie udało się połączyć: {other}"),
    }
}

#[tauri::command]
pub async fn viewer_disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(session) = state.viewer.write().await.take() {
        session.disconnect().await;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseDto {
    kind: String,
    #[serde(default)]
    button: u8,
    #[serde(default)]
    monitor_id: u32,
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
    #[serde(default)]
    wheel_dx: i32,
    #[serde(default)]
    wheel_dy: i32,
}

#[tauri::command]
pub async fn viewer_mouse(state: State<'_, AppState>, event: MouseDto) -> CmdResult<()> {
    let kind = match event.kind.as_str() {
        "move" => mouse_event::Kind::Move,
        "down" => mouse_event::Kind::Down,
        "up" => mouse_event::Kind::Up,
        "wheel" => mouse_event::Kind::Wheel,
        _ => return Err("unknown mouse event".into()),
    };
    // DOM MouseEvent.button: 0 left, 1 middle, 2 right, 3 back, 4 forward.
    let button = match event.button {
        0 => mouse_event::Button::Left,
        1 => mouse_event::Button::Middle,
        2 => mouse_event::Button::Right,
        3 => mouse_event::Button::Back,
        4 => mouse_event::Button::Forward,
        _ => mouse_event::Button::None,
    };
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session
        .mouse(MouseEvent {
            kind: kind as i32,
            button: button as i32,
            monitor_id: event.monitor_id,
            x: event.x,
            y: event.y,
            wheel_dx: event.wheel_dx,
            wheel_dy: event.wheel_dy,
        })
        .await
        .map_err(err)
}

#[tauri::command]
pub async fn viewer_key(state: State<'_, AppState>, hid: u32, down: bool) -> CmdResult<()> {
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session
        .key(KeyEvent {
            hid_usage: hid,
            down,
            ..Default::default()
        })
        .await
        .map_err(err)
}

/// Ctrl+Alt+Del on the remote computer (needs its service for the real one).
#[tauri::command]
pub async fn viewer_special_key(state: State<'_, AppState>, kind: String) -> CmdResult<()> {
    let kind = match kind.as_str() {
        "ctrlAltDel" => rc_proto::special_key::Kind::CtrlAltDel,
        _ => return Err("unknown key".into()),
    };
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session.special_key(kind).await.map_err(err)
}

#[tauri::command]
pub async fn viewer_select_monitor(state: State<'_, AppState>, id: u32) -> CmdResult<()> {
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session.select_monitor(id).await.map_err(err)
}

#[tauri::command]
pub async fn viewer_request_keyframe(state: State<'_, AppState>) -> CmdResult<()> {
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session.request_keyframe().await.map_err(err)
}

#[tauri::command]
pub async fn viewer_send_chat(state: State<'_, AppState>, text: String) -> CmdResult<()> {
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session.send_chat(text).await.map_err(err)
}

// ---------------------------------------------------------------------------
// File transfer
// ---------------------------------------------------------------------------

/// Transfer ids are random u64; sent as strings because JS numbers lose
/// precision above 2^53.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TransferDto {
    id: String,
    name: String,
    incoming: bool,
    transferred: u64,
    total: u64,
    state: &'static str,
    path: Option<String>,
    error: Option<String>,
}

impl From<&TransferEvent> for TransferDto {
    fn from(ev: &TransferEvent) -> Self {
        let (state, path, error) = match &ev.state {
            TransferState::Preparing => ("preparing", None, None),
            TransferState::Running => ("running", None, None),
            TransferState::Done { path } => {
                ("done", path.as_ref().map(|p| p.display().to_string()), None)
            }
            TransferState::Failed(e) => ("failed", None, Some(e.clone())),
            TransferState::Cancelled => ("cancelled", None, None),
        };
        Self {
            id: ev.id.to_string(),
            name: ev.name.clone(),
            incoming: ev.incoming,
            transferred: ev.transferred,
            total: ev.total,
            state,
            path,
            error,
        }
    }
}

fn parse_transfer_id(id: &str) -> CmdResult<u64> {
    id.parse().map_err(|_| "invalid transfer id".to_string())
}

pub(crate) fn to_paths(paths: Vec<String>) -> Vec<PathBuf> {
    paths.into_iter().map(PathBuf::from).collect()
}

#[tauri::command]
pub async fn viewer_send_files(state: State<'_, AppState>, paths: Vec<String>) -> CmdResult<()> {
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session.send_files(to_paths(paths));
    Ok(())
}

#[tauri::command]
pub async fn viewer_cancel_transfer(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let id = parse_transfer_id(&id)?;
    if let Some(session) = state.viewer.read().await.as_ref() {
        session.cancel_transfer(id);
    }
    Ok(())
}

/// Shows a received file in Explorer / Finder.
#[tauri::command]
pub fn reveal_path(path: String) -> CmdResult<()> {
    tauri_plugin_opener::reveal_item_in_dir(path).map_err(err)
}

#[tauri::command]
pub async fn viewer_set_quality(
    state: State<'_, AppState>,
    preset: String,
    max_fps: u32,
) -> CmdResult<()> {
    let preset = match preset.as_str() {
        "quality" => Preset::Quality,
        "speed" => Preset::Speed,
        _ => Preset::Balanced,
    };
    let viewer = state.viewer.read().await;
    let session = viewer.as_ref().ok_or("not connected")?;
    session.set_quality(preset, max_fps).await.map_err(err)
}
