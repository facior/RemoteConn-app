//! Connecting by device ID through a real rendezvous/relay server on loopback.
//! The server is the `rc-server` program from the server repository (see
//! `support`); without it these tests are skipped.

mod support;

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rc_crypto::DeviceIdentity;
use rc_proto::Permissions;
use rc_rendezvous::{RendezvousError, ServerInfo};
use rc_session::files::{OutgoingFile, TransferState};
use rc_session::host::{self, HostConfig, HostHandle, HostHooks, RendezvousStatus, SessionCmd};
use rc_session::viewer::{self, ConnectError, Target, ViewerConfig, ViewerEvent, ViewerSession};
use rc_session::{AccountTrust, AuthMethod, EncoderSettings, PeerInfo};
use support::{Options, TestServer};
use tokio::sync::{mpsc, oneshot, watch};

struct Hooks {
    status: watch::Sender<Option<RendezvousStatus>>,
    peers: Mutex<Vec<PeerInfo>>,
}

impl HostHooks for Hooks {
    fn approve(&self, peer: &PeerInfo) -> oneshot::Receiver<Option<Permissions>> {
        self.peers.lock().unwrap().push(peer.clone());
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Some(Permissions {
            view_screen: true,
            control_input: false,
            ..Default::default()
        }));
        rx
    }

    fn rendezvous(&self, status: RendezvousStatus) {
        let _ = self.status.send(Some(status));
    }
}

async fn start_server_with_key(access_key: &str) -> Option<TestServer> {
    TestServer::start(Options {
        access_key: access_key.into(),
        ..Default::default()
    })
    .await
}

async fn start_server() -> Option<TestServer> {
    start_server_with_key("").await
}

fn info(server: &TestServer) -> ServerInfo {
    server.info()
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// Starts a registered host and returns it with its assigned ID.
async fn start_host(server: &TestServer) -> (HostHandle, Arc<Hooks>, String) {
    start_host_with(info(server)).await
}

async fn start_host_with(server: ServerInfo) -> (HostHandle, Arc<Hooks>, String) {
    start_host_full(server, Arc::new(DeviceIdentity::generate()), None).await
}

async fn start_host_full(
    server: ServerInfo,
    identity: Arc<DeviceIdentity>,
    account: Option<AccountTrust>,
) -> (HostHandle, Arc<Hooks>, String) {
    let (status, mut status_rx) = watch::channel(None);
    let hooks = Arc::new(Hooks {
        status,
        peers: Mutex::new(Vec::new()),
    });
    let handle = host::start(
        HostConfig {
            identity,
            device_name: "id-host".into(),
            device_id: String::new(),
            password: Arc::new(RwLock::new("pw123456".into())),
            permanent_password: Arc::new(RwLock::new(None)),
            bind: "127.0.0.1:0".parse().unwrap(),
            encoder: EncoderSettings::default(),
            rendezvous: Some(server),
            download_dir: Arc::new(|| std::env::temp_dir().join("rc-test-relay-host-dl")),
            account: Arc::new(RwLock::new(account)),
        },
        hooks.clone(),
    )
    .await
    .unwrap();
    let id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            status_rx.changed().await.unwrap();
            if let Some(RendezvousStatus::Online { device_id, .. }) = status_rx.borrow().clone() {
                return device_id;
            }
        }
    })
    .await
    .expect("host registers");
    (handle, hooks, id)
}

async fn connect_by_id(
    server: &TestServer,
    device_id: &str,
    force_relay: bool,
) -> (
    Result<ViewerSession, ConnectError>,
    mpsc::UnboundedReceiver<ViewerEvent>,
) {
    connect_with(info(server), device_id, force_relay).await
}

async fn connect_with(
    server: ServerInfo,
    device_id: &str,
    force_relay: bool,
) -> (
    Result<ViewerSession, ConnectError>,
    mpsc::UnboundedReceiver<ViewerEvent>,
) {
    connect_full(
        server,
        device_id,
        force_relay,
        Arc::new(DeviceIdentity::generate()),
        "pw123456",
        Vec::new(),
    )
    .await
}

