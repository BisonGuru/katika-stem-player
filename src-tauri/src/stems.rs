//! Local stem-splitting via Demucs (htdemucs model by default).
//!
//! We shell out to the `demucs` CLI rather than embedding it. This keeps
//! the Rust crate small and lets us pick up newer Demucs versions without
//! rebuilding. PyTorch + Demucs are heavy (~2 GB on disk) — we expect
//! users to install once via pip and then this module to find the binary.
//!
//! Demucs's output layout with `--mp3` flag:
//!
//!     <output_dir>/htdemucs/<track_basename>/bass.mp3
//!     <output_dir>/htdemucs/<track_basename>/drums.mp3
//!     <output_dir>/htdemucs/<track_basename>/other.mp3
//!     <output_dir>/htdemucs/<track_basename>/vocals.mp3

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::stemplayer::error::{Error, Result};

/// Standard locations to look for the `demucs` binary.
fn demucs_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // 1. Whatever is on PATH (resolves via the OS).
    out.push(PathBuf::from("demucs"));
    // 2. ~/Library/Python/3.9/bin (Apple-bundled python's pip --user)
    if let Some(home) = dirs_home() {
        for v in ["3.13", "3.12", "3.11", "3.10", "3.9"] {
            out.push(home.join(format!("Library/Python/{v}/bin/demucs")));
        }
        out.push(home.join(".local/bin/demucs"));
    }
    // 3. Common system locations
    for p in [
        "/opt/homebrew/bin/demucs",
        "/usr/local/bin/demucs",
        "/usr/bin/demucs",
    ] {
        out.push(PathBuf::from(p));
    }
    out
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Build a PATH string that gives demucs a fighting chance of finding
/// ffmpeg/ffprobe regardless of how the parent app was launched.
fn path_for_demucs() -> String {
    let mut parts: Vec<String> = vec![
        "/usr/local/bin".into(),
        "/opt/homebrew/bin".into(),
        "/usr/bin".into(),
        "/bin".into(),
        "/usr/sbin".into(),
        "/sbin".into(),
    ];
    if let Some(home) = dirs_home() {
        for v in ["3.13", "3.12", "3.11", "3.10", "3.9"] {
            parts.push(home.join(format!("Library/Python/{v}/bin")).display().to_string());
        }
        parts.push(home.join(".local/bin").display().to_string());
        parts.push(home.join(".cargo/bin").display().to_string());
    }
    if let Ok(existing) = std::env::var("PATH") {
        parts.push(existing);
    }
    parts.join(":")
}

