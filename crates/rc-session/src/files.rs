//! File transfer over the bulk stream, in either direction.
//!
//! ```text
//! sender                               receiver
//!   Offer(id, name, size, blake3) ──▶   (permission check, find partial file)
//!                                 ◀──  Accept(resume_offset) | Error
//!   Chunk(offset, data) …        ──▶   append to <downloads>/.<hash>.rcpart
//!   Done                         ──▶   verify BLAKE3, rename to final name
//!                                 ◀──  Done | Error
//! ```
//! Either side may send Cancel. A partial file survives cancellation and
//! disconnects, so offering the same file again resumes where it stopped.

use std::collections::{HashMap, HashSet};
use std::io::{Read, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::Rng;
use rc_proto::{
    envelope::Msg, file_op::Op, FileAccept, FileCancel, FileDone, FileError, FileOffer, FileOp,
};
use rc_transport::{Sender, StreamReceiver};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;

const CHUNK: usize = 128 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
/// The receiver re-hashes the whole file before confirming.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(600);
/// Simultaneous incoming files per session.
const MAX_INCOMING: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    /// Hashing the file before offering it.
    Preparing,
    Running,
    Done {
        path: Option<PathBuf>,
    },
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct TransferEvent {
    pub id: u64,
    pub name: String,
    /// True when this side receives the file.
    pub incoming: bool,
    pub transferred: u64,
    pub total: u64,
    pub state: TransferState,
}

pub type TransferSink = Arc<dyn Fn(TransferEvent) + Send + Sync>;

/// A file to send: a path this process opens itself, or a file already
/// opened on someone's behalf (the service agent runs as SYSTEM and opens
/// files as the logged-in user, so it never reads what the user could not).
#[derive(Debug)]
pub enum OutgoingFile {
    Path(PathBuf),
    Opened { name: String, file: std::fs::File },
}

impl OutgoingFile {
    fn name(&self) -> String {
        match self {
            OutgoingFile::Path(path) => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "plik".into()),
            OutgoingFile::Opened { name, .. } => name.clone(),
        }
    }

    /// A handle positioned at the start of the file.
    fn open(&self) -> std::io::Result<std::fs::File> {
        match self {
            OutgoingFile::Path(path) => std::fs::File::open(path),
            OutgoingFile::Opened { file, .. } => {
                use std::io::Seek;
                let mut copy = file.try_clone()?;
                copy.seek(SeekFrom::Start(0))?;
                Ok(copy)
            }
        }
    }
}

struct Incoming {
    name: String,
    size: u64,
    hash: [u8; 32],
    part: PathBuf,
    file: tokio::fs::File,
    written: u64,
    last_event: Instant,
}

pub struct Transfers {
    sender: Sender,
    sink: TransferSink,
    allow_incoming: AtomicBool,
    download_dir: PathBuf,
    /// Replies for files this side is sending, by transfer id.
    outgoing: Mutex<HashMap<u64, mpsc::UnboundedSender<Op>>>,
    incoming: tokio::sync::Mutex<HashMap<u64, Incoming>>,
    cancelled: Mutex<HashSet<u64>>,
}

/// `<Downloads>/RemoteConn`, where received files go.
pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("RemoteConn")
}

