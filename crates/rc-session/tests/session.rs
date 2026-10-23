//! End-to-end host ⇄ viewer over loopback QUIC.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rc_crypto::DeviceIdentity;
use rc_proto::Permissions;
use rc_session::files::{TransferEvent, TransferState};
use rc_session::host::{self, HostConfig, HostHandle, HostHooks, SessionCmd};
use rc_session::viewer::{self, ConnectError, Target, ViewerConfig, ViewerEvent, ViewerSession};
use rc_session::{AuthMethod, EncoderSettings, PeerInfo};
use tokio::sync::{mpsc, oneshot};

const WAIT: Duration = Duration::from_secs(10);

struct Hooks {
    accept: bool,
    allow_files: bool,
    seen: Mutex<Vec<String>>,
    started: Mutex<Vec<u64>>,
    auth: Mutex<Vec<AuthMethod>>,
    chats: mpsc::UnboundedSender<String>,
    transfers: mpsc::UnboundedSender<TransferEvent>,
}

impl HostHooks for Hooks {
    fn approve(&self, peer: &PeerInfo) -> oneshot::Receiver<Option<Permissions>> {
        self.seen.lock().unwrap().push(peer.device_name.clone());
        self.auth.lock().unwrap().push(peer.auth);
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(self.accept.then_some(Permissions {
            view_screen: true,
            // Never move the real mouse or touch the clipboard during tests.
            control_input: false,
            chat: true,
            file_transfer: self.allow_files,
            ..Default::default()
        }));
        rx
    }

    fn session_started(&self, peer: &PeerInfo) {
        self.started.lock().unwrap().push(peer.session_id);
    }

    fn chat(&self, _peer: &PeerInfo, text: &str) {
        let _ = self.chats.send(text.to_string());
    }

    fn transfer(&self, _peer: &PeerInfo, event: &TransferEvent) {
        let _ = self.transfers.send(event.clone());
    }
}

struct TestHost {
    handle: HostHandle,
    handle_permanent: Arc<RwLock<Option<String>>>,
    hooks: Arc<Hooks>,
    chats: mpsc::UnboundedReceiver<String>,
    transfers: mpsc::UnboundedReceiver<TransferEvent>,
    downloads: PathBuf,
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rc-test-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_host(password: &str, accept: bool, allow_files: bool) -> TestHost {
    let (chats, chats_rx) = mpsc::unbounded_channel();
    let (transfers, transfers_rx) = mpsc::unbounded_channel();
    let hooks = Arc::new(Hooks {
        accept,
        allow_files,
        seen: Mutex::new(Vec::new()),
        started: Mutex::new(Vec::new()),
        auth: Mutex::new(Vec::new()),
        chats,
        transfers,
    });
    let downloads = temp_dir("host-dl");
    let permanent = Arc::new(RwLock::new(None));
    let handle = host::start(
        HostConfig {
            identity: Arc::new(DeviceIdentity::generate()),
            device_name: "test-host".into(),
            device_id: String::new(),
            password: Arc::new(RwLock::new(password.into())),
            permanent_password: permanent.clone(),
            bind: "127.0.0.1:0".parse().unwrap(),
            encoder: EncoderSettings::default(),
            rendezvous: None,
            download_dir: {
                let dir = downloads.clone();
                Arc::new(move || dir.clone())
            },
            account: Arc::new(RwLock::new(None)),
        },
        hooks.clone(),
    )
    .await
    .unwrap();
    TestHost {
        handle,
        handle_permanent: permanent,
        hooks,
        chats: chats_rx,
        transfers: transfers_rx,
        downloads,
    }
}

async fn connect(
    host: &TestHost,
    password: &str,
) -> (
    Result<ViewerSession, ConnectError>,
    mpsc::UnboundedReceiver<ViewerEvent>,
    PathBuf,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let downloads = temp_dir("viewer-dl");
    let result = viewer::connect(
        ViewerConfig {
            identity: Arc::new(DeviceIdentity::generate()),
            device_name: "test-viewer".into(),
            device_id: String::new(),
            target: Target::Address(host.handle.local_addr),
            password: password.into(),
            account_cert: Vec::new(),
            force_relay: false,
            download_dir: downloads.clone(),
        },
        Arc::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )
    .await;
    (result, rx, downloads)
}

/// Waits until `pick` returns `Some` for an event.
async fn wait_for<E, T>(
    events: &mut mpsc::UnboundedReceiver<E>,
    mut pick: impl FnMut(E) -> Option<T>,
) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            let ev = events.recv().await.expect("event channel closed");
            if let Some(found) = pick(ev) {
                return found;
            }
        }
    })
    .await
    .expect("event within timeout")
}

