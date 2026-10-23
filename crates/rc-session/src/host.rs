//! Host side: accepts viewers, authenticates them, streams the screen and
//! injects their input.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rc_capture::{Capturer, CursorUpdate, MonitorInfo};
use rc_codec::{EncodedFrame, H264Encoder, VideoEncoder};
use rc_crypto::{identity::fingerprint, DeviceIdentity, PakeRole, PasswordAuth};
use rc_input::{Injector, Rect};
use rc_proto::text::{clean_label, clean_text};
use rc_proto::{
    auth_result::Status, datagram, envelope::Msg, quality_settings, special_key, CursorPosition,
    CursorShape, Datagram, Disconnect, HelloAck, Monitor, MonitorList, NetworkStats, PakeConfirm,
    PakeMessage, Permissions, QualitySettings, VideoConfig, VideoPacket, PROTOCOL_VERSION,
};
use rc_rendezvous::{HostEvent, HostRegistration, RelayRole, RendezvousErrorKind, ServerInfo};
use rc_transport::{Endpoint, Path, SecureConnection, Sender, StreamReceiver};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::files::{OutgoingFile, TransferEvent, Transfers};
use crate::limiter::{Admission, RateLimiter, GLOBAL_DELAY};
use crate::rate::{preset_cap_kbps, RateController};
use crate::{
    auth_msg, current_platform, unix_now, AccountTrust, AuthMethod, EncoderSettings, PeerInfo,
    SessionError, APPROVE_TIMEOUT, AUTH_TIMEOUT, CLIENT_VERSION,
};

/// Headroom for the protobuf framing around each video fragment.
const FRAGMENT_OVERHEAD: usize = 64;
const MIN_KEYFRAME_GAP: Duration = Duration::from_millis(300);
const MIN_MONITOR_SWITCH_GAP: Duration = Duration::from_millis(500);

pub struct HostConfig {
    pub identity: Arc<DeviceIdentity>,
    pub device_name: String,
    /// ID from a previous registration (empty for a new device).
    pub device_id: String,
    /// Current one-time password; may be rotated while the host runs.
    pub password: Arc<RwLock<String>>,
    /// Optional permanent password, accepted alongside the one-time one.
    pub permanent_password: Arc<RwLock<Option<String>>>,
    pub bind: SocketAddr,
    pub encoder: EncoderSettings,
    /// Rendezvous server to register with; `None` = reachable by IP only.
    pub rendezvous: Option<ServerInfo>,
    /// Where files sent by viewers are saved; asked at the start of each
    /// session (the service agent follows the logged-in user).
    pub download_dir: Arc<dyn Fn() -> PathBuf + Send + Sync>,
    /// Account whose devices get in without a password (may change while
    /// running: login, logout, updates from the server).
    pub account: Arc<RwLock<Option<AccountTrust>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RendezvousStatus {
    Online {
        device_id: String,
        /// Server certificate fingerprint (persist it to pin the server).
        server_fingerprint: [u8; 32],
    },
    Offline {
        kind: RendezvousErrorKind,
        reason: String,
    },
}

/// Callbacks into the UI.
pub trait HostHooks: Send + Sync + 'static {
    /// Asks the local user whether to accept `peer`; resolve with `None` to
    /// reject. Unanswered requests are rejected after [`APPROVE_TIMEOUT`].
    fn approve(&self, peer: &PeerInfo) -> oneshot::Receiver<Option<Permissions>>;
    fn session_started(&self, _peer: &PeerInfo) {}
    fn session_ended(&self, _peer: &PeerInfo, _reason: &str) {}
    fn rendezvous(&self, _status: RendezvousStatus) {}
    /// Chat message from the viewer of session `peer.session_id`.
    fn chat(&self, _peer: &PeerInfo, _text: &str) {}
    /// Progress of a file transfer in either direction.
    fn transfer(&self, _peer: &PeerInfo, _event: &TransferEvent) {}
    /// The trusted account changed (server update): persist it.
    fn account_updated(&self, _trust: &AccountTrust) {}
    /// The viewer asked for Ctrl+Alt+Del or another special key.
    fn special_key(&self, _peer: &PeerInfo, _kind: special_key::Kind) {}
}

