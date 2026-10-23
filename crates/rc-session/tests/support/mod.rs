//! Runs the real `rc-server` program for the tests that need a rendezvous
//! server. The server lives in its own repository; its binary is taken from
//! `RC_SERVER_BIN`, or from a `RemoteConn-server` checkout next to this
//! repository (`target/release` or `target/debug`). Without it those tests
//! are skipped (with a message), so the rest of the suite still runs.

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rc_rendezvous::ServerInfo;

pub struct TestServer {
    child: Child,
    bin: PathBuf,
    pub addr: SocketAddr,
    pub dir: PathBuf,
    pub fingerprint: [u8; 32],
}

#[derive(Default)]
pub struct Options {
    pub access_key: String,
    /// `RC_ACCOUNTS` (empty: the server default, invite).
    pub accounts: String,
    /// Data directory to reuse (restart tests); a fresh one otherwise.
    pub dir: Option<PathBuf>,
}

fn exe_name() -> &'static str {
    if cfg!(windows) {
        "rc-server.exe"
    } else {
        "rc-server"
    }
}

/// The server binary, if one can be found.
pub fn server_bin() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RC_SERVER_BIN") {
        return Some(PathBuf::from(path));
    }
    // crates/rc-session → repository root → sibling checkout.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    ["release", "debug"]
        .iter()
        .map(|profile| {
            root.join("../RemoteConn-server/target")
                .join(profile)
                .join(exe_name())
        })
        .find(|p| p.exists())
}

fn free_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .and_then(|s| s.local_addr())
        .map(|a| a.port())
        .expect("free UDP port")
}

pub fn temp_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("rc-server-test-{}-{nanos}", std::process::id()))
}

impl TestServer {
    /// Starts the server and waits until it answers; `None` (test skipped)
    /// when no server binary is available.
    pub async fn start(options: Options) -> Option<Self> {
        let Some(bin) = server_bin() else {
            eprintln!(
                "SKIPPED: rc-server not found. Build the RemoteConn-server repository next to this \
                 one, or set RC_SERVER_BIN."
            );
            return None;
        };
        let dir = options.dir.unwrap_or_else(temp_dir);
        let endpoint = rc_transport::client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        // Tests run in parallel: the port picked here can be taken by another
        // test's server before ours binds it. Then our server exits, and we
        // try again on another port.
        for _ in 0..5 {
            let addr: SocketAddr = format!("127.0.0.1:{}", free_udp_port()).parse().unwrap();
            let child = Command::new(&bin)
                .arg("run")
                .env("RC_BIND", addr.to_string())
                .env("RC_DATA_DIR", &dir)
                .env("RC_ACCESS_KEY", &options.access_key)
                .env("RC_ACCOUNTS", &options.accounts)
                .env("RC_ADMIN_BIND", "off")
                .env("RUST_LOG", "warn")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap_or_else(|e| panic!("cannot start {}: {e}", bin.display()));
            let mut server = Self {
                child,
                bin: bin.clone(),
                addr,
                dir: dir.clone(),
                fingerprint: [0; 32],
            };
            let info = ServerInfo {
                addr,
                access_key: options.access_key.clone(),
                pin: None,
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                if !server.running() {
                    break;
                }
                let ping = rc_rendezvous::ping(&endpoint, &info);
                if let Ok(Ok(fp)) = tokio::time::timeout(Duration::from_secs(2), ping).await {
                    // Still ours (not another test's server on a reused port)?
                    if server.running() {
                        server.fingerprint = fp;
                        return Some(server);
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        panic!("rc-server did not start");
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn info(&self) -> ServerInfo {
        ServerInfo {
            addr: self.addr,
            access_key: String::new(),
            pin: None,
        }
    }

    /// `rc-server block device <id>` on this server's data directory (the
    /// running server picks it up within seconds).
    pub fn block_device(&self, id: &str) {
        let status = Command::new(&self.bin)
            .args(["block", "device", id])
            .env("RC_DATA_DIR", &self.dir)
            .stdout(Stdio::null())
            .status()
            .expect("rc-server block");
        assert!(status.success());
    }

    pub fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
