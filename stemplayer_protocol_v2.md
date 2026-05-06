# Stem Player USB Protocol — Authoritative Notes (post-capture)

Updated 2026-05-06 from a real wire capture of stemplayer.com pushing a
track to a paired Stem Player. Supersedes v1 guesses where they conflict.

## Summary of corrections vs. our earlier reverse-engineering

| Topic | Earlier guess | Confirmed wire reality |
|---|---|---|
| Album slot ID | `A1`–`A4` only | `A1`–`A4` reserved by built-ins; user uploads start at **`A5`** and increment |
| Track ID | `T1` (uppercase) | **`t1`** (lowercase) |
| `stem` field type | string `"2"` | **integer** `2` |
| `track-config` file push | required | **not used** in fresh uploads (only during legacy migration) |
| Album-config `version` field | `null` | string `"1"` |
| Album-config `artist`/`title` | actual track values | placeholder string `"OTHER"` for user uploads |
| Album-config `tracks` | `[]` or list | `null` |
| `global_id` | hex hash | UUID-format (`00000000-0000-0000-0000-NNNNNNNNNNNN`) |
| Stem upload order | bass, drums, other, vocals | **vocals (2), bass (3), drums (4), other (1)** |

## Confirmed sequence for adding a custom track

```
[USB-level]  open + claim_interface(0)
[OP_CONNECT] 02 00 02
[Auth]       VERSION (0x04 0x01) -> get sn
             POST api.stemplayer.com/accounts/device/challenge {device_id: sn}
             0x04 0x12 {"challenge": <int>}    -> device replies op=0x05 sub=0x12 {"response": "<hex>"}
             POST api.stemplayer.com/accounts/device/login {device_id, challenge_response}

[Pre-write enumeration: read state to find next available slot]
0x04 0x02 (GET_STORAGE_INFO)              x2
0x04 0x03 (GET_TRACKS_INFO)               x2-3
0x04 0x05 {"album":"<id>"} for each existing slot  (multiple)
0x04 0x06 {"album":..,"track":..} for each existing track  (multiple)

[Add the new album]
0x04 0x08 {"album":"A5"}                  ADD_ALBUM

[Push album-config — pretty-printed JSON, NO trailing NUL]
FILE_HEADER (0x06)  {"size":148, "type":"album-config", "album":"A5"}
FILE_BODY  (0x07)   148 bytes of:
    {
      "id": "A5",
      "global_id": "00000000-0000-0000-0000-000087654321",
      "artist": "OTHER",
      "title": "OTHER",
      "version": "1",
      "tracks": null
    }

[Push stems in order: vocals, bass, drums, other — one at a time]
For each (stem_id ∈ [2, 3, 4, 1], in that order):
  FILE_HEADER (0x06)  {"size":<bytes>, "type":"stem-audio-mp3",
                       "track":"t1", "album":"A5", "stem":<id>}
  FILE_BODY  (0x07)   chunked (8 KiB at a time) raw MP3 bytes — no NUL
```

## NO track-config in the fresh-upload path

Earlier we assumed the device required a `track-config` file announce
to bind metadata to a track. The actual capture shows the page goes
**straight from album-config to the four stem-audio-mp3 pushes**. The
device infers the track row from the first stem it sees with a given
`{album, track}` pair.

The `track-config` file type DOES exist — the official client emits it
during the *legacy format migration* path (when an old album/track is
auto-converted on connect). Don't emit it for new uploads.

## Bidirectional flow control

Every IN frame the device sends back must be acknowledged with an OUT
frame `01 00 00` (ACK). Our app currently doesn't send these host ACKs
— the page does. Without ACKs, the device queues replies and won't
process the next request, which manifests as an apparent hang after
the first push.

## Frame format reminder (unchanged from v1)

```
[u16 LE length][u8 opcode][payload of (length - 1) bytes]
```

- ACK: `01 00 00`
- NAK: `02 00 01 <status>` where status: 0=BUSY, 1=SYNTAX_ERROR, 2=STATE_ERROR, 3=RESOURCE_ERROR
- CONNECT: `01 00 02`
- DISCONNECT: `01 00 03`
- CONTROL: `xx xx 04 <sub_byte> [json + NUL]`
- RESPONSE: device-side reply to CONTROL — `xx xx 05 <sub_echo> [json + NUL]`
- FILE_HEADER: `xx xx 06 <json + NUL>`
- FILE_BODY: `xx xx 07 <u32 LE chunk_size><u8 flag=0><chunk bytes>`

## What the device returns from GET_TRACKS_INFO

```json
{"l":[
  {"a":"A1","c":[{"t":"T1"}]},
  {"a":"RECORD","c":[{"t":"T1"}]},
  {"a":"A3","c":[]},
  {"a":"A4","c":[]}
]}
```

- `l` = library (array of albums)
- `a` = album id (e.g., `A1`, `RECORD`)
- `c` = contents — array of `{t: <track_id>}`
- For an empty album, `c: []`
- A4/A5/A6+ slots only appear after `ADD_ALBUM` creates them

## Track-row creation: SOLVED ✓

