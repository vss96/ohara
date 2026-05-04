//! Length-prefixed framing for the IPC channel.
//!
//! Wire format: `[u32 BE length][bytes]`.
//! Max frame size is 16 MiB to guard against runaway writers.

use crate::{EngineError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024; // 16 MiB

/// Read one length-prefixed frame from `reader`.
///
/// Returns [`EngineError::Internal`] for any I/O error or if the
/// declared length exceeds the 16 MiB maximum.
pub async fn read_frame<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let len = reader
        .read_u32()
        .await
        .map_err(|e| EngineError::Internal(format!("ipc read length: {e}")))?;
    if len > MAX_FRAME_BYTES {
        return Err(EngineError::Internal(format!(
            "ipc frame too large: {len} bytes (max {MAX_FRAME_BYTES})"
        )));
    }
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|e| EngineError::Internal(format!("ipc read body: {e}")))?;
    Ok(buf)
}

/// Write one length-prefixed frame to `writer` and flush.
///
/// Returns [`EngineError::Internal`] for any I/O error.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let len = payload.len() as u32;
    writer
        .write_u32(len)
        .await
        .map_err(|e| EngineError::Internal(format!("ipc write length: {e}")))?;
    writer
        .write_all(payload)
        .await
        .map_err(|e| EngineError::Internal(format!("ipc write body: {e}")))?;
    writer
        .flush()
        .await
        .map_err(|e| EngineError::Internal(format!("ipc flush: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_preserves_payload() {
        let payload = b"hello ipc world";
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        write_frame(&mut writer, payload).await.unwrap();
        let received = read_frame(&mut reader).await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn read_rejects_oversized_frame() {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        // Write a u32::MAX length header (way over 16 MiB limit)
        writer.write_u32(u32::MAX).await.unwrap();
        let err = read_frame(&mut reader).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too large") || msg.contains("internal"),
            "expected oversized-frame error, got: {msg}"
        );
    }
}
