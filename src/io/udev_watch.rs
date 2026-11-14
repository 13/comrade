use std::collections::HashMap;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crossbeam_channel::Sender;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::board::DeviceInfo;
use crate::event::Event;

/// Build a `DeviceInfo` from udev properties. Pure. Non-USB or malformed -> `None`.
pub fn device_info_from_props(
    devnode: &Path,
    props: &HashMap<String, String>,
    gid: u32,
) -> Option<DeviceInfo> {
    if props.get("ID_BUS").map(String::as_str) != Some("usb") {
        return None;
    }
    let vid = u16::from_str_radix(props.get("ID_VENDOR_ID")?, 16).ok()?;
    let pid = u16::from_str_radix(props.get("ID_MODEL_ID")?, 16).ok()?;
    let product = props
        .get("ID_MODEL_FROM_DATABASE")
        .or_else(|| props.get("ID_MODEL"))
        .map(|m| m.replace('_', " "));
    Some(DeviceInfo {
        devnode: devnode.to_path_buf(),
        vid,
        pid,
        serial: props.get("ID_SERIAL_SHORT").cloned(),
        id_path: props.get("ID_PATH").cloned(),
        product,
        gid,
    })
}

fn collect_props(device: &udev::Device) -> HashMap<String, String> {
    let mut props: HashMap<String, String> = device
        .properties()
        .map(|p| {
            (
                p.name().to_string_lossy().into_owned(),
                p.value().to_string_lossy().into_owned(),
            )
        })
        .collect();
    // Fallback: some setups only expose USB data on the parent usb_device.
    if !props.contains_key("ID_VENDOR_ID")
        && let Ok(Some(usb)) = device.parent_with_subsystem_devtype("usb", "usb_device")
    {
        let attr = |n: &str| {
            usb.attribute_value(n)
                .map(|v| v.to_string_lossy().into_owned())
        };
        if let (Some(v), Some(m)) = (attr("idVendor"), attr("idProduct")) {
            props.insert("ID_BUS".into(), "usb".into());
            props.insert("ID_VENDOR_ID".into(), v);
            props.insert("ID_MODEL_ID".into(), m);
            if let Some(s) = attr("serial") {
                props.entry("ID_SERIAL_SHORT".into()).or_insert(s);
            }
            if let Some(p) = attr("product") {
                props.entry("ID_MODEL".into()).or_insert(p);
            }
        }
    }
    props
}

fn info_for(device: &udev::Device) -> Option<DeviceInfo> {
    let devnode = device.devnode()?.to_path_buf();
    let gid = std::fs::metadata(&devnode).ok()?.gid();
    device_info_from_props(&devnode, &collect_props(device), gid)
}

/// Enumerate existing USB tty devices.
pub fn enumerate() -> io::Result<Vec<DeviceInfo>> {
    let mut en = udev::Enumerator::new()?;
    en.match_subsystem("tty")?;
    Ok(en.scan_devices()?.filter_map(|d| info_for(&d)).collect())
}

/// Start the watcher thread. `MonitorSocket` is not `Send`, so the thread
/// builds it itself, before enumerating, and reports back whether listening
/// succeeded; `spawn` returns once the monitor is live (enumeration continues
/// on the thread).
pub fn spawn(tx: Sender<Event>) -> io::Result<()> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<io::Result<()>>();
    std::thread::Builder::new()
        .name("udev-watch".into())
        .spawn(move || {
            let socket = match udev::MonitorBuilder::new()
                .and_then(|b| b.match_subsystem("tty"))
                .and_then(|b| b.listen())
            {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            run(socket, tx);
        })?;
    ready_rx
        .recv()
        .unwrap_or_else(|_| Err(io::Error::other("udev-watch thread died")))
}