/// Connections that have not proven a password yet, at most. Bounds what an
/// unauthenticated client can make the host hold.
const MAX_UNAUTHENTICATED: usize = 16;

struct Shared {
    cfg: HostConfig,
    hooks: Arc<dyn HostHooks>,
    limiter: Mutex<RateLimiter>,
    /// ID assigned by the rendezvous server, announced to viewers.
    device_id: RwLock<String>,
    /// Running sessions, for commands from the host UI.
    sessions: Mutex<HashMap<u64, tokio_mpsc::UnboundedSender<SessionCmd>>>,
    next_session: AtomicU64,
    unauthenticated: AtomicUsize,
}

/// Holds one of the [`MAX_UNAUTHENTICATED`] slots until dropped.
struct UnauthenticatedSlot(Arc<Shared>);

impl UnauthenticatedSlot {
    fn acquire(shared: &Arc<Shared>) -> Option<Self> {
        let previous = shared.unauthenticated.fetch_add(1, Ordering::Relaxed);
        let slot = Self(shared.clone());
        (previous < MAX_UNAUTHENTICATED).then_some(slot)
    }
}

impl Drop for UnauthenticatedSlot {
    fn drop(&mut self) {
        self.0.unauthenticated.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A password attempt registered with the limiter; dropping it without a
/// verdict (connection lost mid-way) releases the per-IP slot.
struct Attempt<'a> {
    shared: &'a Shared,
    ip: IpAddr,
    done: bool,
}

impl Attempt<'_> {
    fn finish(mut self, success: bool) {
        self.done = true;
        self.shared
            .limiter
            .lock()
            .unwrap()
            .finish(self.ip, success, Instant::now());
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.shared.limiter.lock().unwrap().abandon(self.ip);
        }
    }
}

pub struct HostHandle {
    endpoint: Endpoint,
    shared: Arc<Shared>,
    registration: Option<JoinHandle<()>>,
    pub local_addr: SocketAddr,
}

impl HostHandle {
    /// Sends a command to a running session; false if it has ended.
    pub fn command(&self, session_id: u64, cmd: SessionCmd) -> bool {
        self.shared
            .sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .is_some_and(|tx| tx.send(cmd).is_ok())
    }

    /// Stops accepting, unregisters and closes all sessions.
    pub fn stop(&self) {
        if let Some(task) = &self.registration {
            task.abort();
        }
        self.endpoint.close(0u32.into(), b"host stopped");
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts listening (and registering, if configured); must be called inside
/// a Tokio runtime.
pub async fn start(cfg: HostConfig, hooks: Arc<dyn HostHooks>) -> Result<HostHandle, SessionError> {
    let endpoint = rc_transport::server_endpoint(cfg.bind)?;
    let local_addr = endpoint
        .local_addr()
        .map_err(rc_transport::TransportError::from)?;
    let shared = Arc::new(Shared {
        device_id: RwLock::new(cfg.device_id.clone()),
        cfg,
        hooks,
        limiter: Mutex::new(RateLimiter::default()),
        sessions: Mutex::new(HashMap::new()),
        next_session: AtomicU64::new(1),
        unauthenticated: AtomicUsize::new(0),
    });

    let ep = endpoint.clone();
    let accept_shared = shared.clone();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let Some(slot) = UnauthenticatedSlot::acquire(&accept_shared) else {
                incoming.refuse();
                continue;
            };
            let shared = accept_shared.clone();
            tokio::spawn(async move {
                let remote = incoming.remote_address();
                let result = async {
                    let sc = rc_transport::accept(incoming, &shared.cfg.identity).await?;
                    run_session(shared, sc, remote, slot).await
                };
                if let Err(e) = result.await {
                    tracing::debug!(%remote, "session failed: {e}");
                }
            });
        }
    });

    let registration = shared.cfg.rendezvous.clone().map(|server| {
        let (tx, rx) = tokio_mpsc::unbounded_channel();
        tokio::spawn(rendezvous_events(endpoint.clone(), shared.clone(), rx));
        rc_rendezvous::spawn_host(
            endpoint.clone(),
            HostRegistration {
                server,
                identity: shared.cfg.identity.clone(),
                device_id: shared.cfg.device_id.clone(),
                device_name: shared.cfg.device_name.clone(),
                account_name: shared
                    .cfg
                    .account
                    .read()
                    .unwrap()
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
            },
            tx,
        )
    });

    tracing::info!(%local_addr, "host listening");
    Ok(HostHandle {
        endpoint,
        shared,
        registration,
        local_addr,
    })
}