impl Transfers {
    pub fn new(
        sender: Sender,
        sink: TransferSink,
        allow_incoming: bool,
        download_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            sender,
            sink,
            allow_incoming: AtomicBool::new(allow_incoming),
            download_dir,
            outgoing: Default::default(),
            incoming: Default::default(),
            cancelled: Default::default(),
        })
    }

    pub fn set_allow_incoming(&self, allow: bool) {
        self.allow_incoming.store(allow, Ordering::Relaxed);
    }

    /// Sends the files one after another in the background.
    pub fn send_files(self: &Arc<Self>, paths: Vec<PathBuf>) {
        self.send_outgoing(paths.into_iter().map(OutgoingFile::Path).collect());
    }

    /// Like [`Transfers::send_files`], also for already opened files.
    pub fn send_outgoing(self: &Arc<Self>, files: Vec<OutgoingFile>) {
        let me = self.clone();
        tokio::spawn(async move {
            for file in files {
                me.send_one(&file).await;
            }
        });
    }

    /// Cancels a transfer in either direction.
    pub fn cancel(self: &Arc<Self>, id: u64) {
        if self.outgoing.lock().unwrap().contains_key(&id) {
            self.cancelled.lock().unwrap().insert(id);
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            if let Some(inc) = me.incoming.lock().await.remove(&id) {
                let _ = me.send_op(Op::Cancel(FileCancel { transfer_id: id })).await;
                me.emit(
                    id,
                    &inc.name,
                    true,
                    inc.written,
                    inc.size,
                    TransferState::Cancelled,
                );
            }
        });
    }

    fn emit(
        &self,
        id: u64,
        name: &str,
        incoming: bool,
        transferred: u64,
        total: u64,
        state: TransferState,
    ) {
        (self.sink)(TransferEvent {
            id,
            name: name.to_string(),
            incoming,
            transferred,
            total,
            state,
        });
    }

    async fn send_op(&self, op: Op) -> Result<(), rc_transport::TransportError> {
        self.sender
            .send_bulk(Msg::FileOp(FileOp { op: Some(op) }))
            .await
    }

    // ----------------------------------------------------------------- send

    async fn send_one(self: &Arc<Self>, source: &OutgoingFile) {
        let id: u64 = rand::rngs::OsRng.gen();
        let name = source.name();
        let opened = source.open();
        let size = match opened.as_ref().map(|f| f.metadata()) {
            Ok(Ok(m)) if m.is_file() => m.len(),
            Ok(Err(e)) => {
                return self.emit(id, &name, false, 0, 0, TransferState::Failed(e.to_string()))
            }
            Ok(_) => {
                return self.emit(
                    id,
                    &name,
                    false,
                    0,
                    0,
                    TransferState::Failed("to nie jest plik".into()),
                )
            }
            Err(e) => {
                return self.emit(id, &name, false, 0, 0, TransferState::Failed(e.to_string()))
            }
        };
        self.emit(id, &name, false, 0, size, TransferState::Preparing);
        let file = opened.expect("checked above");
        let hash = match tokio::task::spawn_blocking(move || hash_reader(file)).await {
            Ok(Ok(hash)) => hash,
            Ok(Err(e)) => {
                return self.emit(
                    id,
                    &name,
                    false,
                    0,
                    size,
                    TransferState::Failed(e.to_string()),
                )
            }
            Err(e) => {
                return self.emit(
                    id,
                    &name,
                    false,
                    0,
                    size,
                    TransferState::Failed(e.to_string()),
                )
            }
        };

        let (tx, mut replies) = mpsc::unbounded_channel();
        self.outgoing.lock().unwrap().insert(id, tx);
        let result = self
            .stream_file(id, source, &name, size, hash, &mut replies)
            .await;
        self.outgoing.lock().unwrap().remove(&id);
        let cancelled = self.cancelled.lock().unwrap().remove(&id);
        let state = match result {
            Ok(()) => TransferState::Done { path: None },
            Err(_) if cancelled => TransferState::Cancelled,
            Err(state) => state,
        };
        let done = if state == (TransferState::Done { path: None }) {
            size
        } else {
            0
        };
        self.emit(id, &name, false, done, size, state);
    }

    async fn stream_file(
        &self,
        id: u64,
        source: &OutgoingFile,
        name: &str,
        size: u64,
        hash: [u8; 32],
        replies: &mut mpsc::UnboundedReceiver<Op>,
    ) -> Result<(), TransferState> {
        let failed = |e: &dyn std::fmt::Display| TransferState::Failed(e.to_string());
        self.send_op(Op::Offer(FileOffer {
            transfer_id: id,
            name: name.into(),
            size,
            blake3: hash.to_vec(),
            target_dir: String::new(),
        }))
        .await
        .map_err(|e| failed(&e))?;

        let offset = match tokio::time::timeout(ACCEPT_TIMEOUT, replies.recv()).await {
            Ok(Some(Op::Accept(a))) if a.resume_offset <= size => a.resume_offset,
            Ok(Some(Op::Error(e))) => return Err(TransferState::Failed(e.message)),
            Ok(Some(Op::Cancel(_))) => return Err(TransferState::Cancelled),
            Ok(_) => return Err(TransferState::Failed("nieoczekiwana odpowiedź".into())),
            Err(_) => return Err(TransferState::Failed("druga strona nie odpowiada".into())),
        };

        let mut file = tokio::fs::File::from_std(source.open().map_err(|e| failed(&e))?);
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(|e| failed(&e))?;
        let mut sent = offset;
        let mut buf = vec![0u8; CHUNK];
        let mut last_event = Instant::now();
        self.emit(id, name, false, sent, size, TransferState::Running);
        loop {
            if self.cancelled.lock().unwrap().contains(&id) {
                let _ = self
                    .send_op(Op::Cancel(FileCancel { transfer_id: id }))
                    .await;
                return Err(TransferState::Cancelled);
            }
            match replies.try_recv() {
                Ok(Op::Cancel(_)) => return Err(TransferState::Cancelled),
                Ok(Op::Error(e)) => return Err(TransferState::Failed(e.message)),
                _ => {}
            }
            let n = file.read(&mut buf).await.map_err(|e| failed(&e))?;
            if n == 0 {
                break;
            }
            self.send_op(Op::Chunk(rc_proto::FileChunk {
                transfer_id: id,
                offset: sent,
                data: buf[..n].to_vec(),
            }))
            .await
            .map_err(|e| failed(&e))?;
            sent += n as u64;
            if last_event.elapsed() >= PROGRESS_INTERVAL {
                last_event = Instant::now();
                self.emit(id, name, false, sent, size, TransferState::Running);
            }
        }
        if sent != size {
            return Err(TransferState::Failed(
                "plik zmienił się w trakcie wysyłania".into(),
            ));
        }
        self.send_op(Op::Done(FileDone { transfer_id: id }))
            .await
            .map_err(|e| failed(&e))?;
        match tokio::time::timeout(VERIFY_TIMEOUT, replies.recv()).await {
            Ok(Some(Op::Done(_))) => Ok(()),
            Ok(Some(Op::Error(e))) => Err(TransferState::Failed(e.message)),
            Ok(Some(Op::Cancel(_))) => Err(TransferState::Cancelled),
            _ => Err(TransferState::Failed("brak potwierdzenia odbioru".into())),
        }
    }

    // -------------------------------------------------------------- receive

    /// Reads the bulk stream until the session ends.
    pub async fn run(self: Arc<Self>, mut bulk: StreamReceiver) {
        while let Ok(msg) = bulk.recv().await {
            let Msg::FileOp(FileOp { op: Some(op) }) = msg else {
                continue;
            };
            match op {
                Op::Offer(offer) => self.on_offer(offer).await,
                Op::Chunk(chunk) => self.on_chunk(chunk).await,
                Op::Done(d) => {
                    if !self.route(d.transfer_id, Op::Done(d)) {
                        self.on_done(d.transfer_id).await;
                    }
                }
                Op::Accept(a) => {
                    self.route(a.transfer_id, Op::Accept(a));
                }
                Op::Cancel(c) => {
                    if !self.route(c.transfer_id, Op::Cancel(c)) {
                        self.drop_incoming(c.transfer_id, TransferState::Cancelled)
                            .await;
                    }
                }
                Op::Error(e) => {
                    if !self.route(e.transfer_id, Op::Error(e.clone())) {
                        self.drop_incoming(e.transfer_id, TransferState::Failed(e.message))
                            .await;
                    }
                }
                Op::ListDir(_) | Op::DirListing(_) => {}
            }
        }
        // Session ended: unfinished downloads stay as partial files.
        for (id, inc) in self.incoming.lock().await.drain() {
            self.emit(
                id,
                &inc.name,
                true,
                inc.written,
                inc.size,
                TransferState::Failed("przerwano połączenie".into()),
            );
        }
    }

    /// Forwards a reply to the task sending `id`; false if not ours.
    fn route(&self, id: u64, op: Op) -> bool {
        match self.outgoing.lock().unwrap().get(&id) {
            Some(tx) => {
                let _ = tx.send(op);
                true
            }
            None => false,
        }
    }

    async fn drop_incoming(&self, id: u64, state: TransferState) {
        if let Some(inc) = self.incoming.lock().await.remove(&id) {
            self.emit(id, &inc.name, true, inc.written, inc.size, state);
        }
    }

    async fn reject(&self, id: u64, message: &str) {
        let _ = self
            .send_op(Op::Error(FileError {
                transfer_id: id,
                message: message.into(),
            }))
            .await;
    }

    async fn on_offer(&self, offer: FileOffer) {
        let id = offer.transfer_id;
        if !self.allow_incoming.load(Ordering::Relaxed) {
            return self
                .reject(id, "przesyłanie plików nie jest dozwolone")
                .await;
        }
        if self.incoming.lock().await.len() >= MAX_INCOMING {
            return self.reject(id, "zbyt wiele równoczesnych plików").await;
        }
        let Ok(hash) = <[u8; 32]>::try_from(offer.blake3.as_slice()) else {
            return self.reject(id, "nieprawidłowa suma kontrolna").await;
        };
        let name = sanitize_name(&offer.name);
        let prepared = async {
            tokio::fs::create_dir_all(&self.download_dir).await?;
            let part =
                self.download_dir
                    .join(format!(".{}-{}.rcpart", hex(&hash[..8]), offer.size));
            let existing = tokio::fs::metadata(&part)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            if existing > offer.size {
                tokio::fs::remove_file(&part).await?;
            }
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&part)
                .await?;
            let written = file.metadata().await?.len();
            Ok::<_, std::io::Error>((part, file, written))
        };
        let (part, file, written) = match prepared.await {
            Ok(p) => p,
            Err(e) => {
                return self
                    .reject(id, &format!("nie można zapisać pliku: {e}"))
                    .await
            }
        };
        self.emit(id, &name, true, written, offer.size, TransferState::Running);
        self.incoming.lock().await.insert(
            id,
            Incoming {
                name,
                size: offer.size,
                hash,
                part,
                file,
                written,
                last_event: Instant::now(),
            },
        );
        let _ = self
            .send_op(Op::Accept(FileAccept {
                transfer_id: id,
                resume_offset: written,
            }))
            .await;
    }

    async fn on_chunk(&self, chunk: rc_proto::FileChunk) {
        let id = chunk.transfer_id;
        let mut incoming = self.incoming.lock().await;
        let Some(inc) = incoming.get_mut(&id) else {
            return;
        };
        let fits = chunk.offset == inc.written && inc.written + chunk.data.len() as u64 <= inc.size;
        let error = if !fits {
            Some("nieoczekiwany fragment pliku".to_string())
        } else if let Err(e) = inc.file.write_all(&chunk.data).await {
            Some(format!("błąd zapisu: {e}"))
        } else {
            None
        };
        if let Some(message) = error {
            let inc = incoming.remove(&id).expect("present");
            drop(incoming);
            self.reject(id, &message).await;
            self.emit(
                id,
                &inc.name,
                true,
                inc.written,
                inc.size,
                TransferState::Failed(message),
            );
            return;
        }
        inc.written += chunk.data.len() as u64;
        if inc.last_event.elapsed() >= PROGRESS_INTERVAL {
            inc.last_event = Instant::now();
            self.emit(
                id,
                &inc.name,
                true,
                inc.written,
                inc.size,
                TransferState::Running,
            );
        }
    }

    async fn on_done(self: &Arc<Self>, id: u64) {
        let Some(mut inc) = self.incoming.lock().await.remove(&id) else {
            return;
        };
        let _ = inc.file.flush().await;
        drop(inc.file);
        // Verification can take a while; keep reading the stream meanwhile.
        let me = self.clone();
        tokio::spawn(async move {
            let part = inc.part.clone();
            let hashed = tokio::task::spawn_blocking(move || hash_file(&part)).await;
            let ok = inc.written == inc.size && matches!(hashed, Ok(Ok(h)) if h == inc.hash);
            if !ok {
                let _ = tokio::fs::remove_file(&inc.part).await;
                me.reject(id, "suma kontrolna się nie zgadza").await;
                me.emit(
                    id,
                    &inc.name,
                    true,
                    inc.written,
                    inc.size,
                    TransferState::Failed("uszkodzony plik".into()),
                );
                return;
            }
            let target = unique_path(&me.download_dir, &inc.name);
            if let Err(e) = tokio::fs::rename(&inc.part, &target).await {
                me.reject(id, &format!("nie można zapisać pliku: {e}"))
                    .await;
                me.emit(
                    id,
                    &inc.name,
                    true,
                    inc.written,
                    inc.size,
                    TransferState::Failed(e.to_string()),
                );
                return;
            }
            mark_downloaded(&target);
            let _ = me.send_op(Op::Done(FileDone { transfer_id: id })).await;
            me.emit(
                id,
                &inc.name,
                true,
                inc.size,
                inc.size,
                TransferState::Done { path: Some(target) },
            );
        });
    }
}