/// Waits for the final state of a transfer.
async fn transfer_result(events: &mut mpsc::UnboundedReceiver<TransferEvent>) -> TransferEvent {
    wait_for(events, |ev| {
        (!matches!(ev.state, TransferState::Preparing | TransferState::Running)).then_some(ev)
    })
    .await
}

fn viewer_transfers(
    events: mpsc::UnboundedReceiver<ViewerEvent>,
) -> mpsc::UnboundedReceiver<TransferEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut events = events;
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            if let ViewerEvent::Transfer(t) = ev {
                let _ = tx.send(t);
            }
        }
    });
    rx
}

fn test_file(dir: &Path, name: &str, len: usize) -> (PathBuf, Vec<u8>) {
    let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
    let path = dir.join(name);
    std::fs::write(&path, &data).unwrap();
    (path, data)
}

#[tokio::test]
async fn correct_password_streams_video() {
    let host = start_host("s3cret", true, false).await;
    let (session, mut events, _) = connect(&host, "s3cret").await;
    let session = session.expect("connects");
    assert_eq!(session.peer.device_name, "test-host");
    assert_eq!(
        *host.hooks.seen.lock().unwrap(),
        vec!["test-viewer".to_string()]
    );

    wait_for(&mut events, |ev| {
        matches!(ev, ViewerEvent::Monitors(_)).then_some(())
    })
    .await;
    if !rc_capture::monitors().unwrap_or_default().is_empty() {
        let frame = wait_for(&mut events, |ev| match ev {
            ViewerEvent::Video(f) => Some(f),
            ViewerEvent::Closed(reason) => panic!("closed: {reason}"),
            _ => None,
        })
        .await;
        assert!(frame.keyframe, "first delivered frame must be a keyframe");
        assert!(!frame.data.is_empty());
    }
    session.disconnect().await;
}