async fn rendezvous_events(
    endpoint: Endpoint,
    shared: Arc<Shared>,
    mut events: tokio_mpsc::UnboundedReceiver<HostEvent>,
) {
    while let Some(event) = events.recv().await {
        match event {
            HostEvent::Registered {
                device_id,
                server_fingerprint,
                ..
            } => {
                *shared.device_id.write().unwrap() = device_id.clone();
                shared.hooks.rendezvous(RendezvousStatus::Online {
                    device_id,
                    server_fingerprint,
                });
            }
            HostEvent::AccountUpdate(update) => {
                let updated = {
                    let mut account = shared.cfg.account.write().unwrap();
                    match account.as_mut() {
                        Some(trust) => trust.apply(&update).then(|| trust.clone()),
                        None => None,
                    }
                };
                if let Some(trust) = updated {
                    shared.hooks.account_updated(&trust);
                }
            }
            HostEvent::Disconnected(kind, reason) => {
                shared
                    .hooks
                    .rendezvous(RendezvousStatus::Offline { kind, reason });
            }
            HostEvent::RelayOffer {
                relay,
                token,
                viewer_addr,
                relay_config,
            } => {
                let Some(slot) = UnauthenticatedSlot::acquire(&shared) else {
                    continue;
                };
                let endpoint = endpoint.clone();
                let shared = shared.clone();
                tokio::spawn(async move {
                    let result = async {
                        let access_key = shared
                            .cfg
                            .rendezvous
                            .as_ref()
                            .map(|s| s.access_key.clone())
                            .unwrap_or_default();
                        let (conn, send, recv) = rc_transport::relay::bind(
                            &endpoint,
                            relay,
                            &token,
                            RelayRole::Host,
                            &access_key,
                            relay_config,
                        )
                        .await?;
                        let identity = &shared.cfg.identity;
                        let sc =
                            rc_transport::responder_over(conn, send, recv, identity, Path::Relayed)
                                .await?;
                        run_session(shared, sc, viewer_addr, slot).await
                    };
                    if let Err(e) = result.await {
                        tracing::warn!(%viewer_addr, "relayed session failed: {e}");
                    }
                });
            }
        }
    }
}

async fn expect(stream: &mut StreamReceiver, timeout: Duration) -> Result<Msg, SessionError> {
    Ok(tokio::time::timeout(timeout, stream.recv()).await??)
}

/// What the host tells a viewer about itself; only after authentication, so
/// strangers cannot learn the name, ID or whether unattended access is on.
fn hello_ack(shared: &Shared, authenticated: bool) -> HelloAck {
    let mut ack = HelloAck {
        protocol_version: PROTOCOL_VERSION,
        client_version: CLIENT_VERSION.into(),
        platform: current_platform() as i32,
        ..Default::default()
    };
    if authenticated {
        ack.device_name = shared.cfg.device_name.clone();
        ack.device_id = shared.device_id.read().unwrap().clone();
        ack.unattended = shared
            .cfg
            .permanent_password
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|p| !p.is_empty());
    }
    ack
}