async fn connect_full(
    server: ServerInfo,
    device_id: &str,
    force_relay: bool,
    identity: Arc<DeviceIdentity>,
    password: &str,
    account_cert: Vec<u8>,
) -> (
    Result<ViewerSession, ConnectError>,
    mpsc::UnboundedReceiver<ViewerEvent>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let result = viewer::connect(
        ViewerConfig {
            identity,
            device_name: "id-viewer".into(),
            device_id: String::new(),
            target: Target::Id {
                device_id: device_id.into(),
                server,
            },
            password: password.into(),
            account_cert,
            force_relay,
            download_dir: std::env::temp_dir()
                .join(format!("rc-test-relay-viewer-{}", rand_suffix())),
        },
        Arc::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )
    .await;
    (result, rx)
}

/// Waits for the first video keyframe (skipped on machines without a display).
async fn expect_keyframe(events: &mut mpsc::UnboundedReceiver<ViewerEvent>) {
    if rc_capture::monitors().unwrap_or_default().is_empty() {
        return;
    }
    let frame = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Some(ViewerEvent::Video(f)) => return f,
                Some(ViewerEvent::Closed(reason)) => panic!("closed: {reason}"),
                Some(_) => {}
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("keyframe within 10 s");
    assert!(frame.keyframe);
}

#[tokio::test]
async fn connects_by_id_directly() {
    let Some(server) = start_server().await else {
        return;
    };
    let (_host, hooks, id) = start_host(&server).await;
    assert_eq!(id.len(), 9);

    let (session, mut events) = connect_by_id(&server, &id, false).await;
    let session = session.expect("connects by ID");
    assert!(!session.peer.relayed, "loopback must not need the relay");
    assert_eq!(session.peer.device_id, id);
    assert!(!hooks.peers.lock().unwrap()[0].relayed);
    expect_keyframe(&mut events).await;
}

#[tokio::test]
async fn connects_through_relay() {
    let Some(server) = start_server().await else {
        return;
    };
    let (_host, hooks, id) = start_host(&server).await;

    let (session, mut events) = connect_by_id(&server, &id, true).await;
    let session = session.expect("connects via relay");
    assert!(session.peer.relayed);
    let host_side = hooks.peers.lock().unwrap()[0].clone();
    assert!(host_side.relayed);
    // The host sees the viewer's own address, not the relay's.
    assert_ne!(host_side.addr, server.addr);
    expect_keyframe(&mut events).await;
}

#[tokio::test]
async fn unknown_and_offline_ids() {
    let Some(server) = start_server().await else {
        return;
    };
    let (result, _) = connect_by_id(&server, "123456789", false).await;
    assert!(
        matches!(
            result,
            Err(ConnectError::Rendezvous(RendezvousError::NotFound))
        ),
        "{:?}",
        result.err()
    );

    let (host, _, id) = start_host(&server).await;
    host.stop();
    drop(host);
    // Give the server a moment to notice.
    let mut offline = false;
    // (Stays under the server's 30 lookups/minute limit.)
    for _ in 0..20 {
        let (result, _) = connect_by_id(&server, &id, false).await;
        if matches!(
            result,
            Err(ConnectError::Rendezvous(RendezvousError::Offline))
        ) {
            offline = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(offline, "stopped host is reported offline");
}

#[tokio::test]
async fn file_transfer_through_relay() {
    let Some(server) = start_server().await else {
        return;
    };
    let (host, hooks, id) = start_host(&server).await;
    let (session, mut events) = connect_by_id(&server, &id, true).await;
    let session = session.expect("connects via relay");
    assert!(session.peer.relayed);

    // Bigger than one relay datagram window and split into many chunks.
    let data: Vec<u8> = (0..1_500_000u32).map(|i| (i % 253) as u8).collect();
    let src = std::env::temp_dir().join(format!("rc-relay-src-{}.bin", rand_suffix()));
    std::fs::write(&src, &data).unwrap();
    let session_id = hooks.peers.lock().unwrap()[0].session_id;
    assert!(host.command(
        session_id,
        SessionCmd::SendFiles(vec![OutgoingFile::Path(src)])
    ));

    let saved = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await {
                Some(ViewerEvent::Transfer(t)) => match t.state {
                    TransferState::Done { path: Some(p) } => return p,
                    TransferState::Failed(e) => panic!("transfer failed: {e}"),
                    _ => {}
                },
                Some(ViewerEvent::Closed(reason)) => panic!("closed: {reason}"),
                Some(_) => {}
                None => panic!("events closed"),
            }
        }
    })
    .await
    .expect("file arrives through the relay");
    assert_eq!(std::fs::read(saved).unwrap(), data);
}

