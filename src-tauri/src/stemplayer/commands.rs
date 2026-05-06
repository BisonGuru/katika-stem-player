//! `0x04` command opcode and its sub-commands.
//!
//! Every entry here was observed on the wire. Names are guesses based on
//! the surrounding context — a bare `0x04 0x00` always shows up at session
//! start; `0x04 0x02` always at session end; `0x04 0x05` carries an album
//! identifier and is followed by `0x04 0x06` (track-select) for each track
//! in that album, etc.

use serde::{Deserialize, Serialize};

use crate::stemplayer::device::{AsFrame, StemDevice};
use crate::stemplayer::error::Result;
use crate::stemplayer::frame::{Frame, OP_CMD};

/// Sub-command bytes that go in the payload of an `OP_CMD` (0x04) frame.
/// Names taken from the krystalgamer/stem-player-emulator decode.
#[allow(dead_code)]
pub mod sub {
    pub const REBOOT: u8 = 0x00;
    pub const VERSION: u8 = 0x01;
    pub const GET_STORAGE_INFO: u8 = 0x02;
    pub const GET_TRACKS_INFO: u8 = 0x03;
    pub const GET_DEVICE_CONFIG: u8 = 0x04;
    /// payload: `{"album":"A1"}`
    pub const GET_ALBUM_CONFIG: u8 = 0x05;
    /// payload: `{"album":"A1","track":"T1"}`
    pub const GET_TRACK_CONFIG: u8 = 0x06;
    pub const GET_ALBUM_COVER: u8 = 0x07;
    /// payload: `{"album":"A4"}` — creates the slot if it doesn't exist
    pub const ADD_ALBUM: u8 = 0x08;
    pub const DELETE_ALBUM: u8 = 0x09;
    pub const DELETE_TRACK: u8 = 0x0a;
    pub const GET_MUSIC_FILE: u8 = 0x0b;
    pub const GET_RECORDING_SLOTS: u8 = 0x0c;
    pub const GET_RECORDING: u8 = 0x0d;
    pub const DELETE_RECORDING: u8 = 0x0e;
    pub const RENAME_ALBUM: u8 = 0x0f;
    pub const MOVE_TRACK: u8 = 0x10;
    pub const GET_STATE_OF_CHARGE: u8 = 0x11;
    /// payload: `{"challenge": "..."}` — cryptographic auth handshake
    pub const CHALLENGE: u8 = 0x12;

    // Legacy aliases used by older code paths (kept for compatibility).
    pub const SESSION_BEGIN: u8 = REBOOT;
    pub const SESSION_END: u8 = GET_STORAGE_INFO;
    pub const ENUM_BEGIN: u8 = GET_TRACKS_INFO;
    pub const SELECT_ALBUM: u8 = GET_ALBUM_CONFIG;
    pub const SELECT_TRACK: u8 = GET_TRACK_CONFIG;
}

/// File-push announce types (the `type` field in the `0x06` JSON payload).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilePushType {
    /// Captured: pushed first during a firmware update — JSON tunables.
    DeviceConfig,
    /// Captured: the firmware blob itself (e.g. `3_stpl.dfu`).
    Dfu,
    /// Inferred: per-track stem MP3 push. Type string not yet confirmed.
    Track,
    /// Anything else — we'll forward the literal string.
    Other(String),
}

impl FilePushType {
    pub fn as_str(&self) -> &str {
        match self {
            Self::DeviceConfig => "device-config",
            Self::Dfu => "dfu",
            Self::Track => "track",
            Self::Other(s) => s.as_str(),
        }
    }
}

/// A `0x04` command frame (sub-command + optional JSON payload).
#[derive(Debug, Clone)]
pub struct Cmd04 {
    pub sub: u8,
    pub payload: Vec<u8>,
}

impl Cmd04 {
    pub fn bare(sub: u8) -> Self {
        Self {
            sub,
            payload: Vec::new(),
        }
    }

    pub fn with_json<T: Serialize>(sub: u8, json: &T) -> Result<Self> {
        let mut payload = serde_json::to_vec(json)?;
        // Captures show JSON is NUL-terminated.
        payload.push(0);
        Ok(Self { sub, payload })
    }

    pub fn select_album(slot: &str) -> Result<Self> {
        #[derive(Serialize)]
        struct A<'a> {
            album: &'a str,
        }
        Self::with_json(sub::SELECT_ALBUM, &A { album: slot })
    }

    pub fn select_track(album: &str, track: &str) -> Result<Self> {
        #[derive(Serialize)]
        struct T<'a> {
            album: &'a str,
            track: &'a str,
        }
        Self::with_json(sub::SELECT_TRACK, &T { album, track })
    }
}

impl AsFrame for Cmd04 {
    fn as_frame(&self) -> Frame {
        let mut payload = Vec::with_capacity(1 + self.payload.len());
        payload.push(self.sub);
        payload.extend_from_slice(&self.payload);
        Frame::new(OP_CMD, payload)
    }
}

/// What we return to the frontend after enumerating the library.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumInfo {
    pub slot: String,
    pub tracks: Vec<String>,
}

/// Walk all four album slots and probe which tracks exist.
///
/// This is a *probe* — the host-only opcodes never tell us "how many tracks
/// are in this album", so we bash on `T1..Tn` until the device errors and
/// stop. A real implementation will replace this once we decode the IN
/// frames returned by `select_album` (which carry album metadata).
pub async fn enumerate_library(dev: &mut StemDevice) -> Result<Vec<AlbumInfo>> {
    dev.send_command(&Cmd04::bare(sub::SESSION_BEGIN)).await?;
    let _ = dev.read_response().await?;

    dev.send_command(&Cmd04::bare(sub::ENUM_BEGIN)).await?;
    let _ = dev.read_response().await?;

    let mut albums = Vec::new();
    for slot in ["A1", "A2", "A3", "A4"] {
        dev.send_command(&Cmd04::select_album(slot)?).await?;
        let _ = dev.read_response().await?;

        let mut tracks = Vec::new();
        for n in 1..=32 {
            let track = format!("T{n}");
            if dev
                .send_command(&Cmd04::select_track(slot, &track)?)
                .await
                .is_err()
            {
                break;
            }
            match dev.read_response().await {
                Ok(_) => tracks.push(track),
                Err(_) => break,
            }
        }

        albums.push(AlbumInfo {
            slot: slot.to_string(),
            tracks,
        });
    }

    dev.send_command(&Cmd04::bare(sub::SESSION_END)).await?;
    let _ = dev.read_response().await?;
    Ok(albums)
}