/// Return the first `demucs` candidate that successfully runs `--version`
/// (or any quick command). `None` if Demucs isn't installed anywhere we
/// know to look.
pub async fn find_demucs() -> Option<PathBuf> {
    for cand in demucs_candidates() {
        let ok = Command::new(&cand)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(cand);
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitResult {
    pub bass: PathBuf,
    pub drums: PathBuf,
    pub other: PathBuf,
    pub vocals: PathBuf,
    pub track_name: String,
}

/// Run Demucs on `input`, writing 4 MP3 stems under `output_dir`.
///
/// Convenience wrapper around `split_stems_with_progress` with a no-op
/// progress callback. Kept for callers that don't care about progress.
pub async fn split_stems(input: &Path, output_dir: &Path) -> Result<SplitResult> {
    split_stems_with_progress(input, output_dir, |_| {}).await
}

/// Same as `split_stems`, but invokes `on_progress(pct)` (0.0–100.0) as
/// Demucs writes tqdm progress bars to stderr. Updates may arrive
/// non-monotonically (separation runs models for each stem in series and
/// each one starts at 0%) — callers should treat this as a per-stage
/// signal of liveness rather than absolute completion.
pub async fn split_stems_with_progress<F>(
    input: &Path,
    output_dir: &Path,
    mut on_progress: F,
) -> Result<SplitResult>
where
    F: FnMut(f32) + Send + 'static,
{
    let demucs = find_demucs().await.ok_or_else(|| {
        Error::Other(
            "demucs not found — install with `pip3 install --user demucs`".into(),
        )
    })?;

    tokio::fs::create_dir_all(output_dir).await?;

    let track_name = input
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Other("bad input filename".into()))?
        .to_string();

    tracing::info!("splitting stems for {} via {}", input.display(), demucs.display());

    let extra_path = path_for_demucs();

    let mut child = Command::new(&demucs)
        .env("PATH", &extra_path)
        .args([
            "--mp3",
            "--mp3-bitrate",
            "192",
            "-d",
            "mps",
            "-n",
            "htdemucs",
            "-o",
        ])
        .arg(output_dir)
        .arg(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Stream stderr so we can react to tqdm progress bars in near-real-time.
    // tqdm uses CR-overwrite (no newlines), so we read in chunks and scan
    // for the most recent NN% marker.
    let stderr = child.stderr.take().unwrap();
    let mut stderr_buffer: Vec<u8> = Vec::with_capacity(64 * 1024);
    let progress_handle = tokio::spawn(async move {
        let mut reader = stderr;
        let mut chunk = [0u8; 1024];
        let mut tail = String::new();
        let mut full = String::new();
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    let s = String::from_utf8_lossy(&chunk[..n]);
                    full.push_str(&s);
                    tail.push_str(&s);
                    if let Some(p) = parse_last_pct(&tail) {
                        on_progress(p);
                    }
                    // Cap tail size so memory doesn't grow on long runs.
                    if tail.len() > 4096 {
                        let cut = tail.len() - 1024;
                        tail = tail.split_off(cut);
                    }
                }
                Err(e) => {
                    tracing::warn!("stderr read error during demucs: {e}");
                    break;
                }
            }
        }
        full.into_bytes()
    });

    // Drain stdout so the child doesn't block on a full pipe.
    let mut stdout = child.stdout.take().unwrap();
    let stdout_handle = tokio::spawn(async move {
        let mut sink = Vec::new();
        let _ = stdout.read_to_end(&mut sink).await;
        sink
    });

    let status = child.wait().await?;
    let captured = progress_handle
        .await
        .unwrap_or_default();
    stderr_buffer.extend_from_slice(&captured);
    let _ = stdout_handle.await;

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_buffer);
        let useful: Vec<&str> = stderr
            .lines()
            .filter(|l| {
                !l.contains("UserWarning")
                    && !l.contains("warnings.warn")
                    && !l.contains("torchaudio.load_with_torchcodec")
                    && !l.trim().is_empty()
            })
            .collect();
        let summary = if useful.is_empty() {
            stderr.lines().last().unwrap_or("(no error output)").to_string()
        } else {
            useful.join(" / ")
        };
        return Err(Error::Other(format!(
            "demucs failed (exit {}): {}",
            status, summary
        )));
    }

    let track_dir = output_dir.join("htdemucs").join(&track_name);
    let result = SplitResult {
        bass: track_dir.join("bass.mp3"),
        drums: track_dir.join("drums.mp3"),
        other: track_dir.join("other.mp3"),
        vocals: track_dir.join("vocals.mp3"),
        track_name,
    };

    for p in [&result.bass, &result.drums, &result.other, &result.vocals] {
        if !p.exists() {
            return Err(Error::Other(format!(
                "demucs ran but expected stem missing: {}",
                p.display()
            )));
        }
    }

    Ok(result)
}

/// Find the most recent `NN%` (or `NN.N%`) marker in `s` and return it
/// as a 0.0–100.0 float. Tqdm progress bars print a `\r%` overwrite line,
/// so the freshest percentage is always the last one in our buffer.
fn parse_last_pct(s: &str) -> Option<f32> {
    let bytes = s.as_bytes();
    let mut last: Option<f32> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // walk back through digits and an optional '.'
            let mut j = i;
            let mut had_digit = false;
            while j > 0 {
                let c = bytes[j - 1];
                if c.is_ascii_digit() || c == b'.' {
                    j -= 1;
                    if c.is_ascii_digit() {
                        had_digit = true;
                    }
                } else {
                    break;
                }
            }
            if had_digit && j < i {
                if let Ok(text) = std::str::from_utf8(&bytes[j..i]) {
                    if let Ok(p) = text.parse::<f32>() {
                        if (0.0..=100.0).contains(&p) {
                            last = Some(p);
                        }
                    }
                }
            }
        }
        i += 1;
    }
    last
}