#[tokio::test]
async fn access_key_is_enforced() {
    let Some(server) = start_server_with_key("tajny-klucz").await else {
        return;
    };
    let endpoint = rc_transport::client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let wrong = ServerInfo {
        access_key: "zly-klucz".into(),
        ..info(&server)
    };
    assert!(matches!(
        rc_rendezvous::ping(&endpoint, &wrong).await,
        Err(RendezvousError::AccessDenied(_))
    ));
    let right = ServerInfo {
        access_key: "tajny-klucz".into(),
        ..info(&server)
    };
    assert_eq!(
        rc_rendezvous::ping(&endpoint, &right).await.unwrap(),
        server.fingerprint
    );

    // Registration, lookup and the relay all work with the key.
    let (_host, _, id) = start_host_with(right.clone()).await;
    let (session, _) = connect_with(right, &id, true).await;
    assert!(
        session
            .expect("relayed session with access key")
            .peer
            .relayed
    );
    let (refused, _) = connect_with(wrong, &id, false).await;
    assert!(matches!(
        refused,
        Err(ConnectError::Rendezvous(RendezvousError::AccessDenied(_)))
    ));
}

#[tokio::test]
async fn server_pin_is_checked() {
    let Some(server) = start_server().await else {
        return;
    };
    let (_host, _, id) = start_host(&server).await;

    let (session, _) = connect_by_id(&server, &id, false).await;
    let seen = session.expect("first contact").server_fingerprint;
    assert_eq!(
        seen,
        Some(server.fingerprint),
        "fingerprint reported for pinning"
    );

    let pinned = ServerInfo {
        pin: Some(server.fingerprint),
        ..info(&server)
    };
    let (session, _) = connect_with(pinned.clone(), &id, true).await;
    session.expect("matching pin, relay included");

    let impostor = ServerInfo {
        pin: Some([7; 32]),
        ..pinned
    };
    let (result, _) = connect_with(impostor, &id, false).await;
    assert!(
        matches!(
            result,
            Err(ConnectError::Rendezvous(RendezvousError::PinMismatch))
        ),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn server_certificate_persists_across_restarts() {
    let dir = support::temp_dir();
    let Some(first) = TestServer::start(Options {
        dir: Some(dir.clone()),
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let fingerprint = first.fingerprint;
    first.stop();
    let second = TestServer::start(Options {
        dir: Some(dir),
        ..Default::default()
    })
    .await
    .expect("server restarts");
    assert_eq!(second.fingerprint, fingerprint);
}

#[tokio::test]
async fn admin_blocklist_applies_without_restart() {
    let Some(server) = start_server().await else {
        return;
    };
    let (_host, hooks, id) = start_host(&server).await;
    let mut status = hooks.status.subscribe();

    server.block_device(&id);

    // The running server notices the file within seconds and drops the host.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            status.changed().await.unwrap();
            if matches!(*status.borrow(), Some(RendezvousStatus::Offline { .. })) {
                return;
            }
        }
    })
    .await
    .expect("blocked host is disconnected");
    let (result, _) = connect_by_id(&server, &id, false).await;
    assert!(
        matches!(
            result,
            Err(ConnectError::Rendezvous(RendezvousError::NotFound))
        ),
        "{:?}",
        result.err()
    );
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[tokio::test]
async fn account_devices_connect_without_password() {
    let Some(server) = TestServer::start(Options {
        accounts: "open".into(),
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let si = info(&server);
    let endpoint = rc_transport::client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();

    // Sign up on the host, which then trusts the account and joins it.
    let password = "bardzo dlugie haslo";
    let signup = rc_rendezvous::account_register(&endpoint, &si, "Ania", password, "", "Ania K.")
        .await
        .expect("sign-up");
    assert_eq!(signup.name, "ania");
    let host_identity = Arc::new(DeviceIdentity::generate());
    let trust = AccountTrust {
        name: "ania".into(),
        public_key: signup.key.public(),
        revoked: Vec::new(),
        disabled: false,
    };
    let (_host, hooks, id) = start_host_full(si.clone(), host_identity.clone(), Some(trust)).await;
    let noise = host_identity.noise_public();
    let signature = host_identity.sign(&rc_crypto::account::bind_message("ania", &id, &noise));
    let devices = rc_rendezvous::account_call(
        &endpoint,
        &si,
        &signup.token,
        rc_rendezvous::AccountOp::BindDevice {
            device_id: id.clone(),
            noise_public: noise,
            verifying_key: host_identity.verifying_key(),
            signature,
            device_name: "Biuro".into(),
        },
    )
    .await
    .expect("bind host");
    assert_eq!(devices.len(), 1);
    assert!(devices[0].online, "host is online");
    // Someone else's ID cannot be put on the account.
    let stranger = DeviceIdentity::generate();
    let forged = rc_rendezvous::account_call(
        &endpoint,
        &si,
        &signup.token,
        rc_rendezvous::AccountOp::BindDevice {
            device_id: id.clone(),
            noise_public: stranger.noise_public(),
            verifying_key: stranger.verifying_key(),
            signature: stranger.sign(&rc_crypto::account::bind_message(
                "ania",
                &id,
                &stranger.noise_public(),
            )),
            device_name: "cudzy".into(),
        },
    )
    .await;
    assert!(forged.is_err(), "ID of another device refused");

    // A laptop logs in with the password: its certificate opens the host.
    let laptop = Arc::new(DeviceIdentity::generate());
    let login = rc_rendezvous::account_login(&endpoint, &si, "ania", password)
        .await
        .expect("login");
    let cert = login
        .key
        .issue_cert("ania", &laptop.noise_public(), unix_now());
    let (session, _) = connect_full(si.clone(), &id, false, laptop.clone(), "", cert.clone()).await;
    let session = session.expect("account device connects without password");
    assert_eq!(session.peer.auth, AuthMethod::Account);
    {
        let peers = hooks.peers.lock().unwrap();
        let host_side = peers.last().unwrap();
        assert_eq!(host_side.auth, AuthMethod::Account);
        assert_eq!(host_side.account.as_deref(), Some("ania"));
    }
    drop(session);

    // Wrong password: no login. Other device without certificate: refused.
    assert!(
        rc_rendezvous::account_login(&endpoint, &si, "ania", "zle haslo")
            .await
            .is_err()
    );
    let (refused, _) = connect_full(
        si.clone(),
        &id,
        false,
        Arc::new(DeviceIdentity::generate()),
        "",
        Vec::new(),
    )
    .await;
    assert!(matches!(refused, Err(ConnectError::AccountNotTrusted)));
    // The certificate is bound to the laptop's key: another device cannot use it.
    let (stolen, _) = connect_full(
        si.clone(),
        &id,
        false,
        Arc::new(DeviceIdentity::generate()),
        "",
        cert.clone(),
    )
    .await;
    assert!(matches!(stolen, Err(ConnectError::AccountNotTrusted)));

    // Lost laptop: revoke it; the server tells the host, which refuses it.
    rc_rendezvous::account_call(
        &endpoint,
        &si,
        &login.token,
        rc_rendezvous::AccountOp::RevokeDevice(laptop.noise_public()),
    )
    .await
    .expect("revoke");
    let mut refused_after_revoke = false;
    for _ in 0..20 {
        let (result, _) =
            connect_full(si.clone(), &id, false, laptop.clone(), "", cert.clone()).await;
        if matches!(result, Err(ConnectError::AccountNotTrusted)) {
            refused_after_revoke = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        refused_after_revoke,
        "revoked device is refused by the host"
    );
}
