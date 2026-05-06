use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("USB error: {0}")]
    Usb(String),

    #[error("device not found (VID 0x1209 / PID 0x572a)")]
    NotFound,

    #[error("device already in use by another process or tab")]
    Busy,

    #[error("frame too large: {0} bytes (max 65535 + header)")]
    FrameTooLarge(usize),

    #[error("frame truncated: expected {expected} bytes, got {got}")]
    FrameTruncated { expected: usize, got: usize },

    #[error("unexpected response opcode 0x{got:02x} (wanted 0x{wanted:02x})")]
    UnexpectedResponse { got: u8, wanted: u8 },

    #[error("JSON encode error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// In nusb 0.1.x, `nusb::Error` is an alias for `std::io::Error`, so the
// `#[from] std::io::Error` above already covers it. `TransferError` is its
// own enum, so we map it explicitly:
impl From<nusb::transfer::TransferError> for Error {
    fn from(e: nusb::transfer::TransferError) -> Self {
        Error::Usb(e.to_string())
    }
}