fn run(socket: udev::MonitorSocket, tx: Sender<Event>) {
    if let Ok(devs) = enumerate() {
        for d in devs {
            if tx.send(Event::DeviceAdded(d)).is_err() {
                return;
            }
        }
    }
    // Everything after this is a hotplug (Core resets ESP boards on plug).
    if tx.send(Event::EnumerationDone).is_err() {
        return;
    }
    loop {
        let mut fds = [PollFd::new(socket.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(_) => {
                // Avoid a hot loop on persistent poll errors.
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
        }
        for ev in socket.iter() {
            let sent = match ev.event_type() {
                udev::EventType::Add => match info_for(&ev) {
                    Some(info) => tx.send(Event::DeviceAdded(info)),
                    None => Ok(()),
                },
                udev::EventType::Remove => match ev.devnode() {
                    Some(n) => tx.send(Event::DeviceRemoved {
                        devnode: n.to_path_buf(),
                    }),
                    None => Ok(()),
                },
                _ => Ok(()),
            };
            if sent.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_esp_jtag_props() {
        let p = props(&[
            ("ID_BUS", "usb"),
            ("ID_VENDOR_ID", "303a"),
            ("ID_MODEL_ID", "1001"),
            ("ID_SERIAL_SHORT", "AA:BB:CC"),
            ("ID_PATH", "pci-0000:00:14.0-usb-0:2:1.0"),
            ("ID_MODEL", "USB_JTAG_serial_debug_unit"),
        ]);
        let d = device_info_from_props(Path::new("/dev/ttyACM1"), &p, 985).unwrap();
        assert_eq!(d.devnode, PathBuf::from("/dev/ttyACM1"));
        assert_eq!((d.vid, d.pid), (0x303a, 0x1001));
        assert_eq!(d.serial.as_deref(), Some("AA:BB:CC"));
        assert_eq!(d.id_path.as_deref(), Some("pci-0000:00:14.0-usb-0:2:1.0"));
        assert_eq!(d.product.as_deref(), Some("USB JTAG serial debug unit"));
        assert_eq!(d.gid, 985);
    }

    #[test]
    fn database_model_preferred() {
        let p = props(&[
            ("ID_BUS", "usb"),
            ("ID_VENDOR_ID", "04e8"),
            ("ID_MODEL_ID", "6860"),
            ("ID_MODEL", "SAMSUNG_Android"),
            ("ID_MODEL_FROM_DATABASE", "Galaxy (MTP)"),
        ]);
        let d = device_info_from_props(Path::new("/dev/ttyACM0"), &p, 1).unwrap();
        assert_eq!(d.product.as_deref(), Some("Galaxy (MTP)"));
    }

    #[test]
    fn serialless_ch340_uses_id_path() {
        let p = props(&[
            ("ID_BUS", "usb"),
            ("ID_VENDOR_ID", "1a86"),
            ("ID_MODEL_ID", "7523"),
            ("ID_PATH", "pci-0000:00:14.0-usb-0:3:1.0"),
        ]);
        let d = device_info_from_props(Path::new("/dev/ttyUSB0"), &p, 1).unwrap();
        assert_eq!(d.serial, None);
        assert_eq!(d.id_path.as_deref(), Some("pci-0000:00:14.0-usb-0:3:1.0"));
        assert_eq!(d.product, None);
    }

    #[test]
    fn non_usb_tty_is_none() {
        let p = props(&[("ID_BUS", "pci")]);
        assert!(device_info_from_props(Path::new("/dev/ttyS0"), &p, 1).is_none());
        assert!(device_info_from_props(Path::new("/dev/ttyS0"), &HashMap::new(), 1).is_none());
    }

    #[test]
    fn bad_hex_is_none() {
        let p = props(&[
            ("ID_BUS", "usb"),
            ("ID_VENDOR_ID", "zzzz"),
            ("ID_MODEL_ID", "7523"),
        ]);
        assert!(device_info_from_props(Path::new("/dev/ttyUSB0"), &p, 1).is_none());
    }

    #[test]
    #[ignore]
    fn enumerates_real_system() {
        let devs = enumerate().unwrap();
        for d in &devs {
            println!("{d:?}");
        }
    }
}
