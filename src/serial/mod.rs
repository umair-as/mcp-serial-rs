//! Serial transport layer: session lifecycle, port I/O, and pattern parsing.
//!
//! See CLAUDE.md §3 for the submodule split (do not flatten) and §5 for the
//! session state machine.
//!
//! The [`SerialBackend`] trait lets us substitute a mock port implementation
//! during testing — real serial ports cannot be opened in CI.

pub mod journal;
pub mod manager;
pub mod parser;
pub mod session;

use serde::Serialize;
use tokio_serial::{SerialPortInfo, SerialPortType};

use crate::config::{self, DeviceProfile};
use crate::errors::SerialError;

/// JSON-serializable description of one available serial port — the shape
/// `serial.list_ports` returns. `vid` / `pid` / `serial` are `Some` only
/// when the underlying transport reports a USB descriptor. `device` /
/// `description` are populated when a [`DeviceProfile`] matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortDescriptor {
    pub port: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial: Option<String>,
    pub device: Option<String>,
    pub description: Option<String>,
}

impl From<SerialPortInfo> for PortDescriptor {
    fn from(info: SerialPortInfo) -> Self {
        let (vid, pid, serial) = match info.port_type {
            SerialPortType::UsbPort(usb) => (Some(usb.vid), Some(usb.pid), usb.serial_number),
            _ => (None, None, None),
        };
        Self {
            port: info.port_name,
            vid,
            pid,
            serial,
            device: None,
            description: None,
        }
    }
}

/// Profile-to-port matching predicate. A profile matches when the port's USB
/// serial string equals `match_serial` and any `match_vid` / `match_pid`
/// filters in the profile also agree. Profiles without `vid` / `pid` filters
/// match purely on the serial string.
pub fn profile_matches_port(profile: &DeviceProfile, port: &PortDescriptor) -> bool {
    port.serial.as_deref() == Some(profile.match_serial.as_str())
        && profile.match_vid.is_none_or(|v| Some(v) == port.vid)
        && profile.match_pid.is_none_or(|v| Some(v) == port.pid)
}

/// Annotate each port with the first matching profile's `name` and
/// `description`. Profile order is the input order of `profiles`; first
/// match wins. Ports with no match keep `device = None`, `description = None`.
pub fn enrich_with_profiles(ports: &mut [PortDescriptor], profiles: &[DeviceProfile]) {
    for port in ports.iter_mut() {
        if let Some(p) = profiles.iter().find(|p| profile_matches_port(p, port)) {
            port.device = Some(p.name.clone());
            port.description = Some(p.description.clone());
        }
    }
}

/// Filter a raw enumeration through [`config::matches_allowlist`] and map
/// surviving entries into [`PortDescriptor`]. Pure function so the allowlist
/// logic is unit-testable without touching the host's serial devices.
pub fn filter_allowlisted(ports: Vec<SerialPortInfo>) -> Vec<PortDescriptor> {
    ports
        .into_iter()
        .filter(|p| config::matches_allowlist(&p.port_name))
        .map(PortDescriptor::from)
        .collect()
}

/// Enumerate available serial ports, filter through the allowlist, and
/// enrich with `device` / `description` from any matching profile.
pub fn list_ports(profiles: &[DeviceProfile]) -> Result<Vec<PortDescriptor>, SerialError> {
    let raw = tokio_serial::available_ports().map_err(|e| SerialError::Io {
        message: format!("available_ports: {e}"),
    })?;
    let mut descriptors = filter_allowlisted(raw);
    enrich_with_profiles(&mut descriptors, profiles);
    Ok(descriptors)
}

