//! File push protocol (`0x06` announce + `0x07` chunked content).
//!
//! Captured behaviour during a firmware update:
//!
//! 1. Host sends `0x06` with JSON `{size, type, name}` + NUL terminator.
//! 2. Device replies with a 3-byte ACK frame (`01 00 00`).
//! 3. For each ~8 KiB chunk: host sends `0x07` containing
//!        `[u32 LE chunk_size][u8 flag][chunk bytes...]`.
//!    Device ACKs each one before the next chunk goes out.
//! 4. The single-chunk variant (e.g. config.txt at 332 bytes) just sends
//!    one `0x07` with the entire payload.
//!
//! `flag` was always `0x00` in our captures. Until we know what other
//! values mean, we keep it pinned at zero.

use serde::Serialize;
use serde_json::json;

use crate::stemplayer::commands::FilePushType;
use crate::stemplayer::device::StemDevice;
use crate::stemplayer::error::Result;
use crate::stemplayer::frame::{Frame, OP_FILE_CHUNK, OP_FILE_HEADER};

/// Max chunk size observed in captures.
pub const CHUNK_SIZE: usize = 8 * 1024;

#[derive(Debug, Serialize)]
struct AnnouncePayload<'a> {
    size: usize,
    #[serde(rename = "type")]
    file_type: &'a str,
    name: &'a str,
}

fn build_announce(file_type: &str, name: &str, size: usize) -> Result<Frame> {
    let payload_struct = AnnouncePayload {
        size,
        file_type,
        name,
    };
    let mut payload = serde_json::to_vec(&payload_struct)?;
    // Captures show NUL terminator at end of JSON.
    payload.push(0);
    Ok(Frame::new(OP_FILE_HEADER, payload))
}

fn build_chunk(chunk: &[u8]) -> Frame {
    let chunk_size = chunk.len() as u32;
    let mut payload = Vec::with_capacity(5 + chunk.len());
    payload.extend_from_slice(&chunk_size.to_le_bytes());
    payload.push(0); // flag
    payload.extend_from_slice(chunk);
    Frame::new(OP_FILE_CHUNK, payload)
}

/// Push a file end-to-end. Returns once the device has ACKed the final chunk.
pub async fn push_file(
    dev: &mut StemDevice,
    file_type: &FilePushType,
    name: &str,
    bytes: &[u8],
) -> Result<()> {
    tracing::info!(
        "pushing {} {} ({} bytes, {} chunks)",
        file_type.as_str(),
        name,
        bytes.len(),
        (bytes.len() + CHUNK_SIZE - 1) / CHUNK_SIZE
    );

    // 1. Announce
    let announce = build_announce(file_type.as_str(), name, bytes.len())?;
    dev.send_frame(&announce).await?;
    dev.read_until_ack().await?;

    // 2. Chunked content
    for (i, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
        let frame = build_chunk(chunk);
        dev.send_frame(&frame).await?;
        dev.read_until_ack().await?;
        tracing::debug!("chunk {} ({} bytes) acked", i, chunk.len());
    }

    Ok(())
}

/// One complete stem track upload: album-config → track-config → 4× stem-audio-mp3.
///
/// Schemas reverse-engineered from the official client's `npm.kano.*.js`:
///
/// - **Album config** body: pretty-printed (2-space indent) JSON
///   `{id: "<UPPERCASE>", global_id, artist, title, version, tracks}` — NO null terminator.
///   FILE_HEADER: `{size, type:"album-config", album}`.
/// - **Track config** body: pretty-printed JSON `{metadata: {...}}` + ONE null byte.
///   FILE_HEADER: `{size, type:"track-config", album, track}`.
/// - **Stem audio** body: raw MP3 bytes.
///   FILE_HEADER: `{size, type:"stem-audio-mp3", album, track, stem}`.
///   Stem IDs are 1-indexed: `other=1, vocals=2, bass=3, drums=4`.
pub struct TrackBundle<'a> {
    pub album_id: &'a str,
    pub album_title: &'a str,
    pub track_id: &'a str,
    pub track_title: &'a str,
    pub global_id: &'a str,
    pub artist: &'a str,
    pub timestamp: &'a str,
    /// Stem audio bytes by name: tuples must follow the device's stem-id mapping:
    /// (other=1, vocals=2, bass=3, drums=4).
    pub other: &'a [u8],
    pub vocals: &'a [u8],
    pub bass: &'a [u8],
    pub drums: &'a [u8],
}

