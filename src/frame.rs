//! Length-prefixed frames (a big-endian `u32`, then the bytes) for Navette's own small
//! messages: the pairing exchange and the link's hello and heartbeat. gliff's framing will run
//! beside them for the media.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Far above any Navette message: a peer cannot make the other side allocate more.
pub const MAX_FRAME: usize = 16 * 1024;

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_cross_an_in_memory_stream() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let sent = async {
            write_frame(&mut a, b"first").await.unwrap();
            write_frame(&mut a, b"").await.unwrap();
        };
        let received = async {
            (
                read_frame(&mut b).await.unwrap(),
                read_frame(&mut b).await.unwrap(),
            )
        };
        let ((), (first, empty)) = tokio::join!(sent, received);
        assert_eq!(first, b"first");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_it_is_read() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let error = read_frame(&mut b).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