/// Runs one session; `peer_addr` is the viewer's real address (not the
/// relay's), used for display and password-attempt limiting.
async fn run_session(
    shared: Arc<Shared>,
    sc: SecureConnection,
    peer_addr: SocketAddr,
    unauthenticated: UnauthenticatedSlot,
) -> Result<(), SessionError> {
    let ip = peer_addr.ip();
    let SecureConnection {
        path,
        remote_static,
        handshake_hash,
        sender,
        mut stream,
        bulk,
        ..
    } = sc;

    let Msg::Hello(hello) = expect(&mut stream, AUTH_TIMEOUT).await? else {
        return Err(SessionError::Protocol("expected Hello"));
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        refuse(&sender, Status::UnsupportedVersion, 0).await?;
        return Err(SessionError::Protocol("unsupported protocol version"));
    }
    // A device of the trusted account gets in without the password.
    let account = shared
        .cfg
        .account
        .read()
        .unwrap()
        .as_ref()
        .filter(|trust| trust.accepts(&hello.account_cert, &remote_static, unix_now()))
        .map(|trust| trust.name.clone());
    let mut first_ack = hello_ack(&shared, false);
    first_ack.account_accepted = account.is_some();
    sender.send(Msg::HelloAck(first_ack)).await?;
    // Everything in Hello comes from the other side: clean before showing it.
    let mut peer = PeerInfo {
        auth: AuthMethod::OneTimePassword,
        account: None,
        session_id: shared.next_session.fetch_add(1, Ordering::Relaxed),
        device_name: clean_label(&hello.device_name, 64),
        device_id: clean_label(&hello.device_id, 16),
        platform: hello.platform(),
        client_version: clean_label(&hello.client_version, 32),
        fingerprint: fingerprint(&remote_static),
        public_key: remote_static,
        addr: peer_addr,
        relayed: path == Path::Relayed,
    };

    match account {
        Some(name) => {
            peer.auth = AuthMethod::Account;
            peer.account = Some(name);
        }
        None => match authenticate(&shared, &sender, &mut stream, &handshake_hash, ip).await? {
            Some(method) => peer.auth = method,
            // Refused (wrong password, lockout); the viewer was told.
            None => return Ok(()),
        },
    }
    drop(unauthenticated);

    // Local user decides.
    let decision = tokio::time::timeout(APPROVE_TIMEOUT, shared.hooks.approve(&peer)).await;
    let Ok(Ok(Some(permissions))) = decision else {
        return refuse(&sender, Status::RejectedByUser, 0).await;
    };

    let monitors = rc_capture::monitors().unwrap_or_default();
    sender.send(auth_msg(Status::Ok, 0)).await?;
    sender.send(Msg::HelloAck(hello_ack(&shared, true))).await?;
    sender.send(Msg::Permissions(permissions)).await?;
    sender
        .send(Msg::MonitorList(MonitorList {
            monitors: monitors.iter().map(to_proto_monitor).collect(),
        }))
        .await?;

    tracing::info!(%peer_addr, peer = %peer.device_name, auth = ?peer.auth, "session started");
    let (ui_tx, ui_rx) = tokio_mpsc::unbounded_channel();
    shared
        .sessions
        .lock()
        .unwrap()
        .insert(peer.session_id, ui_tx);
    shared.hooks.session_started(&peer);
    let session = Session {
        shared: &shared,
        peer: &peer,
        sender: sender.clone(),
        permissions,
        monitors,
    };
    let reason = session.serve(stream, bulk, ui_rx).await;
    shared.sessions.lock().unwrap().remove(&peer.session_id);
    tracing::info!(%peer_addr, %reason, "session ended");
    sender.close(&reason);
    shared.hooks.session_ended(&peer, &reason);
    Ok(())
}

