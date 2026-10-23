//! Viewer side: connects to a host and turns the session into events.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_crypto::{identity::fingerprint, DeviceIdentity, PakeRole, PasswordAuth};
use rc_proto::text::{clean_label, clean_text};
use rc_proto::{
    auth_result::Status, datagram, envelope::Msg, quality_settings::Preset, AuthResult,
    ChatMessage, CursorPosition, CursorShape, Disconnect, Hello, KeyEvent, KeyframeRequest,
    Monitor, MouseEvent, NetworkStats, PakeConfirm, PakeMessage, Permissions, QualitySettings,
    SelectMonitor, VideoConfig, PROTOCOL_VERSION,
};
use rc_rendezvous::{RelayRole, RendezvousError, ServerInfo};
use rc_transport::{DatagramReceiver, Endpoint, Path, SecureConnection, Sender, StreamReceiver};
use tokio::task::JoinHandle;

use crate::files::{TransferEvent, Transfers};
use crate::reassembly::{Output, Reassembler};
use crate::{current_platform, PeerInfo, APPROVE_TIMEOUT, AUTH_TIMEOUT, CLIENT_VERSION};

pub use crate::reassembly::CompleteFrame as VideoFrame;

const KEYFRAME_RETRY: Duration = Duration::from_millis(500);
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// Time for LAN / hole-punched paths before falling back to the relay.
const DIRECT_TIMEOUT: Duration = Duration::from_secs(4);

/// Where to connect.
#[derive(Debug, Clone)]
pub enum Target {
    /// Directly by address (LAN, port forwarding).
    Address(SocketAddr),
    /// By device ID through a rendezvous server (NAT traversal, relay).
    Id {
        device_id: String,
        server: ServerInfo,
    },
}

pub struct ViewerConfig {
    pub identity: Arc<DeviceIdentity>,
    pub device_name: String,
    pub device_id: String,
    pub target: Target,
    /// May be empty when connecting with an account certificate.
    pub password: String,
    /// This device's account certificate (empty: no account).
    pub account_cert: Vec<u8>,
    /// Skip direct paths (testing / restrictive networks).
    pub force_relay: bool,
    /// Where files sent by the host are saved.
    pub download_dir: PathBuf,
}

#[derive(Debug)]
pub enum ViewerEvent {
    Video(VideoFrame),
    VideoConfig(VideoConfig),
    Monitors(Vec<Monitor>),
    Permissions(Permissions),
    Chat(ChatMessage),
    Stats(NetworkStats),
    Transfer(TransferEvent),
    CursorShape(CursorShape),
    CursorPosition(CursorPosition),
    Closed(String),
}

pub type EventSink = Arc<dyn Fn(ViewerEvent) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error(transparent)]
    Transport(#[from] rc_transport::TransportError),
    #[error("wrong password")]
    WrongPassword,
    #[error("the remote user rejected the connection")]
    Rejected,
    #[error("too many failed attempts, retry in {retry_after_secs} s")]
    TooManyAttempts { retry_after_secs: u32 },
    #[error("the remote device runs an incompatible version")]
    UnsupportedVersion,
    #[error("the remote device failed password authentication")]
    HostAuthFailed,
    #[error(transparent)]
    Rendezvous(#[from] RendezvousError),
    #[error("the remote device does not trust your account; enter its password")]
    AccountNotTrusted,
    #[error("the device answered with an unexpected key")]
    HostKeyMismatch,
    #[error("timed out")]
    Timeout,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
}

impl From<tokio::time::error::Elapsed> for ConnectError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        ConnectError::Timeout
    }
}

fn auth_error(result: &AuthResult) -> ConnectError {
    match result.status() {
        Status::WrongPassword => ConnectError::WrongPassword,
        Status::RejectedByUser => ConnectError::Rejected,
        Status::TooManyAttempts => ConnectError::TooManyAttempts {
            retry_after_secs: result.retry_after_secs,
        },
        Status::UnsupportedVersion => ConnectError::UnsupportedVersion,
        Status::Ok | Status::Unspecified => ConnectError::Protocol("unexpected AuthResult"),
    }
}

async fn next(stream: &mut StreamReceiver, timeout: Duration) -> Result<Msg, ConnectError> {
    Ok(tokio::time::timeout(timeout, stream.recv()).await??)
}

pub struct ViewerSession {
    sender: Sender,
    transfers: Arc<Transfers>,
    pub peer: PeerInfo,
    /// Rendezvous server certificate seen while connecting by ID (persist it
    /// to pin the server).
    pub server_fingerprint: Option<[u8; 32]>,
    tasks: Vec<JoinHandle<()>>,
    _endpoint: Endpoint,
}

