use std::{collections::BTreeMap, io};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{
    config::{PeerConfig, PeerPermissions},
    metrics::SessionMetricsSnapshot,
};

pub const MAX_CONTROL_MESSAGE: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Status,
    ReloadConfig,
    Activate {
        peer: String,
    },
    Local,
    ListPeers,
    AddPeer {
        peer: String,
        record: PeerConfig,
    },
    RevokePeer {
        peer: String,
    },
    SetPeerPermissions {
        peer: String,
        permissions: PeerPermissions,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Ack,
    Status(Box<DaemonStatus>),
    Peers {
        peers: BTreeMap<String, PeerPermissions>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatus {
    pub identity: String,
    pub ownership: OwnershipStatus,
    pub selected_peer: Option<String>,
    pub session_epoch: Option<String>,
    pub transport_generation: Option<u64>,
    pub activation_id: Option<u64>,
    pub metrics: BTreeMap<String, SessionMetricsSnapshot>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OwnershipStatus {
    Idle,
    Arming,
    Remote,
    Releasing,
}

pub async fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), ControlError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(message).map_err(ControlError::Encode)?;
    if payload.len() > MAX_CONTROL_MESSAGE {
        return Err(ControlError::TooLarge(payload.len()));
    }
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_message<R, T>(reader: &mut R) -> Result<T, ControlError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = reader.read_u32().await? as usize;
    if length > MAX_CONTROL_MESSAGE {
        return Err(ControlError::TooLarge(length));
    }
    let mut payload = vec![0; length];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(ControlError::Decode)
}

#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &tokio::net::UnixStream) -> Result<u32, ControlError> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let credentials = getsockopt(stream, PeerCredentials).map_err(ControlError::Credentials)?;
    Ok(credentials.uid())
}

#[cfg(target_os = "linux")]
pub fn authorize_peer(
    stream: &tokio::net::UnixStream,
    daemon_uid: u32,
    active_uid: Option<u32>,
) -> Result<u32, ControlError> {
    let uid = peer_uid(stream)?;
    if uid == 0 || uid == daemon_uid || active_uid == Some(uid) {
        Ok(uid)
    } else {
        Err(ControlError::Unauthorized(uid))
    }
}

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("local control I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("could not encode local control message: {0}")]
    Encode(serde_json::Error),
    #[error("could not decode local control message: {0}")]
    Decode(serde_json::Error),
    #[error("local control message is too large: {0} bytes")]
    TooLarge(usize),
    #[cfg(target_os = "linux")]
    #[error("could not read local peer credentials: {0}")]
    Credentials(nix::Error),
    #[cfg(target_os = "linux")]
    #[error("local uid {0} is not authorized")]
    Unauthorized(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_framed_request() {
        let request = Request::Activate {
            peer: "desk".into(),
        };
        let mut encoded = Vec::new();
        write_message(&mut encoded, &request).await.unwrap();
        let decoded: Request = read_message(&mut encoded.as_slice()).await.unwrap();
        assert_eq!(decoded, request);
    }

    #[tokio::test]
    async fn rejects_declared_length_before_allocation() {
        let bytes = ((MAX_CONTROL_MESSAGE + 1) as u32).to_be_bytes();
        let error = read_message::<_, Request>(&mut bytes.as_slice())
            .await
            .unwrap_err();
        assert!(matches!(error, ControlError::TooLarge(_)));
    }
}
