//! Device authentication via Kano's challenge/login API.
//!
//! The Stem Player gates content writes (config, track, stem-audio) behind
//! a challenge handshake. STATE_ERROR responses (`op=0x01 status=0x02`)
//! to file-push frames are the device's way of saying "I'm not yet
//! authenticated to this host". The handshake:
//!
//! 1. `POST https://api.stemplayer.com/accounts/device/challenge`
//!    with `{ "device_id": "<serial>" }` → returns `{ data: { challenge: "<hex>" } }`
//! 2. Send `0x04 0x12 {"challenge": <int>}` to the device. The page converts
//!    the challenge to a signed 32-bit int before sending (uses two's
//!    complement if MSB is set).
//! 3. Device replies `op=0x05 [0x12, ...JSON, NUL]` where JSON is
//!    `{ "response": <int> }`.
//! 4. `POST /accounts/device/login` with `{ device_id, challenge_response }`
//!    → returns auth token on success, 401 otherwise. The Mac app doesn't
//!    actually need the returned token (writes work after step 3 puts the
//!    device in unlocked state) but we POST anyway to keep parity with
//!    the official client.
//!
//! Reverse-engineered from `npm.kano.*.js` in the stem1.stemplayer.com bundle.

use serde::Deserialize;
use serde_json::json;

use crate::stemplayer::commands::{sub, Cmd04};
use crate::stemplayer::device::StemDevice;
use crate::stemplayer::error::{Error, Result};
use crate::stemplayer::frame::{Frame, OP_FILE_HEADER, OP_RESPONSE};

const KANO_API: &str = "https://api.stemplayer.com";

#[derive(Debug, Deserialize)]
struct ChallengeResp {
    data: ChallengeBody,
}
#[derive(Debug, Deserialize)]
struct ChallengeBody {
    challenge: String,
}

/// Top-level: do the full challenge handshake. Returns Ok(()) when the
/// device has been put into the unlocked write-accepting state.
///
/// `usb_serial` is the USB descriptor's serial — but Kano's API expects
/// the device-reported serial from `VERSION` (sub-command 0x01), which is
/// a longer hex string. We query VERSION first and use that.
pub async fn authenticate(dev: &mut StemDevice, _usb_serial: &str) -> Result<()> {
    let device_serial = query_device_serial(dev).await?;
    tracing::info!("authenticate: device-reported serial = {device_serial}");

    let challenge_int = request_challenge(&device_serial).await?;
    tracing::debug!("authenticate: challenge integer = {challenge_int}");
    let device_response = send_challenge_to_device(dev, challenge_int).await?;
    tracing::debug!("authenticate: device response = {device_response}");
    complete_challenge(&device_serial, &device_response).await?;
    tracing::info!("authenticate: device unlocked");
    Ok(())
}

/// Send `0x04 0x01` (VERSION) and return the `sn` field from the JSON
/// the device replies with. Shape per the emulator:
///   `{"appver":"1.0.1747","btver":"1.24.1405","blver":"0.1.1311","sn":"<24chars>"}`
async fn query_device_serial(dev: &mut StemDevice) -> Result<String> {
    let cmd = Cmd04::bare(sub::VERSION);
    dev.send_command(&cmd).await?;
    for _ in 0..4 {
        let buf = dev.read_raw().await?;
        let frame = Frame::decode(&buf)?;
        if frame.op == OP_RESPONSE && frame.payload.first().copied() == Some(sub::VERSION) {
            let mut body = &frame.payload[1..];
            while body.last() == Some(&0) {
                body = &body[..body.len() - 1];
            }
            let parsed: serde_json::Value = serde_json::from_slice(body)
                .map_err(|e| Error::Other(format!("VERSION response not JSON: {e}")))?;
            let sn = parsed
                .get("sn")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Other(format!("VERSION missing .sn: {parsed}")))?
                .to_string();
            return Ok(sn);
        }
        tracing::debug!(?frame, "VERSION: ignoring non-response frame");
    }
    Err(Error::Other("device never returned VERSION info".into()))
}

