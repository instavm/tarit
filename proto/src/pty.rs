//! Shared framing helpers for interactive PTY streams.
//!
//! Frame format, used on both the API UDS leg and the VMM↔guest vsock leg:
//! `[1 byte TYPE][4 bytes BE u32 LEN][LEN bytes PAYLOAD]`.

use serde::Serialize;
use std::io::{self, Read, Write};

pub const TYPE_DATA: u8 = 0;
pub const TYPE_RESIZE: u8 = 1;
pub const TYPE_EXIT: u8 = 2;
pub const TYPE_ERROR: u8 = 3;
pub const TYPE_START: u8 = 4;

pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyStreamFrame {
    pub frame_type: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PtyStart {
    pub cols: u16,
    pub rows: u16,
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PtyResize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct PtyExit {
    pub exit_code: i32,
}

pub fn write_frame<W: Write>(writer: &mut W, frame_type: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PTY stream frame too large",
        ));
    }

    let mut header = [0u8; 5];
    header[0] = frame_type;
    header[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    Ok(())
}

pub fn write_json_frame<W: Write, T: Serialize>(
    writer: &mut W,
    frame_type: u8,
    payload: &T,
) -> io::Result<()> {
    let body =
        serde_json::to_vec(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame(writer, frame_type, &body)
}

pub fn write_error_frame<W: Write>(writer: &mut W, message: &str) -> io::Result<()> {
    write_frame(writer, TYPE_ERROR, message.as_bytes())
}

pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<PtyStreamFrame> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes(
        header[1..5]
            .try_into()
            .expect("PTY frame length field is exactly 4 bytes"),
    ) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PTY stream frame too large",
        ));
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(PtyStreamFrame {
        frame_type: header[0],
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_frame_round_trips_with_big_endian_length() {
        let mut buf = Vec::new();
        write_frame(&mut buf, TYPE_DATA, b"hello").unwrap();

        assert_eq!(&buf[..5], &[TYPE_DATA, 0, 0, 0, 5]);
        let frame = read_frame(&mut &buf[..]).unwrap();
        assert_eq!(frame.frame_type, TYPE_DATA);
        assert_eq!(frame.payload, b"hello");
    }

    #[test]
    fn json_start_frame_uses_exact_shape() {
        let mut buf = Vec::new();
        write_json_frame(
            &mut buf,
            TYPE_START,
            &PtyStart {
                cols: 120,
                rows: 40,
                shell: Some("/bin/bash".into()),
            },
        )
        .unwrap();

        let frame = read_frame(&mut &buf[..]).unwrap();
        assert_eq!(frame.frame_type, TYPE_START);
        let json: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
        assert_eq!(json["cols"], 120);
        assert_eq!(json["rows"], 40);
        assert_eq!(json["shell"], "/bin/bash");
    }

    #[test]
    fn empty_unknown_frame_is_decodable_for_forward_compat() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 99, &[]).unwrap();
        let frame = read_frame(&mut &buf[..]).unwrap();
        assert_eq!(frame.frame_type, 99);
        assert!(frame.payload.is_empty());
    }
}