pub async fn push_track_bundle(dev: &mut StemDevice, b: &TrackBundle<'_>) -> Result<()> {
    push_track_bundle_with_progress(dev, b, |_| {}).await
}

/// Same as `push_track_bundle` but invokes `on_stage(label)` between each
/// device-side push so callers can drive progress UIs. Stage labels:
/// `"album-config"`, `"vocals"`, `"bass"`, `"drums"`, `"other"`,
/// `"track-config"`, `"done"`.
pub async fn push_track_bundle_with_progress<F>(
    dev: &mut StemDevice,
    b: &TrackBundle<'_>,
    mut on_stage: F,
) -> Result<()>
where
    F: FnMut(&str),
{
    // 1. album-config (pretty JSON, NO trailing NUL).
    //    Schema confirmed by wire capture of stemplayer.com pushing a track:
    //    {"id":"A5","global_id":"00000000-...","artist":"OTHER","title":"OTHER","version":"1","tracks":null}
    on_stage("album-config");
    let album_cfg = json!({
        "id": b.album_id.to_uppercase(),
        "global_id": b.global_id,
        "artist": b.artist,
        "title": b.album_title,
        "version": "1",
        "tracks": serde_json::Value::Null,
    });
    let album_cfg_bytes = serde_json::to_vec_pretty(&album_cfg)?;
    push_typed_file(
        dev,
        json!({ "type": "album-config", "album": b.album_id }),
        &album_cfg_bytes,
        false,
    )
    .await?;
    tracing::info!("album-config pushed for {}", b.album_id);

    // 2. (No track-config push YET — it goes at the END, after all stems.
    //    Sending it here causes the device to timeout because it's not
    //    yet in the right state to accept it. See step 4 below.)

    // 3. Four stem-audio-mp3 pushes IN ORDER: vocals → bass → drums → other.
    //    Stem IDs (from kano.js): other=1, vocals=2, bass=3, drums=4.
    //    The `stem` field is a NUMBER, not a string.
    //    The `track` id is LOWERCASE.
    let track_lower = b.track_id.to_lowercase();
    let stems: [(u32, &[u8], &str); 4] = [
        (2, b.vocals, "vocals"),
        (3, b.bass, "bass"),
        (4, b.drums, "drums"),
        (1, b.other, "other"),
    ];
    for (i, (id, bytes, label)) in stems.iter().enumerate() {
        // Settling delay between stems (otherwise stem N+1's announce races
        // stem N's "stored" frames and we deadlock in read_until_ack).
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        on_stage(label);
        push_typed_file(
            dev,
            json!({
                "type": "stem-audio-mp3",
                "album": b.album_id,
                "track": track_lower,
                "stem": id,
            }),
            bytes,
            false,
        )
        .await?;
        tracing::info!(
            "stem {}={} ({} bytes) pushed for {}/{}",
            id,
            label,
            bytes.len(),
            b.album_id,
            track_lower
        );
    }

    // 4. Final track-config push — the COMMIT step that creates the track
    //    row in GET_TRACKS_INFO. Body schema (extracted from kano.js
    //    uploadTrackConfig + validators):
    //      TrackColour:   array of TWO hex colour strings, each /^#[0-9A-F]{6}$/i
    //      tempos:        array of {time_ms, tempo_bpm}
    //      TrackGain_dB:  number
    //      metadata:      {artist, title, global_id, meta_version, stems_version, timestamp}
    //    Timestamp validator regex requires hours 00-11 (yes, AM only —
    //    apparently a bug in their regex they work around with safe defaults).
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    on_stage("track-config");
    let track_cfg = json!({
        "TrackColour": ["#E03E3E", "#FFFFFF"],
        "tempos": [{"time_ms": 0, "tempo_bpm": 120}],
        "TrackGain_dB": 0,
        "metadata": {
            "artist": b.artist,
            "title": b.track_title,
            "global_id": b.global_id,
            "meta_version": "1",
            "stems_version": "1",
            "timestamp": b.timestamp,
        }
    });
    let track_cfg_bytes = serde_json::to_vec_pretty(&track_cfg)?;
    push_typed_file(
        dev,
        json!({ "type": "track-config", "track": &track_lower, "album": b.album_id }),
        &track_cfg_bytes,
        true, // append NUL — matches uploadTrackConfig in kano.js
    )
    .await?;
    tracing::info!(
        "track-config (commit) pushed for {}/{}",
        b.album_id,
        track_lower
    );

    on_stage("done");
    Ok(())
}

