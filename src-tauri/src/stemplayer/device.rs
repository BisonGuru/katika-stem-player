//! USB discovery and connection.
//!
//! Stem Player USB descriptor (verified live):
//!
//! - VID 0x1209 / PID 0x572a
//! - 1 configuration, 1 interface (vendor-specific, class 0xFF)
//! - 2 bulk endpoints: EP1 IN, EP1 OUT, both 64-byte max packet
//!
//! The whole protocol rides on EP1 — no control transfers, no isoch.

use std::time::Duration;

use nusb::transfer::RequestBuffer;
use serde::{Deserialize, Serialize};

use crate::stemplayer::error::{Error, Result};
use crate::stemplayer::frame::{Frame, OP_CONNECT, OP_DISCONNECT};

pub const STEM_PLAYER_VID: u16 = 0x1209;
pub const STEM_PLAYER_PID: u16 = 0x572a;

pub const INTERFACE_NUMBER: u8 = 0;
pub const ENDPOINT_IN: u8 = 0x81; // 0x80 | 1
pub const ENDPOINT_OUT: u8 = 0x01;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const READ_BUFFER_SIZE: usize = 16 * 1024;

/// Lightweight device descriptor we pass to the frontend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub vendor_id: u16,
    pub product_id: u16,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub serial: Option<String>,
}

pub async fn find_devices() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    for d in nusb::list_devices()? {
        if d.vendor_id() == STEM_PLAYER_VID && d.product_id() == STEM_PLAYER_PID {
            out.push(DeviceInfo {
                vendor_id: d.vendor_id(),
                product_id: d.product_id(),
                manufacturer: d.manufacturer_string().map(|s| s.to_string()),
                product: d.product_string().map(|s| s.to_string()),
                serial: d.serial_number().map(|s| s.to_string()),
            });
        }
    }
    Ok(out)
}

/// Open device + claim interface 0. Holds resources until dropped.
pub struct StemDevice {
    info: DeviceInfo,
    interface: nusb::Interface,
}

impl StemDevice {
    pub async fn open(target: &DeviceInfo) -> Result<Self> {
        let dev_info = nusb::list_devices()?
            .find(|d| {
                d.vendor_id() == target.vendor_id
                    && d.product_id() == target.product_id
                    && (target.serial.is_none()
                        || d.serial_number().map(|s| s.to_string()) == target.serial)
            })
            .ok_or(Error::NotFound)?;

        let device = dev_info.open()?;
        let interface = device.claim_interface(INTERFACE_NUMBER).map_err(|e| {
            // Most common failure: another process / browser tab has the device.
            tracing::warn!("claim_interface failed: {e}");
            Error::Busy
        })?;

        let mut me = Self {
            info: target.clone(),
            interface,
        };

        // Application-level CONNECT handshake. The device's protocol
        // ignores CONTROL and FILE commands until it has seen this.
        // (Observed in stemplayer.com captures: a single OP_CONNECT
        // frame is sent right after WebUSB open + claim_interface.)
        let connect = Frame::new(OP_CONNECT, Vec::new());
        if let Err(e) = me.send_frame(&connect).await {
            tracing::warn!("OP_CONNECT send failed: {e}");
        } else {
            // Drain whatever ACK the device sends back. We don't care if
            // it's a NAK or a slow reply — just don't leave it queued.
            let _ = tokio::time::timeout(
                Duration::from_millis(1500),
                me.read_raw(),
            )
            .await;
        }

        Ok(me)
    }

    /// Send the application-level DISCONNECT frame and drop the
    /// interface. Best-effort — if the device is already gone, swallow.
    pub async fn close_gracefully(&mut self) {
        let bye = Frame::new(OP_DISCONNECT, Vec::new());
        let _ = self.send_frame(&bye).await;
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Send any frame as-is.
    pub async fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let bytes = frame.encode()?;
        let completion = self.interface.bulk_out(ENDPOINT_OUT, bytes).await;
        completion.status?;
        Ok(())
    }

    /// Send a 0x04 sub-command (or anything else expressible as a Frame).
    pub async fn send_command<C: AsFrame>(&mut self, cmd: &C) -> Result<()> {
        self.send_frame(&cmd.as_frame()).await
    }

    /// Read one bulk packet. Returns the *payload* bytes from the frame
    /// (length and opcode stripped).
    pub async fn read_response(&mut self) -> Result<Vec<u8>> {
        let buf = self.read_raw().await?;
        let frame = Frame::decode(&buf)?;
        Ok(frame.payload)
    }

    /// Read one bulk packet and return raw bytes (frame still wrapped).
    pub async fn read_raw(&mut self) -> Result<Vec<u8>> {
        let buf = RequestBuffer::new(READ_BUFFER_SIZE);
        let request = self.interface.bulk_in(ENDPOINT_IN, buf);
        let completion = tokio::time::timeout(READ_TIMEOUT, request)
            .await
            .map_err(|_| Error::Other("USB read timeout".into()))?;
        completion.status?;
        Ok(completion.data)
    }

    /// Read until we see a known reply opcode (`0x00` or `0x01`).
    ///
    /// The device speaks (at least) two reply opcodes:
    /// - `0x00` — generic ACK. Frame is `01 00 00`.
    /// - `0x01` — "status" reply. Frame is `02 00 01 <status>`, where the
    ///   status byte's exact meaning isn't fully decoded yet. In our
    ///   firmware-update capture we saw 44 of these mixed in with the
    ///   plain ACKs, so we treat them as successful responses too. The
    ///   status byte is logged at debug level for whoever's looking.
    ///
    /// Anything else gets logged and we keep reading — useful in case the
    /// device sends an unsolicited frame between request and reply.
    pub async fn read_until_ack(&mut self) -> Result<()> {
        loop {
            let buf = self.read_raw().await?;
            let frame = Frame::decode(&buf)?;
            match frame.op {
                0x00 => return Ok(()),
                0x01 => {
                    let status = frame.payload.first().copied().unwrap_or(0);
                    tracing::debug!("device reply op=0x01 status=0x{:02x}", status);
                    return Ok(());
                }
                0x05 => {
                    // RESPONSE — the device sometimes replies with this for
                    // file pushes (especially track-config). Per wire-capture
                    // observations, the host needs to ACK before the device
                    // will progress.
                    tracing::debug!(
                        "device reply op=0x05 (RESPONSE), len={} — sending host ACK",
                        frame.payload.len()
                    );
                    let host_ack = Frame::new(0x00, Vec::new());
                    self.send_frame(&host_ack).await?;
                    return Ok(());
                }
                _ => {
                    tracing::debug!(?frame, "ignoring unexpected reply opcode");
                }
            }
        }
    }
}

/// Anything that can be converted into a wire frame.
pub trait AsFrame {
    fn as_frame(&self) -> Frame;
}

impl AsFrame for Frame {
    fn as_frame(&self) -> Frame {
        self.clone()
    }
}
