//! Versioned, bounded, language-neutral session framing. Lengths are network
//! byte order; payloads are UTF-8 JSON with explicit tags, never Swift enum ABI.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_MAJOR: u16 = 2;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_CAPABILITIES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid or oversized frame")]
    InvalidFrame,
    #[error("unsupported protocol major")]
    VersionMismatch,
    #[error("required capability is unavailable")]
    MissingCapability,
    #[error("invalid capabilities")]
    InvalidCapabilities,
    #[error("connection closed or I/O failed")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub major: u16,
    pub capabilities: Vec<String>,
    pub required: Vec<String>,
}

impl Hello {
    pub fn negotiate(&self, peer: &Self) -> Result<Vec<String>, ProtocolError> {
        if self.major != PROTOCOL_MAJOR || peer.major != self.major {
            return Err(ProtocolError::VersionMismatch);
        }
        let ours = capabilities(&self.capabilities)?;
        let theirs = capabilities(&peer.capabilities)?;
        let our_required = capabilities(&self.required)?;
        let their_required = capabilities(&peer.required)?;
        if !our_required.is_subset(&ours) || !their_required.is_subset(&theirs) {
            return Err(ProtocolError::InvalidCapabilities);
        }
        if !our_required.is_subset(&theirs) || !their_required.is_subset(&ours) {
            return Err(ProtocolError::MissingCapability);
        }
        Ok(ours
            .intersection(&theirs)
            .map(|s| (*s).to_owned())
            .collect())
    }
}

fn capabilities(values: &[String]) -> Result<BTreeSet<&str>, ProtocolError> {
    if values.len() > MAX_CAPABILITIES {
        return Err(ProtocolError::InvalidCapabilities);
    }
    let mut set = BTreeSet::new();
    for value in values {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
            || !set.insert(value.as_str())
        {
            return Err(ProtocolError::InvalidCapabilities);
        }
    }
    Ok(set)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> Result<(), ProtocolError> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidFrame);
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, ProtocolError> {
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidFrame);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

pub async fn exchange_hello<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    hello: &Hello,
) -> Result<Vec<String>, ProtocolError> {
    hello.negotiate(hello)?;
    let bytes = serde_json::to_vec(hello).map_err(|_| ProtocolError::InvalidFrame)?;
    // Both sides can write their bounded hello before reading (under 16 KiB).
    write_frame(writer, &bytes).await?;
    let peer = serde_json::from_slice(&read_frame(reader).await?)
        .map_err(|_| ProtocolError::InvalidFrame)?;
    hello.negotiate(&peer)
}