/// Password exchange (SPAKE2), with per-IP lockout and one attempt in
/// flight. Returns which password matched, or `None` after telling the
/// viewer it was refused.
async fn authenticate(
    shared: &Shared,
    sender: &Sender,
    stream: &mut StreamReceiver,
    handshake_hash: &[u8; 32],
    ip: IpAddr,
) -> Result<Option<AuthMethod>, SessionError> {
    let Msg::Pake(viewer_pake) = expect(stream, AUTH_TIMEOUT).await? else {
        return Err(SessionError::Protocol("expected Pake"));
    };
    let admission = shared.limiter.lock().unwrap().begin(ip, Instant::now());
    let attempt = match admission {
        Admission::Allowed => Attempt {
            shared,
            ip,
            done: false,
        },
        Admission::Locked(wait) => {
            refuse(sender, Status::TooManyAttempts, wait.as_secs() as u32 + 1).await?;
            return Ok(None);
        }
        Admission::Busy => {
            refuse(sender, Status::TooManyAttempts, 5).await?;
            return Ok(None);
        }
    };
    let password = shared.cfg.password.read().unwrap().clone();
    let permanent = shared
        .cfg
        .permanent_password
        .read()
        .unwrap()
        .clone()
        .filter(|p| !p.is_empty());
    let (one_time, my_pake) = PasswordAuth::start(PakeRole::Host, &password, handshake_hash);
    let (alt, my_pake_alt) = match &permanent {
        Some(p) => {
            let (auth, msg) = PasswordAuth::start(PakeRole::Host, p, handshake_hash);
            (Some(auth), msg)
        }
        None => (None, Vec::new()),
    };
    sender
        .send(Msg::Pake(PakeMessage {
            payload: my_pake,
            payload_alt: my_pake_alt,
        }))
        .await?;
    let Msg::PakeConfirm(viewer_confirm) = expect(stream, AUTH_TIMEOUT).await? else {
        return Err(SessionError::Protocol("expected PakeConfirm"));
    };
    let verified = |auth: PasswordAuth, msg: &[u8], tag: &[u8]| {
        auth.finish(msg)
            .and_then(|key| key.verify_peer(tag).map(|()| key))
            .ok()
    };
    let one_time_key = verified(one_time, &viewer_pake.payload, &viewer_confirm.tag);
    let accepted = match (one_time_key, alt) {
        (Some(key), _) => Some((key, AuthMethod::OneTimePassword)),
        (None, Some(alt)) => verified(alt, &viewer_pake.payload_alt, &viewer_confirm.tag_alt)
            .map(|key| (key, AuthMethod::PermanentPassword)),
        (None, None) => None,
    };
    // During a distributed guessing attack every answer is slowed down.
    let under_attack = shared.limiter.lock().unwrap().under_attack(Instant::now());
    if under_attack {
        tokio::time::sleep(GLOBAL_DELAY).await;
    }
    let Some((key, auth_method)) = accepted else {
        attempt.finish(false);
        tracing::info!(%ip, "wrong password");
        refuse(sender, Status::WrongPassword, 0).await?;
        return Ok(None);
    };
    attempt.finish(true);
    sender
        .send(Msg::PakeConfirm(PakeConfirm {
            tag: key.my_tag().to_vec(),
            tag_alt: Vec::new(),
        }))
        .await?;
    Ok(Some(auth_method))
}

/// Sends a failed AuthResult and waits for the viewer to hang up, so the
/// message is not lost when this side drops the connection.
async fn refuse(
    sender: &Sender,
    status: Status,
    retry_after_secs: u32,
) -> Result<(), SessionError> {
    sender.send(auth_msg(status, retry_after_secs)).await?;
    let _ = tokio::time::timeout(Duration::from_secs(3), sender.closed()).await;
    Ok(())
}

