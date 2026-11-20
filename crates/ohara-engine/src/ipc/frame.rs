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
    read_body(reader, len).await
}

/// Like [`read_frame`], but `Ok(None)` when the peer closes the connection
/// before sending any byte (e.g. a connect-only reachability probe).
pub async fn read_frame_or_eof<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let n = reader
        .read(&mut header)
        .await
        .map_err(|e| EngineError::Internal(format!("ipc read length: {e}")))?;
    if n == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut header[n..])
        .await
        .map_err(|e| EngineError::Internal(format!("ipc read length: {e}")))?;
    read_body(reader, u32::from_be_bytes(header))
        .await
        .map(Some)
}

async fn read_body<R: AsyncReadExt + Unpin>(reader: &mut R, len: u32) -> Result<Vec<u8>> {
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
/// Returns [`EngineError::Internal`] if the payload exceeds the 16 MiB
/// maximum (matching the read-side limit) or for any I/O error.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    if payload.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(EngineError::Internal(format!(
            "ipc frame body too large: {} bytes (max {MAX_FRAME_BYTES})",
            payload.len()
        )));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| EngineError::Internal("frame body too large for u32".into()))?;
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
    async fn connect_and_close_without_a_frame_is_not_an_error() {
        let (writer, mut reader) = tokio::io::duplex(64);
        drop(writer);
        assert!(read_frame_or_eof(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn read_frame_or_eof_reads_a_frame_and_rejects_a_truncated_header() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        write_frame(&mut writer, b"ping").await.unwrap();
        let got = read_frame_or_eof(&mut reader).await.unwrap();
        assert_eq!(got.as_deref(), Some(&b"ping"[..]));

        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[0, 0]).await.unwrap();
        drop(writer);
        assert!(read_frame_or_eof(&mut reader).await.is_err());
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

    #[tokio::test]
    async fn write_rejects_oversized_frame() {
        let (mut writer, _reader) = tokio::io::duplex(8);
        // Construct a payload that exceeds MAX_FRAME_BYTES (16 MiB).
        // We don't allocate the full buffer — just need len > limit.
        let oversized = vec![0u8; (MAX_FRAME_BYTES as usize) + 1];
        let err = write_frame(&mut writer, &oversized).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too large") || msg.contains("internal"),
            "expected oversized-frame write error, got: {msg}"
        );
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            #[test]
            fn round_trip_preserves_arbitrary_payloads(
                payload in proptest::collection::vec(any::<u8>(), 0..=65_536)
            ) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let (mut writer, mut reader) = tokio::io::duplex(payload.len().max(1) + 16);
                    write_frame(&mut writer, &payload).await.unwrap();
                    let received = read_frame(&mut reader).await.unwrap();
                    prop_assert_eq!(received, payload);
                    Ok(())
                })?;
            }

            #[test]
            fn read_frame_rejects_any_length_above_max_frame_bytes(
                oversized_len in (MAX_FRAME_BYTES + 1)..=u32::MAX
            ) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let (mut writer, mut reader) = tokio::io::duplex(8);
                    writer.write_u32(oversized_len).await.unwrap();
                    let res = read_frame(&mut reader).await;
                    prop_assert!(res.is_err());
                    Ok(())
                })?;
            }
        }
    }
}