/// Opens the end-to-end channel to the target; also returns the rendezvous
/// server's certificate fingerprint when one was used.
async fn open_channel(
    endpoint: &Endpoint,
    cfg: &ViewerConfig,
) -> Result<(SecureConnection, Option<[u8; 32]>), ConnectError> {
    let (device_id, server) = match &cfg.target {
        Target::Address(addr) => {
            return Ok((
                rc_transport::connect(endpoint, *addr, &cfg.identity).await?,
                None,
            ))
        }
        Target::Id { device_id, server } => (device_id, server),
    };
    let mut found = rc_rendezvous::lookup(endpoint, server, device_id).await?;
    let expected_key = found.noise_public;
    let server_fingerprint = Some(found.server_fingerprint);
    let verify = move |sc: SecureConnection| {
        if sc.remote_static == expected_key {
            Ok(sc)
        } else {
            Err(ConnectError::HostKeyMismatch)
        }
    };

    if !cfg.force_relay {
        let direct = async {
            let conn = rc_transport::dial(endpoint, &found.candidates, DIRECT_TIMEOUT).await?;
            Ok::<_, ConnectError>(rc_transport::secure_initiator(conn, &cfg.identity).await?)
        };
        match direct.await.and_then(verify) {
            Ok(sc) => return Ok((sc, server_fingerprint)),
            Err(e) => tracing::info!(%device_id, "no direct path ({e}), using relay"),
        }
    }

    let (relay, relay_config) = found.request_relay().await?;
    let (conn, send, recv) = rc_transport::relay::bind(
        endpoint,
        relay,
        found.token(),
        RelayRole::Viewer,
        found.access_key(),
        relay_config,
    )
    .await?;
    let sc = rc_transport::initiator_over(conn, send, recv, &cfg.identity, Path::Relayed).await?;
    Ok((verify(sc)?, server_fingerprint))
}

/// Connects, authenticates and waits for the remote user to accept.
pub async fn connect(cfg: ViewerConfig, sink: EventSink) -> Result<ViewerSession, ConnectError> {
    let first_hop = match &cfg.target {
        Target::Address(addr) => *addr,
        Target::Id { server, .. } => server.addr,
    };
    let endpoint = rc_transport::client_endpoint(rc_transport::any_addr_for(first_hop))?;
    let (
        SecureConnection {
            path,
            remote_addr,
            remote_static,
            handshake_hash,
            sender,
            mut stream,
            bulk,
            datagrams,
        },
        server_fingerprint,
    ) = open_channel(&endpoint, &cfg).await?;

    sender
        .send(Msg::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: CLIENT_VERSION.into(),
            platform: current_platform() as i32,
            device_name: cfg.device_name.clone(),
            device_id: cfg.device_id.clone(),
            video_codecs: vec!["h264".into()],
            account_cert: cfg.account_cert.clone(),
        }))
        .await?;
    // The host only tells who it is after authentication (see below).
    let account_accepted = match next(&mut stream, AUTH_TIMEOUT).await? {
        Msg::HelloAck(ack) => ack.account_accepted,
        Msg::AuthResult(r) => return Err(auth_error(&r)),
        _ => return Err(ConnectError::Protocol("expected HelloAck")),
    };
    if !account_accepted {
        if cfg.password.is_empty() {
            return Err(ConnectError::AccountNotTrusted);
        }
        password_exchange(&sender, &mut stream, &cfg.password, &handshake_hash).await?;
    }

    match next(&mut stream, APPROVE_TIMEOUT + AUTH_TIMEOUT).await? {
        Msg::AuthResult(r) if r.status() == Status::Ok => {}
        Msg::AuthResult(r) => return Err(auth_error(&r)),
        _ => return Err(ConnectError::Protocol("expected AuthResult")),
    }
    let Msg::HelloAck(ack) = next(&mut stream, AUTH_TIMEOUT).await? else {
        return Err(ConnectError::Protocol("expected HelloAck"));
    };

    let peer = PeerInfo {
        session_id: 0,
        auth: if account_accepted {
            crate::AuthMethod::Account
        } else {
            // Only the host knows which password matched.
            crate::AuthMethod::OneTimePassword
        },
        account: None,
        device_name: clean_label(&ack.device_name, 64),
        device_id: clean_label(&ack.device_id, 16),
        platform: ack.platform(),
        client_version: clean_label(&ack.client_version, 32),
        fingerprint: fingerprint(&remote_static),
        public_key: remote_static,
        addr: remote_addr,
        relayed: path == Path::Relayed,
    };
    let transfer_sink = sink.clone();
    let transfers = Transfers::new(
        sender.clone(),
        Arc::new(move |ev| transfer_sink(ViewerEvent::Transfer(ev))),
        // The viewer chose to connect, so it accepts files from the host.
        true,
        cfg.download_dir.clone(),
    );
    let tasks = vec![
        tokio::spawn(control_task(stream, sender.clone(), sink.clone())),
        tokio::spawn(media_task(datagrams, sender.clone(), sink)),
        tokio::spawn(transfers.clone().run(bulk)),
    ];
    Ok(ViewerSession {
        sender,
        transfers,
        peer,
        server_fingerprint,
        tasks,
        _endpoint: endpoint,
    })
}

