//! Length-prefixed JSON framing used by Aegis IPC v24.

use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Read, Write};
use zeroize::Zeroize as _;

pub const MAX_FRAME: usize = 16 * 1024 * 1024;

pub(crate) fn write_msg<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(json_error)?;
    if bytes.len() > MAX_FRAME {
        bytes.zeroize();
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {} exceeds {MAX_FRAME}", bytes.len()),
        ));
    }
    let length = bytes.len() as u32;
    let result = writer
        .write_all(&length.to_le_bytes())
        .and_then(|()| writer.write_all(&bytes))
        .and_then(|()| writer.flush());
    bytes.zeroize();
    result
}

pub(crate) fn read_msg<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {length} exceeds {MAX_FRAME}"),
        ));
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes)?;
    let result = serde_json::from_slice(&bytes).map_err(json_error);
    bytes.zeroize();
    result
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_round_trips_and_bounds_allocations() {
        let mut bytes = Vec::new();
        write_msg(&mut bytes, &serde_json::json!({"type": "Subscribe"})).unwrap();
        let decoded: serde_json::Value = read_msg(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(decoded["type"], "Subscribe");

        let mut oversized = Cursor::new(((MAX_FRAME as u32) + 1).to_le_bytes());
        assert!(read_msg::<_, serde_json::Value>(&mut oversized).is_err());
    }
}