#[tokio::test]
async fn wrong_password_is_rejected_and_locked_out() {
    let host = start_host("s3cret", true, false).await;
    for _ in 0..3 {
        let (result, _, _) = connect(&host, "guess").await;
        assert!(
            matches!(result, Err(ConnectError::WrongPassword)),
            "{:?}",
            result.err()
        );
    }
    let (result, _, _) = connect(&host, "s3cret").await;
    assert!(
        matches!(result, Err(ConnectError::TooManyAttempts { retry_after_secs }) if retry_after_secs > 0),
        "{:?}",
        result.err()
    );
    // The user was never asked.
    assert!(host.hooks.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn user_can_reject() {
    let host = start_host("s3cret", false, false).await;
    let (result, _, _) = connect(&host, "s3cret").await;
    assert!(
        matches!(result, Err(ConnectError::Rejected)),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn chat_both_ways_and_host_disconnect() {
    let mut host = start_host("s3cret", true, false).await;
    let (session, mut events, _) = connect(&host, "s3cret").await;
    let session = session.expect("connects");

    session.send_chat("cześć z viewera".into()).await.unwrap();
    let got = wait_for(&mut host.chats, Some).await;
    assert_eq!(got, "cześć z viewera");

    let id = host.hooks.started.lock().unwrap()[0];
    assert!(host
        .handle
        .command(id, SessionCmd::Chat("hej z hosta".into())));
    let text = wait_for(&mut events, |ev| match ev {
        ViewerEvent::Chat(c) => Some(c.text),
        _ => None,
    })
    .await;
    assert_eq!(text, "hej z hosta");

    assert!(host.handle.command(id, SessionCmd::Disconnect));
    wait_for(&mut events, |ev| {
        matches!(ev, ViewerEvent::Closed(_)).then_some(())
    })
    .await;
    // The ended session unregisters itself.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!host.handle.command(id, SessionCmd::Chat("za późno".into())));
}

#[tokio::test]
async fn file_upload_resumes_and_verifies() {
    let mut host = start_host("s3cret", true, true).await;
    let (session, events, _) = connect(&host, "s3cret").await;
    let session = session.expect("connects");
    let mut viewer_side = viewer_transfers(events);
    let src = temp_dir("src");
    let (path, data) = test_file(&src, "raport końcowy.bin", 3 * 1024 * 1024 + 17);

    // Pretend an earlier attempt already delivered the first megabyte.
    let hash = blake3::hash(&data);
    let hex: String = hash.as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let partial = host.downloads.join(format!(".{hex}-{}.rcpart", data.len()));
    std::fs::write(&partial, &data[..1024 * 1024]).unwrap();

    session.send_files(vec![path]);
    let sent = transfer_result(&mut viewer_side).await;
    assert_eq!(sent.state, TransferState::Done { path: None }, "{sent:?}");
    let received = transfer_result(&mut host.transfers).await;
    let TransferState::Done { path: Some(saved) } = received.state else {
        panic!("{received:?}");
    };
    assert!(received.incoming);
    assert_eq!(saved, host.downloads.join("raport końcowy.bin"));
    assert_eq!(std::fs::read(&saved).unwrap(), data);
    assert!(!partial.exists(), "partial file renamed");
}

#[tokio::test]
async fn file_upload_needs_permission() {
    let host = start_host("s3cret", true, false).await;
    let (session, events, _) = connect(&host, "s3cret").await;
    let session = session.expect("connects");
    let mut viewer_side = viewer_transfers(events);
    let (path, _) = test_file(&temp_dir("src"), "a.txt", 10);
    session.send_files(vec![path]);
    let result = transfer_result(&mut viewer_side).await;
    assert!(
        matches!(result.state, TransferState::Failed(_)),
        "{result:?}"
    );
    assert_eq!(
        std::fs::read_dir(&host.downloads)
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
}

#[tokio::test]
async fn host_sends_file_to_viewer() {
    let host = start_host("s3cret", true, false).await;
    let (session, events, viewer_downloads) = connect(&host, "s3cret").await;
    let _session = session.expect("connects");
    let mut viewer_side = viewer_transfers(events);
    let (path, data) = test_file(&temp_dir("src"), "z hosta.txt", 200_000);

    let id = host.hooks.started.lock().unwrap()[0];
    assert!(host.handle.command(
        id,
        SessionCmd::SendFiles(vec![rc_session::files::OutgoingFile::Path(path)])
    ));
    let received = transfer_result(&mut viewer_side).await;
    let TransferState::Done { path: Some(saved) } = received.state else {
        panic!("{received:?}");
    };
    assert_eq!(saved, viewer_downloads.join("z hosta.txt"));
    assert_eq!(std::fs::read(saved).unwrap(), data);
}

#[tokio::test]
async fn quality_preset_changes_bitrate() {
    if rc_capture::monitors().unwrap_or_default().is_empty() {
        return;
    }
    let host = start_host("s3cret", true, false).await;
    let (session, mut events, _) = connect(&host, "s3cret").await;
    let session = session.expect("connects");
    let first = wait_for(&mut events, |ev| match ev {
        ViewerEvent::VideoConfig(c) => Some(c.bitrate_kbps),
        _ => None,
    })
    .await;
    assert_eq!(first, 8000);

    session
        .set_quality(rc_proto::quality_settings::Preset::Speed, 0)
        .await
        .unwrap();
    // A keyframe makes the host encode (and announce) even on a static screen.
    session.request_keyframe().await.unwrap();
    let next = wait_for(&mut events, |ev| match ev {
        ViewerEvent::VideoConfig(c) if c.bitrate_kbps != 8000 => Some(c.bitrate_kbps),
        _ => None,
    })
    .await;
    assert_eq!(next, 2500);
}

#[tokio::test]
async fn permanent_password_works_alongside_one_time() {
    let host = start_host("jednorazowe", true, false).await;
    // Not set yet: the permanent password is refused.
    let (result, _, _) = connect(&host, "stale-haslo-123").await;
    assert!(
        matches!(result, Err(ConnectError::WrongPassword)),
        "{:?}",
        result.err()
    );

    *host.handle_permanent.write().unwrap() = Some("stale-haslo-123".into());
    let (session, _, _) = connect(&host, "stale-haslo-123").await;
    session.expect("permanent password accepted");
    let (session, _, _) = connect(&host, "jednorazowe").await;
    session.expect("one-time password still accepted");
    let (result, _, _) = connect(&host, "zle").await;
    assert!(
        matches!(result, Err(ConnectError::WrongPassword)),
        "{:?}",
        result.err()
    );

    assert_eq!(
        *host.hooks.auth.lock().unwrap(),
        vec![AuthMethod::PermanentPassword, AuthMethod::OneTimePassword]
    );
}

#[tokio::test]
async fn parallel_guessing_cannot_bypass_lockout() {
    let host = Arc::new(start_host("s3cret-pw", true, false).await);
    let attempts: Vec<_> = (0..10)
        .map(|i| {
            let host = host.clone();
            tokio::spawn(async move { connect(&host, &format!("guess-{i}")).await.0.err() })
        })
        .collect();
    let mut wrong = 0;
    let mut refused = 0;
    for attempt in attempts {
        match attempt.await.unwrap() {
            Some(ConnectError::WrongPassword) => wrong += 1,
            Some(ConnectError::TooManyAttempts { .. }) => refused += 1,
            other => panic!("unexpected: {other:?}"),
        }
    }
    // Only the free attempts may actually test a password.
    assert!(wrong <= 3, "{wrong} passwords were tested");
    assert_eq!(wrong + refused, 10);
}
