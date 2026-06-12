use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Content is streamed in chunks of this size after the response header.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// CBOR header frames are tiny; anything bigger is a protocol violation.
pub const MAX_FRAME_SIZE: u32 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRequest {
    pub item_hash: String,
    /// Reserved for future range requests; must be 0.
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResponseHeader {
    pub found: bool,
    pub size: u64,
}

pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = cbor4ii::serde::to_vec(Vec::new(), value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if bytes.len() > MAX_FRAME_SIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

pub async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = reader.read_u32().await?;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    cbor4ii::serde::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(8192);
        let request = FetchRequest {
            item_hash: "ab".repeat(32),
            offset: 0,
        };
        write_frame(&mut a, &request).await.unwrap();
        let read: FetchRequest = read_frame(&mut b).await.unwrap();
        assert_eq!(read, request);
    }

    #[tokio::test]
    async fn header_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(8192);
        let header = FetchResponseHeader {
            found: true,
            size: 12345,
        };
        write_frame(&mut a, &header).await.unwrap();
        let read: FetchResponseHeader = read_frame(&mut b).await.unwrap();
        assert_eq!(read, header);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(16384);
        // Hand-craft a frame announcing more than MAX_FRAME_SIZE bytes.
        tokio::io::AsyncWriteExt::write_u32(&mut a, MAX_FRAME_SIZE + 1)
            .await
            .unwrap();
        let result: std::io::Result<FetchRequest> = read_frame(&mut b).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn truncated_frame_errors() {
        let (mut a, mut b) = tokio::io::duplex(8192);
        tokio::io::AsyncWriteExt::write_u32(&mut a, 100)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut a, &[0u8; 10])
            .await
            .unwrap();
        drop(a);
        let result: std::io::Result<FetchRequest> = read_frame(&mut b).await;
        assert!(result.is_err());
    }

    #[test]
    fn item_hash_validation() {
        use crate::fetch::is_valid_item_hash;
        assert!(is_valid_item_hash(&"a".repeat(64)));
        assert!(is_valid_item_hash(
            "QmZkurbY2G2hWay59yiTgQNaQxHSNzKZFt2jbnwJhQcKgV"
        ));
        assert!(!is_valid_item_hash(""));
        assert!(!is_valid_item_hash("../etc/passwd"));
        assert!(!is_valid_item_hash(&"a".repeat(129)));
        assert!(!is_valid_item_hash("abc/def"));
    }
}
