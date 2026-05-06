# Katika Stem Player

A standalone desktop manager for the Yeezy / Kano Stem Player.

Talks directly to the device over USB — no dependency on stemplayer.com,
stem.fm, or any Kano cloud service. Browse the on-device library, add
tracks (with local stem-splitting via Demucs), delete albums, reboot,
and more.

## Features

- **Live library view** — connect over USB and see exactly what's on the device, including built-in albums, the on-device recording slot, and your custom uploads.
- **Add a song** — drop any MP3 / WAV / FLAC / M4A / AAC / OGG, Katika splits it into 4 stems locally with [Demucs](https://github.com/facebookresearch/demucs) (htdemucs model, MPS-accelerated on Apple Silicon) and uploads them to the device.
- **Real titles** — uploaded albums and tracks are stored with their actual filename, not the `OTHER` placeholder the official app uses.
- **One-click delete** — remove individual tracks or whole album slots from the device.
- **New album** — create an empty user-upload slot manually.
- **Reboot** — recover from poisoned slots after a failed upload.
- **Live progress** — Demucs split progress is parsed off stderr in real time, then a per-stage bar walks through the device-side push.

## Requirements

- macOS (Apple Silicon recommended; Intel works for everything except MPS-accelerated Demucs).
- A Yeezy / Kano Stem Player (USB VID `0x1209` / PID `0x572a`).
- [Demucs](https://github.com/facebookresearch/demucs) installed locally.
  ```bash
  pip3 install --user demucs
  ```
- ffmpeg + ffprobe somewhere on `PATH` (Demucs needs them).
  ```bash
  brew install ffmpeg
  ```

## Install (binary)

Download the latest `.dmg` from the [Releases page](../../releases) (or
from [katikaws.com/labs](https://katikaws.com/labs)).

The build is **not** notarized yet, so on first launch macOS Gatekeeper
will block it. Right-click → **Open** to run anyway, or in Terminal:

```bash
xattr -dr com.apple.quarantine /Applications/Katika\ Stem\ Player.app
```

## Build from source

```bash
# 1. Install Rust + Tauri
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cargo install tauri-cli --version "^2.0.0"

# 2. Clone and run
git clone https://github.com/<your-username>/katika-stem-player.git
cd katika-stem-player
cargo tauri dev          # dev build with hot reload
cargo tauri build        # release build → src-tauri/target/release/bundle/dmg/
```

## How it works

The Yeezy / Kano Stem Player exposes a single vendor-specific USB
interface with two bulk endpoints (EP1 IN / EP1 OUT). All commands ride
that interface in a simple framing:

```
[u16 LE length][u8 opcode][payload of (length - 1) bytes]
```

The protocol was reverse-engineered end-to-end by capturing the wire
traffic of `stemplayer.com` doing a track upload. See
[`stemplayer_protocol_v2.md`](stemplayer_protocol_v2.md) for the full
notes — opcode table, the cloud-mediated authentication challenge, the
exact track-upload sequence (`ADD_ALBUM` → `album-config` → 4 stems in
the order vocals / bass / drums / other → `track-config` as a commit),
and a list of the gotchas (JSON key insertion order, inter-stem 250 ms
delays, slot poisoning, etc.).

The Rust backend (`src-tauri/`) handles USB, framing, file pushes,
authentication, and the Demucs subprocess. The frontend
(`src/index.html`, `src/main.js`, `src/styles.css`) is plain HTML / JS /
CSS served directly from disk — no bundler. Tauri 2 wires the two
together with `withGlobalTauri: true` so the frontend reaches Rust via
`window.__TAURI__.core.invoke`.

## Status

This is a labs project. It works end-to-end against a real device on
macOS but only that one platform has been tested. Linux and Windows
should mostly Just Work via `nusb`, but neither has been wired up yet.

## License

[MIT](LICENSE) · 2026 Jason Coles

## Acknowledgements

- The unofficial protocol notes assembled around the
  [krystalgamer/stem-player-emulator](https://github.com/krystalgamer/stem-player-emulator)
  project were invaluable for cross-checking opcodes during the
  reverse-engineering pass.
- [`nusb`](https://github.com/kevinmehall/nusb) for clean async USB on
  macOS without libusb.
- [Demucs](https://github.com/facebookresearch/demucs) for the
  state-of-the-art stem splitting that makes "drop a song, get four
  stems" feel like magic.