fn to_proto_monitor(m: &MonitorInfo) -> Monitor {
    Monitor {
        id: m.id,
        name: m.name.clone(),
        x: m.x,
        y: m.y,
        width: m.width,
        height: m.height,
        scale_percent: 100,
        primary: m.primary,
    }
}

fn rect(m: &MonitorInfo) -> Rect {
    Rect {
        x: m.x,
        y: m.y,
        width: m.width,
        height: m.height,
    }
}

enum VideoCmd {
    Keyframe,
    SelectMonitor(u32),
    Stats(NetworkStats),
    Quality(QualitySettings),
}

/// Actions the host UI can take on a running session.
#[derive(Debug)]
pub enum SessionCmd {
    Chat(String),
    SendFiles(Vec<OutgoingFile>),
    CancelTransfer(u64),
    Disconnect,
}

struct Session<'a> {
    shared: &'a Shared,
    peer: &'a PeerInfo,
    sender: Sender,
    permissions: Permissions,
    monitors: Vec<MonitorInfo>,
}

impl Session<'_> {
    /// Runs an authenticated session until either side ends it.
    async fn serve(
        self,
        mut stream: StreamReceiver,
        bulk: StreamReceiver,
        mut ui: tokio_mpsc::UnboundedReceiver<SessionCmd>,
    ) -> String {
        let Session {
            shared,
            peer,
            sender,
            permissions,
            monitors,
        } = self;
        let Some(first) = monitors.iter().find(|m| m.primary).or(monitors.first()) else {
            return "no monitor to share".into();
        };
        let mut target = rect(first);
        let clipboard = permissions
            .clipboard
            .then(|| crate::clipboard_sync(sender.clone()));
        let hooks = shared.hooks.clone();
        let transfer_peer = peer.clone();
        let transfers = Transfers::new(
            sender.clone(),
            Arc::new(move |ev| hooks.transfer(&transfer_peer, &ev)),
            permissions.file_transfer,
            (shared.cfg.download_dir)(),
        );
        let transfer_task = tokio::spawn(transfers.clone().run(bulk));

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (done_tx, mut video_done) = oneshot::channel::<String>();
        if permissions.view_screen {
            let sender = sender.clone();
            let settings = shared.cfg.encoder;
            let monitor_id = first.id;
            let rt = tokio::runtime::Handle::current();
            std::thread::Builder::new()
                .name("rc-video".into())
                .spawn(move || {
                    let reason = match video_loop(rt, sender, monitor_id, settings, cmd_rx) {
                        Ok(()) => "video stopped".to_string(),
                        Err(e) => format!("video error: {e}"),
                    };
                    let _ = done_tx.send(reason);
                })
                .expect("spawn video thread");
        }

        let mut injector = if permissions.control_input {
            Injector::new()
                .map_err(|e| tracing::warn!("input disabled: {e}"))
                .ok()
        } else {
            None
        };

        let reason = loop {
            tokio::select! {
                msg = stream.recv() => match msg {
                    Ok(Msg::Mouse(ev)) => {
                        if let Some(inj) = injector.as_mut() {
                            if let Err(e) = inj.mouse(&ev, target) {
                                tracing::debug!("mouse: {e}");
                            }
                        }
                    }
                    Ok(Msg::Key(ev)) => {
                        if let Some(inj) = injector.as_mut() {
                            if let Err(e) = inj.key(&ev) {
                                tracing::debug!("key: {e}");
                            }
                        }
                    }
                    Ok(Msg::SpecialKey(key)) => {
                        if permissions.control_input {
                            shared.hooks.special_key(peer, key.kind());
                        }
                    }
                    Ok(Msg::KeyframeRequest(_)) => {
                        let _ = cmd_tx.send(VideoCmd::Keyframe);
                    }
                    Ok(Msg::Stats(stats)) => {
                        let _ = cmd_tx.send(VideoCmd::Stats(stats));
                    }
                    Ok(Msg::Quality(q)) => {
                        let _ = cmd_tx.send(VideoCmd::Quality(q));
                    }
                    Ok(Msg::SelectMonitor(sel)) => {
                        if let Some(m) = monitors.iter().find(|m| m.id == sel.monitor_id) {
                            target = rect(m);
                            let _ = cmd_tx.send(VideoCmd::SelectMonitor(m.id));
                        }
                    }
                    Ok(Msg::Clipboard(data)) => {
                        if let (Some(sync), Some(content)) = (&clipboard, crate::clip_from_proto(data)) {
                            sync.set(content);
                        }
                    }
                    Ok(Msg::Chat(chat)) => {
                        if permissions.chat {
                            let text = clean_text(&chat.text, crate::MAX_CHAT_CHARS);
                        if !text.trim().is_empty() {
                            shared.hooks.chat(peer, &text);
                        }
                        }
                    }
                    Ok(Msg::Disconnect(d)) => break format!("viewer disconnected: {}", d.reason),
                    Ok(_) => {}
                    Err(e) => break format!("connection lost: {e}"),
                },
                cmd = ui.recv() => match cmd {
                    Some(SessionCmd::Chat(text)) => {
                        let _ = sender.send(crate::chat_msg(text)).await;
                    }
                    Some(SessionCmd::SendFiles(files)) => transfers.send_outgoing(files),
                    Some(SessionCmd::CancelTransfer(id)) => transfers.cancel(id),
                    Some(SessionCmd::Disconnect) | None => {
                        let _ = sender
                            .send(Msg::Disconnect(Disconnect {
                                reason: "closed by the remote user".into(),
                            }))
                            .await;
                        break "disconnected by host user".into();
                    }
                },
                reason = &mut video_done => break reason.unwrap_or_else(|_| "video stopped".into()),
            }
        };
        // Dropping the command channel stops the video thread.
        drop(cmd_tx);
        transfer_task.abort();
        if let Some(inj) = injector.as_mut() {
            inj.release_all();
        }
        reason
    }
}