/// Fetch a fresh challenge from Kano's API. Returns the value as a signed
/// i32 (matching how the page transforms it before sending to the device).
pub async fn request_challenge(serial: &str) -> Result<i32> {
    let client = reqwest::Client::new();
    let resp: ChallengeResp = client
        .post(format!("{KANO_API}/accounts/device/challenge"))
        .json(&json!({ "device_id": serial }))
        .send()
        .await
        .map_err(|e| Error::Other(format!("challenge http: {e}")))?
        .error_for_status()
        .map_err(|e| Error::Other(format!("challenge http status: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Other(format!("challenge json: {e}")))?;
    let raw = u32::from_str_radix(&resp.data.challenge, 16)
        .map_err(|e| Error::Other(format!("challenge hex parse: {e}")))?;
    // Mirror the page's signed-conversion shenanigans:
    //   e = (0x80000000 & e) > 0 ? (e - 0x100000000) & 0xffffffff : e & 0xffffffff;
    // In Rust this is just the i32 reinterpret of the u32.
    Ok(raw as i32)
}

/// Send `0x04 0x12 {"challenge": <int>}` to the device, read its `op=0x05
/// RESPONSE` frame, parse `{ "response": <hex_string> }` from the JSON payload.
///
/// Empirically the device returns a 256-bit (64 hex char) value — looks
/// like an HMAC or signed digest computed with a device-burnt key.
pub async fn send_challenge_to_device(
    dev: &mut StemDevice,
    challenge: i32,
) -> Result<String> {
    let cmd = Cmd04::with_json(sub::CHALLENGE, &json!({ "challenge": challenge }))?;
    dev.send_command(&cmd).await?;

    // Read until we see a RESPONSE frame (it might come after one or more NAKs).
    for _ in 0..4 {
        let buf = dev.read_raw().await?;
        let frame = Frame::decode(&buf)?;
        if frame.op == OP_RESPONSE && frame.payload.first().copied() == Some(sub::CHALLENGE) {
            // Strip leading sub-byte and trailing NUL, then JSON-parse.
            let mut body = &frame.payload[1..];
            while body.last() == Some(&0) {
                body = &body[..body.len() - 1];
            }
            let parsed: serde_json::Value = serde_json::from_slice(body)
                .map_err(|e| Error::Other(format!("challenge response not JSON: {e}")))?;
            let resp = parsed
                .get("response")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    Error::Other(format!(
                        "challenge response missing .response string: {}",
                        parsed
                    ))
                })?
                .to_string();
            return Ok(resp);
        }
        tracing::debug!(?frame, "non-RESPONSE frame, retrying");
    }
    Err(Error::Other(
        "device never returned a CHALLENGE response".into(),
    ))
}

/// POST the device's challenge response back to Kano. We don't actually
/// need the returned auth token for direct device writes — the writes
/// unlock as soon as the device sees a valid challenge round-trip — but
/// we ping the endpoint for parity with the official client and to surface
/// any "your device isn't recognised" errors early.
pub async fn complete_challenge(serial: &str, response: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{KANO_API}/accounts/device/login"))
        .json(&json!({
            "device_id": serial,
            "challenge_response": response,
        }))
        .send()
        .await
        .map_err(|e| Error::Other(format!("login http: {e}")))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        tracing::debug!("complete_challenge: 2xx body={}", body);
        Ok(())
    } else {
        // Don't hard-fail — the device-side handshake is what actually
        // unlocks writes. If Kano disagrees we'll find out when writes
        // still NAK, but no point gating local behaviour on the cloud.
        tracing::warn!(
            "complete_challenge: kano returned {} body={}",
            status,
            body
        );
        Ok(())
    }
}

// We don't actually use OP_FILE_HEADER here, but importing the constant
// is convenient for future code that might want to inspect frame opcodes.
#[allow(dead_code)]
const _UNUSED: u8 = OP_FILE_HEADER;
