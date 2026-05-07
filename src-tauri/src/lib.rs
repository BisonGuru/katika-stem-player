//! Tauri command bridge.
//!
//! The frontend calls into Rust through these commands. Each one wraps a
//! piece of `stemplayer::*` and serialises errors as plain strings (Tauri
//! requires `Serialize` for error returns; using `String` keeps things simple).

mod auth;
mod stemplayer;
mod stems;
mod cache;



use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::Mutex;

use stemplayer::{
    commands::{Cmd04, FilePushType},
    device::{find_devices, DeviceInfo, StemDevice},
    files::push_file,
};

/// JSON payload for the `upload-progress` event emitted to the frontend.
/// `pct` is 0.0–100.0; the frontend only renders a coarse bar so floats
/// are fine. `stage` and `message` give the UI a label to show.
#[derive(Debug, Clone, serde::Serialize)]
struct UploadProgress {
    stage: &'static str,
    pct: f32,
    message: String,
}

fn emit_progress(app: &tauri::AppHandle, stage: &'static str, pct: f32, message: impl Into<String>) {
    let _ = app.emit(
        "upload-progress",
        UploadProgress {
            stage,
            pct,
            message: message.into(),
        },
    );
}

/// Global handle to the currently-open device.
/// `None` means "not connected"; only one device is supported at a time
/// (the first Stem Player we see, since the protocol uses session state).
type SharedDevice = Arc<Mutex<Option<StemDevice>>>;


/// Heuristic: does this error string look like a transient USB disconnect
/// that we should try to recover from by re-opening + re-auth?
fn is_transient_disconnect(s: &str) -> bool {
    let s = s.to_lowercase();
    s.contains("device disconnected")
        || s.contains("disconnected")
        || s.contains("no such device")
        || s.contains("usb read timeout")
        || s.contains("io error")
        || s.contains("ioerror")
        || s.contains("transfer timed out")
        || s.contains("not connected")
        || s.contains("endpoint stalled")
}

/// Drop the current StemDevice, re-open via the cached descriptor, and
/// re-run the Kano cloud auth challenge. The Stem Player auto-sleeps
/// after ~15s of USB silence and drops off the bus; this restores the
/// session after the user taps the puck to wake it.
async fn reconnect(state: &tauri::State<'_, SharedDevice>) -> Result<(), String> {
    let info = {
        let guard = state.lock().await;
        guard.as_ref().ok_or_else(|| "Not connected".to_string())?.info().clone()
    };
    {
        let mut guard = state.lock().await;
        *guard = None;
    }
    let mut dev = StemDevice::open(&info).await.map_err(|e| e.to_string())?;
    let serial = info.serial.clone().unwrap_or_default();
    auth::authenticate(&mut dev, &serial).await.map_err(|e| e.to_string())?;
    {
        let mut guard = state.lock().await;
        *guard = Some(dev);
    }
    tracing::info!("reconnected after transient disconnect");
    Ok(())
}