/// Marks a received file as coming from another computer (Windows `Mark of
/// the Web`), so SmartScreen and Office treat it like a download and warn
/// before running it or enabling macros.
fn mark_downloaded(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        if let Err(e) = std::fs::write(&stream, b"[ZoneTransfer]\r\nZoneId=3\r\n") {
            tracing::debug!("cannot mark {}: {e}", path.display());
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    hash_reader(std::fs::File::open(path)?)
}

fn hash_reader(mut file: std::fs::File) -> std::io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Keeps only a safe file name: no directories, no characters Windows
/// forbids, no reserved device names.
pub fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        // Bidi overrides would let "photo\u{202E}gpj.exe" pose as a picture.
        .filter(|&c| !rc_proto::text::is_spoofing_char(c))
        .collect();
    let trimmed = cleaned
        .trim_matches(|c: char| c == '.' || c == ' ')
        .to_string();
    let stem = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    let mut result = if trimmed.is_empty() {
        "plik".to_string()
    } else if reserved {
        format!("_{trimmed}")
    } else {
        trimmed
    };
    if result.len() > 200 {
        let mut cut = 200;
        while !result.is_char_boundary(cut) {
            cut -= 1;
        }
        result.truncate(cut);
    }
    result
}

/// `dir/name`, or `dir/name (1).ext`, … if taken.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .expect("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_name("raport.pdf"), "raport.pdf");
        assert_eq!(sanitize_name("../../Windows/system32/evil.dll"), "evil.dll");
        assert_eq!(sanitize_name("C:\\Users\\x\\a.txt"), "a.txt");
        assert_eq!(sanitize_name("a<b>:c?.txt"), "a_b__c_.txt");
        assert_eq!(sanitize_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_name("com1"), "_com1");
        assert_eq!(sanitize_name("..."), "plik");
        assert_eq!(sanitize_name(""), "plik");
        assert_eq!(sanitize_name("zażółć.png"), "zażółć.png");
        assert_eq!(sanitize_name("zdjecie\u{202E}gpj.exe"), "zdjeciegpj.exe");
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("rc-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        std::fs::write(dir.join("a (1).txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        assert_eq!(unique_path(&dir, "b.txt"), dir.join("b.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