fn video_loop(
    rt: tokio::runtime::Handle,
    sender: Sender,
    mut monitor_id: u32,
    settings: EncoderSettings,
    cmds: mpsc::Receiver<VideoCmd>,
) -> Result<(), SessionError> {
    let mut capturer = Capturer::new(monitor_id)?;
    let mut encoder = H264Encoder::new(settings);
    let mut rate = RateController::new(
        preset_cap_kbps(quality_settings::Preset::Balanced),
        settings.bitrate_kbps,
    );
    encoder.set_bitrate(rate.current_kbps());
    let mut interval = Duration::from_secs(1) / settings.fps.max(1);
    let started = Instant::now();
    let mut frame_id = 0u64;
    let mut force_keyframe = true;
    let mut announced = None;
    let mut last_forced: Option<Instant> = None;
    let mut last_switch: Option<Instant> = None;

    loop {
        let mut new_bitrate = None;
        loop {
            match cmds.try_recv() {
                // Keyframes are expensive: honour at most a few per second, so a
                // viewer cannot keep the encoder busy with requests.
                Ok(VideoCmd::Keyframe) => {
                    if last_forced.is_none_or(|t| t.elapsed() >= MIN_KEYFRAME_GAP) {
                        force_keyframe = true;
                        last_forced = Some(Instant::now());
                    }
                }
                Ok(VideoCmd::SelectMonitor(id)) => {
                    if id != monitor_id
                        && last_switch.is_none_or(|t| t.elapsed() >= MIN_MONITOR_SWITCH_GAP)
                    {
                        capturer = Capturer::new(id)?;
                        monitor_id = id;
                        force_keyframe = true;
                        last_switch = Some(Instant::now());
                    }
                }
                Ok(VideoCmd::Stats(stats)) => {
                    new_bitrate = rate.on_stats(&stats, Instant::now()).or(new_bitrate);
                }
                Ok(VideoCmd::Quality(q)) => {
                    new_bitrate = rate.set_cap(preset_cap_kbps(q.preset())).or(new_bitrate);
                    if q.max_fps > 0 {
                        let fps = q.max_fps.clamp(5, 60);
                        encoder.set_settings(EncoderSettings {
                            fps,
                            ..encoder.settings()
                        });
                        interval = Duration::from_secs(1) / fps;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if let Some(kbps) = new_bitrate {
            tracing::debug!(kbps, "video bitrate");
            encoder.set_bitrate(kbps);
            announced = None; // re-announce with the new bitrate
        }

        let tick = Instant::now();
        let encoded = match capturer.next_frame(interval)? {
            Some(f) => encoder.encode(f.data, f.width, f.height, force_keyframe)?,
            None if force_keyframe => match capturer.last_frame() {
                Some(f) => encoder.encode(f.data, f.width, f.height, true)?,
                None => None,
            },
            None => None,
        };
        if let Some(cursor) = capturer.take_cursor() {
            send_cursor(&rt, &sender, monitor_id, cursor)?;
        }

        if let Some(frame) = encoded {
            if frame.keyframe {
                force_keyframe = false;
            }
            let config = (monitor_id, frame.width, frame.height);
            if announced != Some(config) {
                let current = encoder.settings();
                rt.block_on(sender.send(Msg::VideoConfig(VideoConfig {
                    codec: encoder.codec().into(),
                    monitor_id,
                    width: frame.width,
                    height: frame.height,
                    fps: current.fps,
                    bitrate_kbps: current.bitrate_kbps,
                })))?;
                announced = Some(config);
            }
            let ts = started.elapsed().as_micros() as u64;
            send_fragments(&sender, monitor_id, frame_id, ts, &frame)?;
            frame_id += 1;
        }

        if let Some(rest) = interval.checked_sub(tick.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// Shape changes go out reliably and in order; position updates are
/// fire-and-forget so a slow stream never stalls capture.
fn send_cursor(
    rt: &tokio::runtime::Handle,
    sender: &Sender,
    monitor_id: u32,
    cursor: CursorUpdate,
) -> Result<(), SessionError> {
    let cursor_id = cursor.shape.as_ref().map_or(0, |s| s.id);
    if let Some(shape) = cursor.shape {
        rt.block_on(sender.send(Msg::CursorShape(CursorShape {
            cursor_id: shape.id,
            width: shape.width,
            height: shape.height,
            hotspot_x: shape.hot_x,
            hotspot_y: shape.hot_y,
            rgba: shape.rgba,
        })))?;
    }
    let sender = sender.clone();
    rt.spawn(async move {
        let _ = sender
            .send(Msg::CursorPosition(CursorPosition {
                monitor_id,
                x: cursor.x,
                y: cursor.y,
                cursor_id,
                visible: cursor.visible,
            }))
            .await;
    });
    Ok(())
}

fn send_fragments(
    sender: &Sender,
    monitor_id: u32,
    frame_id: u64,
    capture_ts_us: u64,
    frame: &EncodedFrame,
) -> Result<(), SessionError> {
    let max = sender
        .max_datagram_payload()
        .ok_or(SessionError::Protocol("peer does not accept datagrams"))?;
    let chunk = max.saturating_sub(FRAGMENT_OVERHEAD).max(256);
    let count = frame.data.len().div_ceil(chunk) as u32;
    for (i, part) in frame.data.chunks(chunk).enumerate() {
        sender.send_datagram(&Datagram {
            msg: Some(datagram::Msg::Video(VideoPacket {
                monitor_id,
                frame_id,
                fragment_index: i as u32,
                fragment_count: count,
                keyframe: frame.keyframe,
                capture_ts_us,
                data: part.to_vec(),
            })),
        })?;
    }
    Ok(())
}