#[tauri::command]
async fn list_devices() -> Result<Vec<DeviceInfo>, String> {
    find_devices().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn connect(state: tauri::State<'_, SharedDevice>) -> Result<DeviceInfo, String> {
    let mut guard = state.lock().await;
    if let Some(d) = guard.as_ref() {
        return Ok(d.info().clone());
    }
    let devices = find_devices().await.map_err(|e| e.to_string())?;
    let info = devices
        .into_iter()
        .next()
        .ok_or_else(|| "No Stem Player found".to_string())?;
    let mut dev = StemDevice::open(&info).await.map_err(|e| e.to_string())?;
    let info_out = dev.info().clone();

    // Run the device-auth challenge flow if we have a serial. Without
    // this, all CONTROL writes and FILE_HEADER pushes come back as
    // STATE_ERROR (op=0x01 status=0x02) — the device's "I'm not yet
    // authenticated" reply.
    if let Some(serial) = info_out.serial.as_deref() {
        match auth::authenticate(&mut dev, serial).await {
            Ok(()) => tracing::info!("device authenticated via Kano challenge"),
            Err(e) => tracing::warn!("device auth failed (continuing read-only): {e}"),
        }
    }

    *guard = Some(dev);
    Ok(info_out)
}

#[tauri::command]
async fn disconnect(state: tauri::State<'_, SharedDevice>) -> Result<(), String> {
    let mut guard = state.lock().await;
    *guard = None;
    Ok(())
}

#[tauri::command]
async fn is_connected(state: tauri::State<'_, SharedDevice>) -> Result<bool, String> {
    let guard = state.lock().await;
    Ok(guard.is_some())
}

/// Send a `0x04` sub-command. The frontend can pass any sub-command byte
/// plus an optional JSON payload object. We're permissive here on purpose —
/// the whole point of having a Rust manager is to be able to probe the
/// device's command surface freely.
#[tauri::command]
async fn send_cmd(
    state: tauri::State<'_, SharedDevice>,
    sub: u8,
    payload: Option<serde_json::Value>,
) -> Result<Vec<u8>, String> {
    let mut last_err: Option<String> = None;
    for attempt in 0..=1 {
        let res: Result<Vec<u8>, String> = async {
            let mut guard = state.lock().await;
            let dev = guard
                .as_mut()
                .ok_or_else(|| "Not connected".to_string())?;
            let cmd = if let Some(ref p) = payload {
                Cmd04::with_json(sub, p).map_err(|e| e.to_string())?
            } else {
                Cmd04::bare(sub)
            };
            dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
            let bytes = dev.read_response().await.map_err(|e| e.to_string())?;
            // Side-effect: cache firmware response when sub=0x01 (VERSION).
            if sub == 0x01 {
                let _ = cache_firmware_response(&bytes).await;
            }
            Ok(bytes)
        }.await;
        match res {
            Ok(v) => return Ok(v),
            Err(e) if attempt == 0 && is_transient_disconnect(&e) => {
                tracing::warn!("send_cmd 0x{:02x}: {} — reconnecting", sub, e);
                if let Err(reconnect_err) = reconnect(&state).await {
                    return Err(format!("reconnect failed: {reconnect_err} (after: {e})"));
                }
                continue;
            }
            Err(e) => { last_err = Some(e); break; }
        }
    }
    Err(last_err.unwrap_or_else(|| "send_cmd: unknown error".into()))
}

/// Convenience wrapper: enumerate all albums + tracks on the device.
#[tauri::command]
async fn enumerate(
    state: tauri::State<'_, SharedDevice>,
) -> Result<Vec<stemplayer::commands::AlbumInfo>, String> {
    let mut guard = state.lock().await;
    let dev = guard
        .as_mut()
        .ok_or_else(|| "Not connected".to_string())?;
    stemplayer::commands::enumerate_library(dev)
        .await
        .map_err(|e| e.to_string())
}

/// Push a file (config, track, or anything else) to the device.
/// Path is resolved on the host; bytes are streamed in 8 KiB chunks.
#[tauri::command]
async fn push_file_cmd(
    state: tauri::State<'_, SharedDevice>,
    path: String,
    file_type: String,
    name: String,
) -> Result<(), String> {
    let mut guard = state.lock().await;
    let dev = guard
        .as_mut()
        .ok_or_else(|| "Not connected".to_string())?;

    let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
    let kind = match file_type.as_str() {
        "device-config" => FilePushType::DeviceConfig,
        "dfu" => FilePushType::Dfu,
        "track" => FilePushType::Track,
        other => FilePushType::Other(other.to_string()),
    };

    push_file(dev, &kind, &name, &bytes)
        .await
        .map_err(|e| e.to_string())
}

/// Delete a track from a slot. Wraps `0x04 0x0a DELETE_TRACK`.
#[tauri::command]
async fn delete_track(
    state: tauri::State<'_, SharedDevice>,
    album: String,
    track: String,
) -> Result<(), String> {
    let mut last_err: Option<String> = None;
    for attempt in 0..=1 {
        let res: Result<(), String> = async {
            let mut guard = state.lock().await;
            let dev = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
            let cmd = stemplayer::commands::Cmd04::with_json(
                stemplayer::commands::sub::DELETE_TRACK,
                &serde_json::json!({ "album": album, "track": track }),
            )
            .map_err(|e| e.to_string())?;
            dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
            let _ = dev.read_response().await;
            Ok(())
        }.await;
        match res {
            Ok(()) => {
                tracing::info!("DELETE_TRACK {}/{} sent", album, track);
                let _ = cache::delete_by_album_track(&album, &track).await;
                return Ok(());
            }
            Err(e) if attempt == 0 && is_transient_disconnect(&e) => {
                tracing::warn!("delete_track {}/{}: {} — reconnecting", album, track, e);
                if let Err(reconnect_err) = reconnect(&state).await {
                    return Err(format!("reconnect failed: {reconnect_err} (after: {e})"));
                }
                continue;
            }
            Err(e) => { last_err = Some(e); break; }
        }
    }
    Err(last_err.unwrap_or_else(|| "delete_track: unknown error".into()))
}

/// Delete an album (and all its tracks). Wraps `0x04 0x09 DELETE_ALBUM`.
#[tauri::command]
async fn delete_album(
    state: tauri::State<'_, SharedDevice>,
    album: String,
) -> Result<(), String> {
    let mut last_err: Option<String> = None;
    for attempt in 0..=1 {
        let res: Result<(), String> = async {
            let mut guard = state.lock().await;
            let dev = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
            let cmd = stemplayer::commands::Cmd04::with_json(
                stemplayer::commands::sub::DELETE_ALBUM,
                &serde_json::json!({ "album": album }),
            )
            .map_err(|e| e.to_string())?;
            dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
            let _ = dev.read_response().await;
            Ok(())
        }.await;
        match res {
            Ok(()) => {
                tracing::info!("DELETE_ALBUM {} sent", album);
                let _ = cache::delete_by_album(&album).await;
                return Ok(());
            }
            Err(e) if attempt == 0 && is_transient_disconnect(&e) => {
                tracing::warn!("delete_album {}: {} — reconnecting", album, e);
                if let Err(reconnect_err) = reconnect(&state).await {
                    return Err(format!("reconnect failed: {reconnect_err} (after: {e})"));
                }
                continue;
            }
            Err(e) => { last_err = Some(e); break; }
        }
    }
    Err(last_err.unwrap_or_else(|| "delete_album: unknown error".into()))
}

/// Enriched album entry returned to the frontend.
///
/// `id` is the slot ID (`A1`, `A8`, `RECORD`, ...). `title` is the
/// human-readable name pulled from the album-config (`null` for slots
/// where we have no readable title — built-ins, or pre-Katika user
/// uploads that wrote the placeholder string `"OTHER"`). `tracks` is the
/// raw `[{t: "T1"}, …]` list from `GET_TRACKS_INFO` (we keep it shaped
/// the way the device returns it so existing rendering logic still works).
#[derive(Debug, Clone, serde::Serialize)]
struct LibraryAlbum {
    id: String,
    title: Option<String>,
    artist: Option<String>,
    tracks: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct Library {
    albums: Vec<LibraryAlbum>,
}

/// Read the device library and enrich each user-upload slot with the
/// title/artist stored in its album-config. Built-in slots (`A1..A4` and
/// `RECORD`) are returned with `title=None` so the frontend can apply
/// its own static labels.
///
/// This is one round trip for `GET_TRACKS_INFO` plus one extra
/// round trip per user-upload slot for `GET_ALBUM_CONFIG`. With at most
/// a couple dozen slots it's still fast enough to call on every refresh.
#[tauri::command]
async fn read_library(state: tauri::State<'_, SharedDevice>) -> Result<Library, String> {
    let mut last_err: Option<String> = None;
    for attempt in 0..=1 {
        match read_library_inner(&state).await {
            Ok(v) => return Ok(v),
            Err(e) if attempt == 0 && is_transient_disconnect(&e) => {
                tracing::warn!("read_library: {} — reconnecting", e);
                if let Err(re) = reconnect(&state).await {
                    return Err(format!("reconnect failed: {re} (after: {e})"));
                }
                continue;
            }
            Err(e) => { last_err = Some(e); break; }
        }
    }
    Err(last_err.unwrap_or_else(|| "read_library: unknown error".into()))
}

async fn read_library_inner(state: &tauri::State<'_, SharedDevice>) -> Result<Library, String> {
    let mut guard = state.lock().await;
    let dev = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;

    // 1. GET_TRACKS_INFO
    let cmd = stemplayer::commands::Cmd04::bare(stemplayer::commands::sub::GET_TRACKS_INFO);
    dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
    let resp = dev.read_response().await.map_err(|e| e.to_string())?;
    let parsed = parse_response_json(&resp)?;
    let albums = parsed
        .get("l")
        .and_then(|v| v.as_array())
        .ok_or("library payload missing `l` array")?
        .clone();

    let mut out = Vec::with_capacity(albums.len());
    for album in albums {
        let id = album
            .get("a")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let tracks = album
            .get("c")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Try to read the album-config for user-upload slots. Built-ins
        // tend not to have a meaningful title (the device hardcodes
        // album names from firmware), and `RECORD` doesn't either.
        let mut title: Option<String> = None;
        let mut artist: Option<String> = None;
        let is_user_slot = matches!(id.as_str(), "A5" | "A6" | "A7" | "A8" | "A9")
            || id
                .strip_prefix('A')
                .and_then(|n| n.parse::<u32>().ok())
                .map(|n| n >= 5)
                .unwrap_or(false);

        if is_user_slot {
            if let Ok((t, a)) = read_album_title(dev, &id).await {
                title = t;
                artist = a;
            }
        }

        out.push(LibraryAlbum {
            id,
            title,
            artist,
            tracks,
        });
    }

    Ok(Library { albums: out })
}

/// Issue `GET_ALBUM_CONFIG` for `slot` and parse the JSON body returned
/// by the device. Returns `(title, artist)` if both are readable strings
/// other than the legacy `"OTHER"` placeholder used by the official
/// Kano client.
async fn read_album_title(
    dev: &mut StemDevice,
    slot: &str,
) -> Result<(Option<String>, Option<String>), String> {
    let cmd = stemplayer::commands::Cmd04::with_json(
        stemplayer::commands::sub::GET_ALBUM_CONFIG,
        &serde_json::json!({ "album": slot }),
    )
    .map_err(|e| e.to_string())?;
    dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
    let resp = dev.read_response().await.map_err(|e| e.to_string())?;
    let parsed = parse_response_json(&resp)?;
    let pick = |v: &serde_json::Value| -> Option<String> {
        let s = v.as_str()?;
        if s.is_empty() || s == "OTHER" {
            None
        } else {
            Some(s.to_string())
        }
    };
    let title = parsed.get("title").and_then(pick);
    let artist = parsed.get("artist").and_then(pick);
    Ok((title, artist))
}

/// Strip the leading sub-command echo + trailing NUL from a CONTROL
/// response payload and parse the remainder as JSON.
fn parse_response_json(resp: &[u8]) -> Result<serde_json::Value, String> {
    let mut body: &[u8] = if resp.is_empty() { resp } else { &resp[1..] };
    while body.last() == Some(&0) {
        body = &body[..body.len() - 1];
    }
    serde_json::from_slice(body).map_err(|e| format!("parse json: {e}"))
}

/// Reboot the device. Useful for recovering from poisoned slots.
/// `0x04 0x00 REBOOT`. Drops the device handle since the USB endpoint
/// goes away on reboot.
#[tauri::command]
async fn reboot_device(
    state: tauri::State<'_, SharedDevice>,
) -> Result<(), String> {
    let mut guard = state.lock().await;
    if let Some(mut dev) = guard.take() {
        let cmd = stemplayer::commands::Cmd04::bare(stemplayer::commands::sub::REBOOT);
        let _ = dev.send_command(&cmd).await;
        // Don't try to read response — device is going away.
        tracing::info!("REBOOT command sent; device handle dropped");
    }
    Ok(())
}

/// Create a new empty album slot on the device. Picks the lowest free
/// `A5..A99` slot via `pick_fresh_album_slot`, sends `ADD_ALBUM`, and
/// returns the new slot's ID so the frontend can highlight it.
#[tauri::command]
async fn add_album(state: tauri::State<'_, SharedDevice>) -> Result<String, String> {
    let mut last_err: Option<String> = None;
    for attempt in 0..=1 {
        let res: Result<String, String> = async {
            let mut guard = state.lock().await;
            let dev = guard.as_mut().ok_or_else(|| "Not connected".to_string())?;
            let slot = pick_fresh_album_slot(dev).await?;
            let cmd = stemplayer::commands::Cmd04::with_json(
                stemplayer::commands::sub::ADD_ALBUM,
                &serde_json::json!({ "album": slot }),
            )
            .map_err(|e| e.to_string())?;
            dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
            let _ = dev.read_response().await;
            tracing::info!("ADD_ALBUM {} (manual) sent", slot);
            Ok(slot)
        }.await;
        match res {
            Ok(v) => return Ok(v),
            Err(e) if attempt == 0 && is_transient_disconnect(&e) => {
                tracing::warn!("add_album: {} — reconnecting", e);
                if let Err(reconnect_err) = reconnect(&state).await {
                    return Err(format!("reconnect failed: {reconnect_err} (after: {e})"));
                }
                continue;
            }
            Err(e) => { last_err = Some(e); break; }
        }
    }
    Err(last_err.unwrap_or_else(|| "add_album: unknown error".into()))
}

/// Whether Demucs is installed and runnable.
#[tauri::command]
async fn check_demucs() -> Result<Option<String>, String> {
    Ok(stems::find_demucs()
        .await
        .map(|p| p.display().to_string()))
}

/// Stem-split an input audio file with Demucs and push all four output
/// MP3s to the device as type=track.
///
/// We release the device handle before running Demucs (which can take
/// 30s+) so the Stem Player's auto-sleep doesn't matter, then re-open
/// it with a retry window before pushing — that lets the user tap the
/// puck to wake it back up if needed.
#[tauri::command]
async fn split_and_push(
    app: tauri::AppHandle,
    state: tauri::State<'_, SharedDevice>,
    path: String,
) -> Result<stems::SplitResult, String> {
    use std::path::Path;
    use std::time::{Duration, Instant};

    emit_progress(&app, "preparing", 1.0, "Preparing…");

    // 1. Capture the current device descriptor and release the handle.
    let target_info = {
        let mut guard = state.lock().await;
        let info = guard
            .as_ref()
            .ok_or_else(|| "Not connected".to_string())?
            .info()
            .clone();
        *guard = None; // drop the Interface so the device is free to sleep
        info
    };

    // 2. Run Demucs. Output goes to /tmp/stemplayer-mgr-<unix-ts>/.
    let mut out_dir = std::env::temp_dir();
    out_dir.push(format!(
        "stemplayer-mgr-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));

    emit_progress(&app, "splitting", 5.0, "Splitting song into stems…");
    let app_for_split = app.clone();
    let result = stems::split_stems_with_progress(
        Path::new(&path),
        &out_dir,
        move |demucs_pct| {
            // Demucs goes 0→100 multiple times (once per model bag / stem),
            // but always monotonically within a run. Map its current % into
            // the overall 5–60% slice. We don't try to dedupe re-runs here;
            // Demucs's bag-of-1 default model just runs once.
            let overall = 5.0 + (demucs_pct / 100.0) * 55.0;
            emit_progress(
                &app_for_split,
                "splitting",
                overall,
                format!("Splitting stems · {:.0}%", demucs_pct),
            );
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    emit_progress(&app, "splitting", 60.0, "Stems ready");

    // 3. Reopen the device. If it auto-slept during the split it'll be
    //    off the USB bus until the user taps it; we poll for up to 15s.
    tracing::info!("re-opening device after split (tap the puck if it slept)");
    emit_progress(&app, "reopening", 62.0, "Reconnecting to device…");
    let max_wait = Duration::from_secs(15);
    let started = Instant::now();
    let dev = loop {
        match StemDevice::open(&target_info).await {
            Ok(d) => break d,
            Err(e) if started.elapsed() < max_wait => {
                tokio::time::sleep(Duration::from_millis(750)).await;
                tracing::debug!("waiting for device to come back online: {e}");
            }
            Err(e) => {
                return Err(format!(
                    "device didn't come back online after split. Tap any button on \
                     the Stem Player and click Connect, then run split-and-push again. \
                     (stems are saved at {} so you don't have to re-split.) Underlying: {e}",
                    out_dir.display()
                ));
            }
        }
    };
    {
        let mut guard = state.lock().await;
        *guard = Some(dev);
    }

    // 4. Push the album-config + track-config + 4 stem-audio-mp3 bundle.
    //    This is the documented Stem Player track-upload protocol (see
    //    krystalgamer/stem-player-emulator for the wire details).
    let mut guard = state.lock().await;
    let dev = guard
        .as_mut()
        .ok_or_else(|| "device vanished after reopen".to_string())?;

    // Stem index convention from the emulator: 0=bass, 1=drums, 2=other, 3=vocals
    let bass_bytes = tokio::fs::read(&result.bass).await.map_err(|e| e.to_string())?;
    let drums_bytes = tokio::fs::read(&result.drums).await.map_err(|e| e.to_string())?;
    let other_bytes = tokio::fs::read(&result.other).await.map_err(|e| e.to_string())?;
    let vocals_bytes = tokio::fs::read(&result.vocals).await.map_err(|e| e.to_string())?;

    // The device's library has built-in slots A1..A4 and RECORD. User
    // uploads start at A5 and increment. Wire capture of stemplayer.com
    // confirms it picks the next available slot. Track id is "t1"
    // (lowercase) for a one-track album.
    // TODO: read the library and pick the first non-existing An slot
    //       (currently we hardcode A5; subsequent uploads collide).
    // Pick the first non-existing user-upload slot (A5..A99) by reading
    // the current library. Slots that already exist (even if their `c`
    // is empty) might be poisoned from a previous failed push, so we
    // skip past them entirely.
    let album_id = pick_fresh_album_slot(dev).await.unwrap_or_else(|e| {
        tracing::warn!("slot picker failed ({e}); falling back to A10");
        "A10".to_string()
    });
    let album_id_owned = album_id;
    let album_id: &str = &album_id_owned;
    let track_id = "t1";
    tracing::info!("using fresh album slot {album_id}");

    emit_progress(
        &app,
        "creating-slot",
        66.0,
        format!("Creating album slot {album_id}…"),
    );

    // ADD_ALBUM — creates the slot.
    {
        let cmd = stemplayer::commands::Cmd04::with_json(
            stemplayer::commands::sub::ADD_ALBUM,
            &serde_json::json!({ "album": album_id }),
        )
        .map_err(|e| e.to_string())?;
        dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
        let _ = dev.read_response().await;
        tracing::info!("ADD_ALBUM {} sent", album_id);
    }

    // global_id is UUID-format. Per the official client, user uploads
    // use the form 00000000-0000-0000-0000-NNNNNNNNNNNN where the last
    // 12 hex chars are derived from the file. We hash the title for
    // stability across re-uploads.
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in result.track_name.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let suffix = format!("{:012x}", hash & 0xffffffffffffu64);
    let global_id = format!("00000000-0000-0000-0000-{}", suffix);

    // Timestamp in the format the migration code uses: "YYYY-MM-DD HH:MM:SS"
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let timestamp = format_unix_timestamp(now_secs);

    // Use the source file's basename as the album/track title. The
    // official Kano client hardcodes "OTHER" here because the device
    // itself has no screen, but the device firmware accepts and stores
    // any string we hand it — so we put the actual song name in so a
    // companion app like ours can read it back via GET_ALBUM_CONFIG.
    let title = std::path::Path::new(&path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "Untitled".to_string());
    let bundle = stemplayer::files::TrackBundle {
        album_id,
        album_title: &title,
        track_id,
        track_title: &title,
        global_id: &global_id,
        artist: "Katika upload",
        timestamp: &timestamp,
        other: &other_bytes,
        vocals: &vocals_bytes,
        bass: &bass_bytes,
        drums: &drums_bytes,
    };

    // Map each push_track_bundle stage label into an overall % for the UI.
    // Album-config + 4 stems + track-config + done = 7 stages; we fan
    // them out across 70..100% of the bar.
    let app_for_push = app.clone();
    let cache_global_id = global_id.clone();
    let cache_title = title.clone();
    let cache_album = album_id.to_string();
    let cache_track = track_id.to_string();
    let cache_timestamp = timestamp.clone();
    let cache_vocals = result.vocals.clone();
    let cache_bass = result.bass.clone();
    let cache_drums = result.drums.clone();
    let cache_other = result.other.clone();
    let cache_original = std::path::PathBuf::from(&path);

    stemplayer::files::push_track_bundle_with_progress(dev, &bundle, move |stage| {
        let (pct, msg) = match stage {
            "album-config" => (70.0, "Uploading album info…"),
            "vocals"       => (76.0, "Uploading vocals…"),
            "bass"         => (82.0, "Uploading bass…"),
            "drums"        => (88.0, "Uploading drums…"),
            "other"        => (94.0, "Uploading other…"),
            "track-config" => (98.0, "Finalizing on device…"),
            "done"         => (100.0, "Done"),
            _              => (70.0, "Uploading…"),
        };
        emit_progress(&app_for_push, "pushing", pct, msg);
    })
    .await
    .map_err(|e| e.to_string())?;

    // Persist a local mirror of this upload so the in-app player can serve it.
    let _ = cache::cache_track(
        &cache_global_id,
        &cache_title,
        "Katika upload",
        &cache_album,
        &cache_track,
        &cache_timestamp,
        &cache::CachedStemPaths {
            vocals: &cache_vocals,
            bass: &cache_bass,
            drums: &cache_drums,
            other: &cache_other,
        },
        Some(&cache_original),
    ).await;

    emit_progress(&app, "done", 100.0, "Done");
    Ok(result)
}

/// Format a Unix timestamp as "YYYY-MM-DD HH:MM:SS" (UTC, no chrono dep).
///
/// IMPORTANT: the device's timestamp validator regex (extracted from
/// kano.js — `s.If`) only accepts hours 00-11. We clamp the hour into
/// that range so the upload doesn't fail validation for late-day uploads.
fn format_unix_timestamp(secs: u64) -> String {
    // Days since 1970-01-01
    let days = secs / 86_400;
    let s = (secs % 86_400) as u32;
    let mut h = s / 3600;
    if h > 11 { h %= 12; } // satisfy device validator's AM-only regex
    let m = (s % 3600) / 60;
    let s = s % 60;

    // Year/month/day
    let mut year = 1970i32;
    let mut days_left = days as i64;
    loop {
        let leap = is_leap(year);
        let yd = if leap { 366 } else { 365 };
        if days_left < yd {
            break;
        }
        days_left -= yd;
        year += 1;
    }
    let mdays = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 0usize;
    while month < 12 && days_left >= mdays[month] as i64 {
        days_left -= mdays[month] as i64;
        month += 1;
    }
    let day = (days_left + 1) as u32;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        year,
        month + 1,
        day,
        h,
        m,
        s
    )
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Read the device library and return the first user-upload album slot
/// (A5, A6, A7, ...) that does NOT exist yet. We use a non-existing slot
/// rather than an existing-empty one because poisoned slots from earlier
/// failed pushes cause the device to silently reject new uploads.
async fn pick_fresh_album_slot(dev: &mut StemDevice) -> Result<String, String> {
    let cmd = stemplayer::commands::Cmd04::bare(stemplayer::commands::sub::GET_TRACKS_INFO);
    dev.send_command(&cmd).await.map_err(|e| e.to_string())?;
    let resp = dev.read_response().await.map_err(|e| e.to_string())?;
    // Strip leading sub-command echo + trailing NUL, then JSON-parse.
    let mut body: &[u8] = if resp.is_empty() { &resp } else { &resp[1..] };
    while body.last() == Some(&0) {
        body = &body[..body.len() - 1];
    }
    let parsed: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| format!("library JSON parse: {e}"))?;
    let albums = parsed
        .get("l")
        .and_then(|v| v.as_array())
        .ok_or("missing l array")?;
    let mut existing = std::collections::HashSet::new();
    for a in albums {
        if let Some(id) = a.get("a").and_then(|v| v.as_str()) {
            existing.insert(id.to_string());
        }
    }
    for n in 5..=99 {
        let candidate = format!("A{n}");
        if !existing.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err("no free album slot in A5..A99".into())
}


/// List every locally-cached track (one entry per `~/Library/Application Support/Katika/library/<global_id>/`).
#[tauri::command]
async fn list_cached_tracks() -> Result<Vec<cache::CachedTrack>, String> {
    cache::list_tracks().await.map_err(|e| e.to_string())
}

/// Resolve a single cached track by global_id.
#[tauri::command]
async fn get_cached_track(global_id: String) -> Result<Option<cache::CachedTrack>, String> {
    cache::get_track(&global_id).await.map_err(|e| e.to_string())
}


/// Persist the most recent firmware-version response (raw 0x04 0x01 reply
/// from the device) so the frontend can show it even when disconnected.
async fn cache_firmware_response(bytes: &[u8]) -> std::io::Result<()> {
    if bytes.is_empty() { return Ok(()); }
    let mut body = &bytes[1..]; // strip echoed sub-byte
    while body.last() == Some(&0) { body = &body[..body.len() - 1]; }
    let parsed: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(String::from_utf8_lossy(body).into_owned()),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bundle = serde_json::json!({ "firmware": parsed, "captured_at": now });

    let dir = cache::cache_root().parent().map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(dir.join("firmware.json"), serde_json::to_vec_pretty(&bundle).unwrap()).await
}

#[derive(Debug, Clone, serde::Serialize)]
struct CachedFirmware {
    firmware: serde_json::Value,
    captured_at: u64,
}

/// Read back the most recently cached firmware-version response. Returns
/// None if we've never queried VERSION before on this machine.
#[tauri::command]
async fn read_cached_firmware() -> Result<Option<CachedFirmware>, String> {
    let dir = cache::cache_root().parent().map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    let path = dir.join("firmware.json");
    if !path.exists() { return Ok(None); }
    let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let firmware = parsed.get("firmware").cloned().unwrap_or(serde_json::Value::Null);
    let captured_at = parsed.get("captured_at").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok(Some(CachedFirmware { firmware, captured_at }))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "stemplayer_mgr_lib=debug,nusb=info".into()),
        )
        .init();

    let shared: SharedDevice = Arc::new(Mutex::new(None));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(shared)
        .invoke_handler(tauri::generate_handler![
            list_devices,
            connect,
            disconnect,
            is_connected,
            send_cmd,
            enumerate,
            push_file_cmd,
            check_demucs,
            split_and_push,
            delete_track,
            delete_album,
            reboot_device,
            add_album,
            read_library,
            list_cached_tracks,
            get_cached_track,
            read_cached_firmware,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
