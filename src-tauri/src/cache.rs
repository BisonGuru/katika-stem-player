//! Local cache of pushed tracks so the in-app player can play them back.
//!
//! Each successful upload through `split_and_push` ends up here under
//! `~/Library/Application Support/Katika/library/<global_id>/`:
//!
//! ```text
//! library/
//!   00000000-0000-0000-0000-abcdef123456/
//!     track.json              <- {global_id, title, artist, album_id, track_id, stamp}
//!     vocals.mp3
//!     bass.mp3
//!     drums.mp3
//!     other.mp3
//!     original.mp3            <- the source file the user dropped
//! ```
//!
//! `list_cached_tracks` enumerates every directory under `library/`,
//! reads its sidecar, and returns the lot. The frontend matches entries
//! against the live device library by `(album_id, track_id)` and shows
//! a play button only for matched rows.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTrack {
    pub global_id: String,
    pub title: String,
    pub artist: String,
    pub album_id: String,
    pub track_id: String,
    pub timestamp: String,
    pub vocals: String,
    pub bass: String,
    pub drums: String,
    pub other: String,
    pub original: Option<String>,
}

/// Where Katika persists its local-side mirror of pushed tracks.
/// Picked to match the macOS convention for app-local data.
pub fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/Katika/library")
}

/// Save a fresh upload's stems + sidecar into the cache. Idempotent — if
/// the same `global_id` already exists we overwrite (re-uploads are the
/// only realistic source of collision).
pub async fn cache_track(
    global_id: &str,
    title: &str,
    artist: &str,
    album_id: &str,
    track_id: &str,
    timestamp: &str,
    stems: &CachedStemPaths<'_>,
    original: Option<&Path>,
) -> std::io::Result<()> {
    let dir = cache_root().join(global_id);
    tokio::fs::create_dir_all(&dir).await?;

    // Copy the four stem MP3s with canonical names.
    tokio::fs::copy(stems.vocals, dir.join("vocals.mp3")).await?;
    tokio::fs::copy(stems.bass, dir.join("bass.mp3")).await?;
    tokio::fs::copy(stems.drums, dir.join("drums.mp3")).await?;
    tokio::fs::copy(stems.other, dir.join("other.mp3")).await?;

    // The original input file is nice to have for a "play unmixed" button later.
    // It might already share an extension; we always store as original.<ext>.
    if let Some(orig) = original {
        let ext = orig
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("mp3");
        let dst = dir.join(format!("original.{ext}"));
        // Best-effort — failure to copy the original isn't fatal for playback.
        let _ = tokio::fs::copy(orig, &dst).await;
    }

    let sidecar = serde_json::json!({
        "global_id": global_id,
        "title": title,
        "artist": artist,
        "album_id": album_id,
        "track_id": track_id,
        "timestamp": timestamp,
    });
    tokio::fs::write(dir.join("track.json"), serde_json::to_vec_pretty(&sidecar).unwrap()).await?;
    tracing::info!("cached track {global_id} -> {}", dir.display());
    Ok(())
}

pub struct CachedStemPaths<'a> {
    pub vocals: &'a Path,
    pub bass: &'a Path,
    pub drums: &'a Path,
    pub other: &'a Path,
}

/// Enumerate every cached track. Skips any directory whose sidecar fails
/// to parse (pre-Katika layouts, partial uploads, etc.).
pub async fn list_tracks() -> std::io::Result<Vec<CachedTrack>> {
    let root = cache_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut rd = tokio::fs::read_dir(&root).await?;
    while let Some(entry) = rd.next_entry().await? {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let sidecar = p.join("track.json");
        let bytes = match tokio::fs::read(&sidecar).await {
            Ok(b) => b,
            Err(_) => continue,
        };
        let parsed: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let s = |k: &str| -> String {
            parsed
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let original = if p.join("original.mp3").exists() {
            Some(p.join("original.mp3").display().to_string())
        } else {
            // Try to find any original.* file
            let mut candidate: Option<String> = None;
            if let Ok(mut sub) = tokio::fs::read_dir(&p).await {
                while let Ok(Some(f)) = sub.next_entry().await {
                    let fp = f.path();
                    if let Some(name) = fp.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("original.") {
                            candidate = Some(fp.display().to_string());
                            break;
                        }
                    }
                }
            }
            candidate
        };
        out.push(CachedTrack {
            global_id: s("global_id"),
            title: s("title"),
            artist: s("artist"),
            album_id: s("album_id"),
            track_id: s("track_id"),
            timestamp: s("timestamp"),
            vocals: p.join("vocals.mp3").display().to_string(),
            bass: p.join("bass.mp3").display().to_string(),
            drums: p.join("drums.mp3").display().to_string(),
            other: p.join("other.mp3").display().to_string(),
            original,
        });
    }
    Ok(out)
}

/// Look up one cached track by global_id. Returns None if missing.
pub async fn get_track(global_id: &str) -> std::io::Result<Option<CachedTrack>> {
    let all = list_tracks().await?;
    Ok(all.into_iter().find(|t| t.global_id == global_id))
}

/// Remove every cache directory whose sidecar's `(album_id, track_id)`
/// matches the arguments. Returns the count removed.
pub async fn delete_by_album_track(album_id: &str, track_id: &str) -> std::io::Result<usize> {
    let needle_album = album_id.to_string();
    let needle_track = track_id.to_lowercase();
    let mut removed = 0usize;
    for t in list_tracks().await? {
        if t.album_id == needle_album && t.track_id.to_lowercase() == needle_track {
            let dir = cache_root().join(&t.global_id);
            tokio::fs::remove_dir_all(&dir).await.ok();
            tracing::info!("cache: removed {} ({} / {})", dir.display(), needle_album, needle_track);
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove every cache directory whose sidecar's `album_id` matches.
/// Returns the count removed.
pub async fn delete_by_album(album_id: &str) -> std::io::Result<usize> {
    let needle = album_id.to_string();
    let mut removed = 0usize;
    for t in list_tracks().await? {
        if t.album_id == needle {
            let dir = cache_root().join(&t.global_id);
            tokio::fs::remove_dir_all(&dir).await.ok();
            tracing::info!("cache: removed {} (album {})", dir.display(), needle);
            removed += 1;
        }
    }
    Ok(removed)
}
