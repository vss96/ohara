//! Length-prefixed framing for the IPC channel.
//!
//! Wire format: `[u32 BE length][bytes]`.
//! Max frame size is 16 MiB to guard against runaway writers.

use crate::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Read one length-prefixed frame from `reader`.
///
/// Returns [`EngineError::Internal`] for any I/O error or if the
/// declared length exceeds the 16 MiB maximum.
pub async fn read_frame<R: AsyncReadExt + Unpin>(_reader: &mut R) -> Result<Vec<u8>> {
    todo!("C.1 implementation pending")
}

/// Write one length-prefixed frame to `writer` and flush.
///
/// Returns [`EngineError::Internal`] for any I/O error.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(_writer: &mut W, _payload: &[u8]) -> Result<()> {
    todo!("C.1 implementation pending")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    #[should_panic(expected = "C.1 implementation pending")]
    async fn round_trip_preserves_payload() {
        let payload = b"hello ipc world";
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        write_frame(&mut writer, payload).await.unwrap();
        let received = read_frame(&mut reader).await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    #[should_panic(expected = "C.1 implementation pending")]
    async fn read_rejects_oversized_frame() {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        // Write a u32::MAX length header (way over 16 MiB limit)
        writer.write_u32(u32::MAX).await.unwrap();
        let _err = read_frame(&mut reader).await;
    }
}
