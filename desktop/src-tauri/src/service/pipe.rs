//! Messages on the service and agent pipes: one JSON object per line.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};

use crate::host_cmds::PermissionsDto;

/// Longest accepted line (file lists, configuration).
const MAX_LINE: usize = 1 << 20;

/// App window → agent.
#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentCmd {
    Status,
    NewPassword,
    Respond {
        id: u64,
        accept: bool,
        permissions: PermissionsDto,
        trust: bool,
    },
    Chat {
        session_id: u64,
        text: String,
    },
    Disconnect {
        session_id: u64,
    },
    SendFiles {
        session_id: u64,
        paths: Vec<String>,
    },
    CancelTransfer {
        session_id: u64,
        id: u64,
    },
}

/// Agent or elevated helper → service.
#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServiceCmd {
    /// Ctrl+Alt+Del (from the agent).
    Sas,
    /// New settings and secrets (from the elevated helper).
    Configure(Box<ServiceConfig>),
}

/// What the service needs to share the screen as this user would.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceConfig {
    pub config: crate::config::Config,
    /// Device identity (hex), so the ID stays the same as in the app.
    pub identity: String,
    pub permanent_password: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request<T> {
    pub id: u64,
    pub cmd: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Message {
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    /// A UI event, re-emitted by the app window as is.
    Event { name: String, payload: Value },
}

pub async fn write_line<W: AsyncWrite + Unpin>(
    w: &mut W,
    value: &impl Serialize,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

pub struct LineReader<R> {
    lines: Lines<BufReader<R>>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub fn new(r: R) -> Self {
        Self {
            lines: BufReader::with_capacity(64 * 1024, r).lines(),
        }
    }

    /// The next message; `None` at end of stream or on garbage.
    pub async fn next<T: DeserializeOwned>(&mut self) -> Option<T> {
        let line = self.lines.next_line().await.ok()??;
        if line.len() > MAX_LINE {
            return None;
        }
        serde_json::from_str(&line).ok()
    }
}
