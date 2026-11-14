use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub devnode: PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub serial: Option<String>,
    pub id_path: Option<String>,
    pub product: Option<String>,
    pub gid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct BoardKey(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Family {
    UartBridge,
    EspUsbJtag,
    NativeCdc,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinePolicy {
    pub dtr: bool,
    pub rts: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Auto(Family),
    Bootloader,
    Other,
}

pub fn classify(vid: u16, pid: u16) -> Class {
    match vid {
        0x10c4 | 0x1a86 | 0x0403 | 0x2a03 | 0x0483 => Class::Auto(Family::UartBridge),
        0x2341 => match pid {
            0x0043 | 0x0001 | 0x0243 | 0x0010 | 0x0042 => Class::Auto(Family::UartBridge),
            0x0036 | 0x0037 => Class::Bootloader,
            _ => Class::Auto(Family::NativeCdc),
        },
        0x303a => match pid {
            0x1001 => Class::Auto(Family::EspUsbJtag),
            _ => Class::Auto(Family::NativeCdc),
        },
        0x1b4f => match pid {
            0x9205 | 0x9207 => Class::Bootloader,
            _ => Class::Auto(Family::NativeCdc),
        },
        0x2e8a | 0x239a | 0x2886 | 0x16c0 => Class::Auto(Family::NativeCdc),
        _ => Class::Other,
    }
}

pub fn line_policy(f: Family) -> LinePolicy {
    match f {
        Family::UartBridge | Family::EspUsbJtag => LinePolicy {
            dtr: false,
            rts: false,
        },
        Family::NativeCdc | Family::Unknown => LinePolicy {
            dtr: true,
            rts: false,
        },
    }
}

pub fn board_key(d: &DeviceInfo) -> BoardKey {
    let (vid, pid) = (d.vid, d.pid);
    if let Some(s) = d.serial.as_deref().filter(|s| !s.is_empty()) {
        return BoardKey(format!("{vid:04x}:{pid:04x}:{s}"));
    }
    if let Some(p) = d.id_path.as_deref().filter(|p| !p.is_empty()) {
        return BoardKey(format!("{vid:04x}:{pid:04x}@{p}"));
    }
    BoardKey(format!("{vid:04x}:{pid:04x}@{}", d.devnode.display()))
}

pub fn friendly_name(d: &DeviceInfo) -> String {
    let known = match (d.vid, d.pid) {
        (0x303a, 0x1001) => Some("ESP32-S3/C3"),
        (0x2341, 0x0043) => Some("Uno"),
        (0x2341, 0x8036) => Some("Leonardo"),
        (0x1a86, _) => Some("CH340"),
        (0x10c4, _) => Some("CP210x"),
        (0x0403, _) => Some("FTDI"),
        (0x2e8a, _) => Some("RP2040"),
        _ => None,
    };
    if let Some(n) = known {
        return n.to_string();
    }
    match d.product.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => format!("{:04x}:{:04x}", d.vid, d.pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(
        vid: u16,
        pid: u16,
        serial: Option<&str>,
        id_path: Option<&str>,
        node: &str,
    ) -> DeviceInfo {
        DeviceInfo {
            devnode: PathBuf::from(node),
            vid,
            pid,
            serial: serial.map(String::from),
            id_path: id_path.map(String::from),
            product: None,
            gid: 20,
        }
    }

    #[test]
    fn samsung_phone_is_other() {
        assert_eq!(classify(0x04e8, 0x6860), Class::Other);
    }
    #[test]
    fn esp_jtag() {
        assert_eq!(classify(0x303a, 0x1001), Class::Auto(Family::EspUsbJtag));
    }
    #[test]
    fn uno_is_bridge_leonardo_native_caterina_boot() {
        assert_eq!(classify(0x2341, 0x0043), Class::Auto(Family::UartBridge));
        assert_eq!(classify(0x2341, 0x8036), Class::Auto(Family::NativeCdc));
        assert_eq!(classify(0x2341, 0x0036), Class::Bootloader);
    }
    #[test]
    fn other_vendors() {
        assert_eq!(classify(0x1a86, 0x7523), Class::Auto(Family::UartBridge));
        assert_eq!(classify(0x303a, 0x0002), Class::Auto(Family::NativeCdc));
        assert_eq!(classify(0x1b4f, 0x9205), Class::Bootloader);
        assert_eq!(classify(0x1b4f, 0x0001), Class::Auto(Family::NativeCdc));
        assert_eq!(classify(0x2e8a, 0x000a), Class::Auto(Family::NativeCdc));
    }
    #[test]
    fn policies() {
        assert_eq!(
            line_policy(Family::UartBridge),
            LinePolicy {
                dtr: false,
                rts: false
            }
        );
        assert_eq!(
            line_policy(Family::EspUsbJtag),
            LinePolicy {
                dtr: false,
                rts: false
            }
        );
        assert_eq!(
            line_policy(Family::NativeCdc),
            LinePolicy {
                dtr: true,
                rts: false
            }
        );
        assert_eq!(
            line_policy(Family::Unknown),
            LinePolicy {
                dtr: true,
                rts: false
            }
        );
    }
    #[test]
    fn key_prefers_serial() {
        let d = dev(0x303a, 0x1001, Some("AB"), Some("p"), "/dev/ttyACM0");
        assert_eq!(board_key(&d), BoardKey("303a:1001:AB".into()));
    }
    #[test]
    fn serialless_boards_keyed_by_path() {
        let a = dev(
            0x1a86,
            0x7523,
            None,
            Some("pci-0000:00:14.0-usb-0:1:1.0"),
            "/dev/ttyUSB0",
        );
        let b = dev(
            0x1a86,
            0x7523,
            None,
            Some("pci-0000:00:14.0-usb-0:2:1.0"),
            "/dev/ttyUSB1",
        );
        assert_ne!(board_key(&a), board_key(&b));
        let a2 = dev(
            0x1a86,
            0x7523,
            None,
            Some("pci-0000:00:14.0-usb-0:1:1.0"),
            "/dev/ttyUSB1",
        );
        assert_eq!(board_key(&a), board_key(&a2));
        assert_eq!(board_key(&a).0, "1a86:7523@pci-0000:00:14.0-usb-0:1:1.0");
    }
    #[test]
    fn key_falls_back_to_devnode_and_ignores_empty_serial() {
        let d = dev(0x1a86, 0x7523, Some(""), None, "/dev/ttyUSB0");
        assert_eq!(board_key(&d).0, "1a86:7523@/dev/ttyUSB0");
    }
    #[test]
    fn friendly_names() {
        assert_eq!(
            friendly_name(&dev(0x303a, 0x1001, None, None, "x")),
            "ESP32-S3/C3"
        );
        assert_eq!(friendly_name(&dev(0x2341, 0x0043, None, None, "x")), "Uno");
        assert_eq!(
            friendly_name(&dev(0x2341, 0x8036, None, None, "x")),
            "Leonardo"
        );
        assert_eq!(
            friendly_name(&dev(0x1a86, 0x7523, None, None, "x")),
            "CH340"
        );
        assert_eq!(
            friendly_name(&dev(0x10c4, 0xea60, None, None, "x")),
            "CP210x"
        );
        assert_eq!(friendly_name(&dev(0x0403, 0x6001, None, None, "x")), "FTDI");
        assert_eq!(
            friendly_name(&dev(0x2e8a, 0x000a, None, None, "x")),
            "RP2040"
        );
        let mut d = dev(0x1234, 0x5678, None, None, "x");
        assert_eq!(friendly_name(&d), "1234:5678");
        d.product = Some("Widget".into());
        assert_eq!(friendly_name(&d), "Widget");
    }
}