/// Read any pending IN frames with a very short timeout, ignoring them.
/// Used between file pushes to clear out any "stem stored" notifications
/// the device might emit that we don't otherwise handle.
async fn drain_pending(dev: &mut StemDevice) -> Result<()> {
    use std::time::Duration;
    for _ in 0..3 {
        let res = tokio::time::timeout(Duration::from_millis(150), dev.read_raw()).await;
        match res {
            Ok(Ok(buf)) => {
                if let Ok(frame) = Frame::decode(&buf) {
                    tracing::debug!(?frame, "drained inter-stem frame");
                }
            }
            _ => break,
        }
    }
    Ok(())
}

/// Push a file whose FILE_HEADER carries a JSON `meta` plus a `size` field
/// auto-derived from `bytes.len()` (plus 1 if `nul_terminate_body`).
/// The body is `bytes`, optionally followed by a single NUL byte.
///
/// IMPORTANT: the device parser appears strict about JSON KEY ORDER.
/// The official client always writes `size` first, then `type`, then any
/// upload-specific fields. We rebuild `meta` in that order before
/// serialising. (Cargo.toml enables serde_json's `preserve_order`
/// feature so insertion order is honoured.)
async fn push_typed_file(
    dev: &mut StemDevice,
    meta: serde_json::Value,
    bytes: &[u8],
    nul_terminate_body: bool,
) -> Result<()> {
    use serde_json::{Map, Value};
    let body_len = bytes.len() + if nul_terminate_body { 1 } else { 0 };

    // Reconstruct in the canonical wire order: size, type, then any other keys.
    let original = if let Value::Object(m) = meta { m } else { Map::new() };
    let mut ordered = Map::new();
    ordered.insert("size".into(), Value::from(body_len));
    if let Some(t) = original.get("type") {
        ordered.insert("type".into(), t.clone());
    }
    for (k, v) in original.iter() {
        if k == "type" || k == "size" { continue; }
        ordered.insert(k.clone(), v.clone());
    }

    let mut announce_payload = serde_json::to_vec(&Value::Object(ordered))?;
    announce_payload.push(0); // FILE_HEADER's JSON is always NUL-terminated
    let announce = Frame::new(OP_FILE_HEADER, announce_payload);
    dev.send_frame(&announce).await?;
    dev.read_until_ack().await?;

    // Send the body in CHUNK_SIZE-byte chunks. If we owe a trailing NUL,
    // append it to the final chunk.
    let last_chunk_idx = if bytes.is_empty() { 0 } else { (bytes.len() - 1) / CHUNK_SIZE };
    for (i, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
        let mut buf: Vec<u8>;
        let payload: &[u8] = if nul_terminate_body && i == last_chunk_idx {
            buf = chunk.to_vec();
            buf.push(0);
            &buf
        } else {
            chunk
        };
        let frame = build_chunk(payload);
        dev.send_frame(&frame).await?;
        dev.read_until_ack().await?;
    }
    // If body is empty but we still owe a NUL, send it as a 1-byte chunk.
    if bytes.is_empty() && nul_terminate_body {
        let frame = build_chunk(&[0]);
        dev.send_frame(&frame).await?;
        dev.read_until_ack().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_layout_matches_capture() {
        // Capture for config.txt:
        //   length=0x39 (57), op=0x06, then JSON + NUL
        //   {"size":332,"type":"device-config","name":"config.txt"}\0
        let frame = build_announce("device-config", "config.txt", 332).unwrap();
        let bytes = frame.encode().unwrap();
        assert_eq!(bytes[0], 0x39);
        assert_eq!(bytes[1], 0x00);
        assert_eq!(bytes[2], 0x06);
        let body = &bytes[3..];
        // body should end with NUL
        assert_eq!(*body.last().unwrap(), 0);
    }

    #[test]
    fn chunk_layout_matches_capture() {
        // Capture: length=0x2006 (8198), op=0x07, size=8192, flag=0, then 8192 bytes
        let payload = vec![0xAA; 8192];
        let frame = build_chunk(&payload);
        let bytes = frame.encode().unwrap();
        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 8198);
        assert_eq!(bytes[2], 0x07);
        let chunk_size = u32::from_le_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
        assert_eq!(chunk_size, 8192);
        assert_eq!(bytes[7], 0); // flag
        assert_eq!(&bytes[8..], &payload[..]);
    }
}