/// SPAKE2 with the host's one-time and permanent password (see
/// `PakeMessage`).
async fn password_exchange(
    sender: &Sender,
    stream: &mut StreamReceiver,
    password: &str,
    handshake_hash: &[u8; 32],
) -> Result<(), ConnectError> {
    // The same password goes into two exchanges: the host matches them
    // against its one-time and (if set) permanent password.
    let (auth, my_pake) = PasswordAuth::start(PakeRole::Viewer, password, handshake_hash);
    let (auth_alt, my_pake_alt) = PasswordAuth::start(PakeRole::Viewer, password, handshake_hash);
    sender
        .send(Msg::Pake(PakeMessage {
            payload: my_pake,
            payload_alt: my_pake_alt,
        }))
        .await?;
    let (key, key_alt) = match next(stream, AUTH_TIMEOUT).await? {
        Msg::Pake(host_pake) => {
            let key = auth
                .finish(&host_pake.payload)
                .map_err(|_| ConnectError::WrongPassword)?;
            let key_alt = (!host_pake.payload_alt.is_empty())
                .then(|| auth_alt.finish(&host_pake.payload_alt).ok())
                .flatten();
            (key, key_alt)
        }
        Msg::AuthResult(r) => return Err(auth_error(&r)),
        _ => return Err(ConnectError::Protocol("expected Pake")),
    };
    sender
        .send(Msg::PakeConfirm(PakeConfirm {
            tag: key.my_tag().to_vec(),
            tag_alt: key_alt
                .as_ref()
                .map(|k| k.my_tag().to_vec())
                .unwrap_or_default(),
        }))
        .await?;
    match next(stream, AUTH_TIMEOUT).await? {
        Msg::PakeConfirm(c) => {
            let alt_ok = key_alt
                .as_ref()
                .is_some_and(|k| k.verify_peer(&c.tag).is_ok());
            if key.verify_peer(&c.tag).is_err() && !alt_ok {
                return Err(ConnectError::HostAuthFailed);
            }
        }
        Msg::AuthResult(r) => return Err(auth_error(&r)),
        _ => return Err(ConnectError::Protocol("expected PakeConfirm")),
    }
    Ok(())
}

impl ViewerSession {
    pub async fn mouse(&self, ev: MouseEvent) -> Result<(), rc_transport::TransportError> {
        self.sender.send(Msg::Mouse(ev)).await
    }

    pub async fn key(&self, ev: KeyEvent) -> Result<(), rc_transport::TransportError> {
        self.sender.send(Msg::Key(ev)).await
    }

    /// Ctrl+Alt+Del and similar keys the local system would intercept.
    pub async fn special_key(
        &self,
        kind: rc_proto::special_key::Kind,
    ) -> Result<(), rc_transport::TransportError> {
        self.sender
            .send(Msg::SpecialKey(rc_proto::SpecialKey { kind: kind as i32 }))
            .await
    }

    pub async fn select_monitor(
        &self,
        monitor_id: u32,
    ) -> Result<(), rc_transport::TransportError> {
        self.sender
            .send(Msg::SelectMonitor(SelectMonitor { monitor_id }))
            .await
    }

    /// Sends files to the host (it must have granted file transfer).
    pub fn send_files(&self, paths: Vec<PathBuf>) {
        self.transfers.send_files(paths);
    }

    pub fn cancel_transfer(&self, id: u64) {
        self.transfers.cancel(id);
    }

    /// Quality preset (bitrate cap) and frame rate; `max_fps` 0 keeps it.
    pub async fn set_quality(
        &self,
        preset: Preset,
        max_fps: u32,
    ) -> Result<(), rc_transport::TransportError> {
        self.sender
            .send(Msg::Quality(QualitySettings {
                preset: preset as i32,
                max_fps,
            }))
            .await
    }