/// Pluggable opener for the underlying serial transport.
///
/// The associated [`SerialBackend::Port`] type is the concrete handle the
/// manager will read from and write to: production uses
/// `tokio_serial::SerialStream`; tests use `tokio::io::DuplexStream` or
/// `tokio::io::Join<Empty, Sink>` when no real I/O is needed.
pub trait SerialBackend: Send + Sync + 'static {
    type Port: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static;

    /// Open the given device at `baud`. Implementations should map underlying
    /// I/O errors to [`SerialError::Io`] or [`SerialError::PortNotFound`].
    fn open(
        &self,
        port: &str,
        baud: u32,
    ) -> impl std::future::Future<Output = Result<Self::Port, SerialError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_serial::UsbPortInfo;

    fn usb_port(name: &str, vid: u16, pid: u16, sn: Option<&str>) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.into(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid,
                serial_number: sn.map(String::from),
                manufacturer: None,
                product: None,
            }),
        }
    }

    fn other_port(name: &str, ty: SerialPortType) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.into(),
            port_type: ty,
        }
    }

    #[test]
    fn filter_keeps_only_allowlisted_paths() {
        let ports = vec![
            usb_port("/dev/ttyUSB0", 0x303A, 0x1001, Some("ESP32-CHIP")),
            other_port("/dev/ttyACM1", SerialPortType::PciPort),
            // These three must be dropped — not in `/dev/ttyUSB*` or `/dev/ttyACM*`.
            other_port("/dev/ttyS0", SerialPortType::Unknown),
            other_port("/dev/null", SerialPortType::Unknown),
            usb_port("/dev/bus/usb/001/002", 0xDEAD, 0xBEEF, None),
        ];

        let filtered = filter_allowlisted(ports);
        let names: Vec<&str> = filtered.iter().map(|p| p.port.as_str()).collect();
        assert_eq!(names, vec!["/dev/ttyUSB0", "/dev/ttyACM1"]);
    }

    #[test]
    fn usb_port_descriptor_extracts_vid_pid_serial() {
        let filtered = filter_allowlisted(vec![usb_port(
            "/dev/ttyUSB0",
            0x303A,
            0x1001,
            Some("ESP32-CHIP"),
        )]);
        assert_eq!(filtered.len(), 1);
        let d = &filtered[0];
        assert_eq!(d.port, "/dev/ttyUSB0");
        assert_eq!(d.vid, Some(0x303A));
        assert_eq!(d.pid, Some(0x1001));
        assert_eq!(d.serial.as_deref(), Some("ESP32-CHIP"));
    }

    #[test]
    fn non_usb_port_descriptor_has_null_metadata() {
        let filtered = filter_allowlisted(vec![other_port(
            "/dev/ttyACM0",
            SerialPortType::PciPort,
        )]);
        assert_eq!(filtered.len(), 1);
        let d = &filtered[0];
        assert_eq!(d.port, "/dev/ttyACM0");
        assert_eq!(d.vid, None);
        assert_eq!(d.pid, None);
        assert_eq!(d.serial, None);
    }

    #[test]
    fn usb_port_without_serial_number_serializes_nulls_correctly() {
        let filtered = filter_allowlisted(vec![usb_port("/dev/ttyUSB7", 0x0403, 0x6001, None)]);
        let json = serde_json::to_value(&filtered[0]).unwrap();
        assert_eq!(json["port"], "/dev/ttyUSB7");
        assert_eq!(json["vid"], 0x0403);
        assert_eq!(json["pid"], 0x6001);
        assert!(json["serial"].is_null());
        // Device / description are null when no profile matches.
        assert!(json["device"].is_null());
        assert!(json["description"].is_null());
    }

    // --- profile matching --- //

    fn profile(name: &str, serial: &str, vid: Option<u16>, pid: Option<u16>) -> DeviceProfile {
        DeviceProfile {
            name: name.into(),
            match_serial: serial.into(),
            match_vid: vid,
            match_pid: pid,
            baud: 115_200,
            description: format!("{name} test"),
            probe: None,
            tags: vec![],
        }
    }

    #[test]
    fn profile_matches_on_serial_only() {
        let p = profile("esp32c6", "ABC123", None, None);
        let mut ports = filter_allowlisted(vec![usb_port("/dev/ttyUSB0", 0x10C4, 0xEA60, Some("ABC123"))]);
        enrich_with_profiles(&mut ports, &[p]);
        assert_eq!(ports[0].device.as_deref(), Some("esp32c6"));
        assert_eq!(ports[0].description.as_deref(), Some("esp32c6 test"));
    }

    #[test]
    fn profile_with_vid_pid_filter_rejects_mismatched_ids() {
        // Same serial, but the profile requires a specific VID/PID that
        // doesn't match this port. Must NOT match.
        let p = profile("esp32c6", "DUP-SERIAL", Some(0x303A), Some(0x1001));
        let mut ports = filter_allowlisted(vec![usb_port(
            "/dev/ttyUSB0",
            0x10C4, // wrong vid
            0xEA60, // wrong pid
            Some("DUP-SERIAL"),
        )]);
        enrich_with_profiles(&mut ports, &[p]);
        assert_eq!(ports[0].device, None, "vid/pid mismatch must reject");
    }

    #[test]
    fn profile_with_vid_pid_filter_accepts_full_match() {
        let p = profile("esp32c6", "DUP-SERIAL", Some(0x303A), Some(0x1001));
        let mut ports = filter_allowlisted(vec![usb_port(
            "/dev/ttyUSB0",
            0x303A,
            0x1001,
            Some("DUP-SERIAL"),
        )]);
        enrich_with_profiles(&mut ports, &[p]);
        assert_eq!(ports[0].device.as_deref(), Some("esp32c6"));
    }

    #[test]
    fn enrichment_first_match_wins_for_overlapping_profiles() {
        let a = profile("first", "ABC123", None, None);
        let b = profile("second", "ABC123", None, None);
        let mut ports = filter_allowlisted(vec![usb_port("/dev/ttyUSB0", 0x10C4, 0xEA60, Some("ABC123"))]);
        enrich_with_profiles(&mut ports, &[a, b]);
        assert_eq!(ports[0].device.as_deref(), Some("first"));
    }

    #[test]
    fn enrichment_leaves_non_matching_ports_unannotated() {
        let p = profile("esp32c6", "ABC123", None, None);
        let mut ports = filter_allowlisted(vec![
            usb_port("/dev/ttyUSB0", 0x10C4, 0xEA60, Some("ABC123")),
            usb_port("/dev/ttyUSB1", 0x10C4, 0xEA60, Some("OTHER")),
        ]);
        enrich_with_profiles(&mut ports, &[p]);
        assert_eq!(ports[0].device.as_deref(), Some("esp32c6"));
        assert_eq!(ports[1].device, None);
        assert_eq!(ports[1].description, None);
    }

    #[test]
    fn port_without_usb_serial_never_matches() {
        let p = profile("anything", "anything", None, None);
        let mut ports = filter_allowlisted(vec![other_port(
            "/dev/ttyACM0",
            SerialPortType::PciPort,
        )]);
        enrich_with_profiles(&mut ports, &[p]);
        assert_eq!(ports[0].device, None);
    }

    #[test]
    fn filter_preserves_input_order() {
        let ports = vec![
            usb_port("/dev/ttyUSB3", 0x1, 0x2, None),
            usb_port("/dev/ttyUSB0", 0x3, 0x4, None),
            usb_port("/dev/ttyUSB1", 0x5, 0x6, None),
        ];
        let filtered = filter_allowlisted(ports);
        let names: Vec<&str> = filtered.iter().map(|p| p.port.as_str()).collect();
        assert_eq!(names, vec!["/dev/ttyUSB3", "/dev/ttyUSB0", "/dev/ttyUSB1"]);
    }
}