The track row is created by a **`track-config` FILE_HEADER push that goes
LAST**, after all four stems. Body schema (extracted from kano.js
`uploadTrackConfig`):

```json
{
  "TrackColour": ["#FF6A00", "#FFFFFF"],
  "tempos": [{"time_ms": 0, "tempo_bpm": 120}],
  "TrackGain_dB": 0,
  "metadata": {
    "artist": "OTHER",
    "title": "OTHER",
    "global_id": "00000000-0000-0000-0000-XXXXXXXXXXXX",
    "meta_version": "1",
    "stems_version": "1",
    "timestamp": "2026-05-06 11:00:00"
  }
}
```

Validators (also in kano.js):

- `TrackColour` must be an array of EXACTLY two hex strings each matching `/^#[0-9A-F]{6}$/i`.
- `TrackGain_dB` must be a number.
- `tempos` must be an array; default `[{time_ms: 0, tempo_bpm: <bpm>}]`.
- `timestamp` must match `/^\d{4}-(0[1-9]|1[0-2])-([0-2]\d|3[01]) (0\d|1[01]):[0-5]\d:[0-5]\d$/`. NOTE the hour pattern only accepts **00–11** (AM only). Either a regex bug in their code or intentional. The legacy-migration emitter defaults to `"2021-08-25 00:00:00"`. Clamp host hours into 00–11 to be safe.
- `bpm` and `temposConfig` cannot both be missing.
- `meta_version`, `stems_version` are strings.

## The complete fresh-upload sequence (verified end-to-end on real hardware)

```
[USB-level]   open + claim_interface(0)
[Connect]     OP_CONNECT (frame: 01 00 02)
[Auth]        VERSION (0x04 0x01) → get sn
              POST api.stemplayer.com/accounts/device/challenge {device_id:sn}
              0x04 0x12 {"challenge":<int32>} → device replies 0x05 + {"response":"<hex>"}
              POST api.stemplayer.com/accounts/device/login {device_id, challenge_response}

[Slot setup]  0x04 0x08 {"album":"A<n>"}  ADD_ALBUM (n ≥ 5)

[Push album-config]
              FILE_HEADER (0x06)  {"size":N,"type":"album-config","album":"A<n>"}
              FILE_BODY  (0x07)   pretty JSON {id, global_id, artist, title, version, tracks}
              (NO trailing NUL on body)

[Push stems in order: vocals (id 2), bass (3), drums (4), other (1)]
For each stem:
              250ms settling delay
              FILE_HEADER (0x06)  {"size":N,"type":"stem-audio-mp3","track":"t1","album":"A<n>","stem":<id>}
              FILE_BODY  (0x07)   raw 192k MP3 bytes (no NUL)

[Commit ← THIS IS WHAT CREATES THE TRACK ROW]
              250ms settling delay
              FILE_HEADER (0x06)  {"size":N,"type":"track-config","track":"t1","album":"A<n>"}
              FILE_BODY  (0x07)   pretty JSON {TrackColour, tempos, TrackGain_dB, metadata}
              (WITH trailing NUL byte on body — matches uploadTrackConfig in kano.js)
```

## Hard-won implementation notes

- **JSON key insertion order matters.** The official client serialises
  FILE_HEADER metadata in the order `{size, type, …}`. If you serialise
  alphabetically (default `serde_json::Map` behaviour), the device
  silently ignores or stalls. Use `serde_json` with the `preserve_order`
  feature and explicitly insert keys in the order: size, type, then
  any upload-specific fields.
- **Inter-stem 250 ms delay** between stem pushes is required. Without it,
  `read_until_ack` hangs because stem N+1's announce races stem N's
  "stored" handshake.
- **Slot poisoning is real.** A user-upload slot that experienced a failed
  push attempt enters a state where future pushes to the same slot also
  silently stall. Bump to a fresh slot (A11, A12 …) when retrying — the
  poisoned slots eventually clear on a reboot but until then they're dead.
- **NAK with status 0x02 (STATE_ERROR) during chunk pushes is normal**, not
  a fatal error. The device returns this for every chunk during a long
  push; treating it as success and continuing matches the page's behaviour.
- **STATE_ERROR codes** (full table from `kano.js` v_table):
  `0=BUSY, 1=SYNTAX_ERROR, 2=STATE_ERROR, 3=RESOURCE_ERROR`.
- **Stem ID mapping** (1-indexed, NOT 0-indexed): `1=other, 2=vocals,
  3=bass, 4=drums`. The `stem` field in the FILE_HEADER must be sent as
  a JSON number, not a string.
- **Album / track ID conventions**: built-in albums are `A1`–`A4` and
  `RECORD`. User uploads start at `A5` and increment. The track id within
  a fresh album is **lowercase `t1`**; subsequent track rows the device
  registers itself appear as uppercase `T1`, `T2`, `T3`, ... in
  `GET_TRACKS_INFO` replies.
- **`track-config` is NEVER emitted at the start** for fresh uploads — it
  only goes at the very end, as the commit step. Sending it earlier
  causes the device to time out.
