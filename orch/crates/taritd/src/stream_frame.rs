//! Shared bounded framing for the WebSocket PTY and SSH stream bridges.
use tarit_vmm_client::{PtyStreamFrame, MAX_FRAME_LEN};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub(crate) enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
}

pub(crate) async fn read<R>(reader: &mut R) -> Result<PtyStreamFrame, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 5];
    reader.read_exact(&mut header).await?;
    let frame_type = header[0];
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::Protocol("stream frame too large".into()));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(PtyStreamFrame {
        frame_type,
        payload,
    })
}

pub(crate) async fn write<W>(writer: &mut W, frame: &PtyStreamFrame) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    if frame.payload.len() > MAX_FRAME_LEN {
        return Err(FrameError::Protocol("stream frame too large".into()));
    }
    let mut header = [0u8; 5];
    header[0] = frame.frame_type;
    header[1..5].copy_from_slice(&(frame.payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(&frame.payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rejects_oversized_reads_before_allocating_and_writes_before_emitting() {
        let mut header = [0u8; 5];
        header[1..].copy_from_slice(&((MAX_FRAME_LEN + 1) as u32).to_be_bytes());
        assert!(matches!(
            read(&mut &header[..]).await,
            Err(FrameError::Protocol(_))
        ));
        let mut output = Vec::new();
        let frame = PtyStreamFrame {
            frame_type: 0,
            payload: vec![0; MAX_FRAME_LEN + 1],
        };
        assert!(matches!(
            write(&mut output, &frame).await,
            Err(FrameError::Protocol(_))
        ));
        assert!(output.is_empty());
    }
}