    pub async fn send_chat(&self, text: String) -> Result<(), rc_transport::TransportError> {
        self.sender.send(crate::chat_msg(text)).await
    }

    pub async fn request_keyframe(&self) -> Result<(), rc_transport::TransportError> {
        self.sender
            .send(Msg::KeyframeRequest(KeyframeRequest { monitor_id: 0 }))
            .await
    }

    pub async fn disconnect(self) {
        let _ = self
            .sender
            .send(Msg::Disconnect(Disconnect {
                reason: "closed by viewer".into(),
            }))
            .await;
        // Dropping closes the connection and stops the tasks.
    }
}

impl Drop for ViewerSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.sender.close("viewer closed");
    }
}

async fn control_task(mut stream: StreamReceiver, sender: Sender, sink: EventSink) {
    // Runs while the host allows clipboard sharing.
    let mut clipboard = None;
    let reason = loop {
        match stream.recv().await {
            Ok(Msg::VideoConfig(c)) => sink(ViewerEvent::VideoConfig(c)),
            Ok(Msg::MonitorList(l)) => sink(ViewerEvent::Monitors(l.monitors)),
            Ok(Msg::Permissions(p)) => {
                if p.clipboard && clipboard.is_none() {
                    clipboard = Some(crate::clipboard_sync(sender.clone()));
                } else if !p.clipboard {
                    clipboard = None;
                }
                sink(ViewerEvent::Permissions(p));
            }
            Ok(Msg::Clipboard(data)) => {
                if let (Some(sync), Some(content)) = (&clipboard, crate::clip_from_proto(data)) {
                    sync.set(content);
                }
            }
            Ok(Msg::Chat(mut c)) => {
                c.text = clean_text(&c.text, crate::MAX_CHAT_CHARS);
                if !c.text.trim().is_empty() {
                    sink(ViewerEvent::Chat(c));
                }
            }
            Ok(Msg::CursorShape(s)) if valid_cursor(&s) => sink(ViewerEvent::CursorShape(s)),
            Ok(Msg::CursorPosition(p)) => sink(ViewerEvent::CursorPosition(p)),
            Ok(Msg::Disconnect(d)) => break d.reason,
            Ok(_) => {}
            Err(e) => break format!("connection lost: {e}"),
        }
    };
    sender.close(&reason);
    sink(ViewerEvent::Closed(reason));
}

async fn media_task(mut datagrams: DatagramReceiver, sender: Sender, sink: EventSink) {
    let mut reassembler = Reassembler::default();
    let mut last_request: Option<Instant> = None;
    let mut ticker = tokio::time::interval(STATS_INTERVAL);
    let (mut bytes, mut frames) = (0usize, 0u32);

    loop {
        let mut need_keyframe = false;
        tokio::select! {
            dg = datagrams.recv() => {
                let Ok(dg) = dg else { return };
                let Some(datagram::Msg::Video(pkt)) = dg.msg else { continue };
                bytes += pkt.data.len();
                match reassembler.push(pkt) {
                    Some(Output::Frame(frame)) => {
                        frames += 1;
                        sink(ViewerEvent::Video(frame));
                    }
                    Some(Output::NeedKeyframe) => need_keyframe = true,
                    None => {}
                }
            }
            _ = ticker.tick() => {
                // Also covers a lost first keyframe on a static screen.
                need_keyframe = reassembler.needs_keyframe();
                let stats = NetworkStats {
                    rtt_ms: sender.rtt().as_millis() as u32,
                    loss_permille: loss_permille(&sender),
                    received_kbps: (bytes * 8 / 1000) as u32,
                    decoded_fps: frames,
                };
                (bytes, frames) = (0, 0);
                let _ = sender.send(Msg::Stats(stats)).await;
                sink(ViewerEvent::Stats(stats));
            }
        }
        if need_keyframe && last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_RETRY) {
            last_request = Some(Instant::now());
            let _ = sender
                .send(Msg::KeyframeRequest(KeyframeRequest { monitor_id: 0 }))
                .await;
        }
    }
}

fn loss_permille(sender: &Sender) -> u32 {
    let path = sender.stats().path;
    if path.sent_packets == 0 {
        return 0;
    }
    (path.lost_packets * 1000 / path.sent_packets) as u32
}

/// Cursor images come from the host: accept only sane sizes whose pixel data
/// matches the declared dimensions.
fn valid_cursor(s: &CursorShape) -> bool {
    (1..=256).contains(&s.width)
        && (1..=256).contains(&s.height)
        && s.rgba.len() == (s.width * s.height * 4) as usize
        && s.hotspot_x < s.width
        && s.hotspot_y < s.height
}
