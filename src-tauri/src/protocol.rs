//! HUAWEI FreeClip 2 SPP (RFCOMM) private protocol.
//!
//! Frame:  5A | LEN(2,BE) | 00 | SVC(1) | CMD(1) | TLV... | CRC16/XMODEM(2,BE)
//! TLV:    TYPE(1) | SIZE(1) | VALUE
//! LEN = 1 + 2 + len(params)
//!
//! Reverse-engineered from the official AI Life app HCI snoop log by the
//! original Python project; reimplemented here in Rust.

use serialport::SerialPort;
use std::time::{Duration, Instant};

pub const SERVICE_BATTERY: u8 = 0x01;
pub const CMD_READ_BATTERY: u8 = 0x08;
pub const SERVICE_GENERIC: u8 = 0x2B;
pub const CMD_SESSION_HELLO: u8 = 0x70;
pub const CMD_GENERIC_SETTINGS: u8 = 0xB4;
pub const CMD_ACK: u8 = 0x06;
pub const FEATURE_SPATIAL_AUDIO: u8 = 0x18;

pub const SPATIAL_OFF: u8 = 0;
pub const SPATIAL_HEAD_TRACKING: u8 = 1;
pub const SPATIAL_FIXED: u8 = 2;

const MAGIC: u8 = 0x5A;
const MAX_FRAME: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("串口操作失败: {0}")]
    Serial(String),
    #[error("会话握手失败（2B/70 无回应）")]
    Handshake,
    #[error("未收到耳机的设置回包")]
    NoReply,
    #[error("耳机当前不可用：{0}")]
    Unavailable(String),
}

impl From<serialport::Error> for Error {
    fn from(e: serialport::Error) -> Self {
        Error::Serial(e.to_string())
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Serial(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// CRC-16/XMODEM, poly 0x1021, init 0x0000.
/// Sample: 5a0009000108010002000300 -> 0xfbb9
pub fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn tlv(ptype: u8, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + value.len());
    out.push(ptype);
    out.push(value.len() as u8);
    out.extend_from_slice(value);
    out
}

pub fn build_frame(service: u8, command: u8, params: &[u8]) -> Vec<u8> {
    let length = (3 + params.len()) as u16;
    let mut head = Vec::with_capacity(5 + params.len());
    head.push(MAGIC);
    head.extend_from_slice(&length.to_be_bytes());
    head.push(0x00);
    head.push(service);
    head.push(command);
    head.extend_from_slice(params);
    let crc = crc16_xmodem(&head);
    head.extend_from_slice(&crc.to_be_bytes());
    head
}

pub fn battery_frame() -> Vec<u8> {
    let mut p = tlv(1, &[]);
    p.extend(tlv(2, &[]));
    p.extend(tlv(3, &[]));
    build_frame(SERVICE_BATTERY, CMD_READ_BATTERY, &p)
}

pub fn hello_frame() -> Vec<u8> {
    build_frame(SERVICE_GENERIC, CMD_SESSION_HELLO, &tlv(1, &[0x01]))
}

/// Read a generic setting. FreeClip 2 requires the third empty TLV.
pub fn settings_read_frame(feature: u8) -> Vec<u8> {
    let mut p = tlv(1, &[feature]);
    p.extend(tlv(2, &[]));
    p.extend(tlv(3, &[]));
    build_frame(SERVICE_GENERIC, CMD_GENERIC_SETTINGS, &p)
}

pub fn settings_write_frame(feature: u8, value: u8) -> Vec<u8> {
    let mut p = tlv(1, &[feature]);
    p.extend(tlv(2, &[value]));
    build_frame(SERVICE_GENERIC, CMD_GENERIC_SETTINGS, &p)
}

pub type Frame = (u8, u8, Vec<u8>); // (service, command, params)

/// Incremental frame splitter. Keeps partial data between reads.
pub struct FrameFeed {
    buf: Vec<u8>,
}

impl FrameFeed {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let start = match self.buf.iter().position(|&b| b == MAGIC) {
                Some(i) => i,
                None => {
                    self.buf.clear();
                    return out;
                }
            };
            if start > 0 {
                self.buf.drain(..start);
            }
            if self.buf.len() < 5 {
                return out;
            }
            let len = u16::from_be_bytes([self.buf[1], self.buf[2]]) as usize;
            let total = 5 + len;
            if total > MAX_FRAME {
                self.buf.remove(0);
                continue;
            }
            if self.buf.len() < total {
                return out;
            }
            let frame = self.buf[..total].to_vec();
            self.buf.drain(..total);
            let got = u16::from_be_bytes([frame[total - 2], frame[total - 1]]);
            if crc16_xmodem(&frame[..total - 2]) == got {
                out.push((frame[4], frame[5], frame[6..total - 2].to_vec()));
            }
        }
    }
}

pub fn parse_tlvs(params: &[u8]) -> std::collections::HashMap<u8, Vec<u8>> {
    let mut out = std::collections::HashMap::new();
    let mut i = 0;
    while i + 2 <= params.len() {
        let ptype = params[i];
        let size = params[i + 1] as usize;
        let end = (i + 2 + size).min(params.len());
        out.insert(ptype, params[i + 2..end].to_vec());
        i += 2 + size;
    }
    out
}

/// One short-lived SPP session. The port is owned by this value and is closed
/// on Drop (RAII) — no leaked COM handle, even if an error occurs.
pub struct Buds {
    port: Box<dyn SerialPort>,
    feed: FrameFeed,
}

impl Buds {
    pub fn open(port_name: &str) -> Result<Self> {
        eprintln!("[proto] opening {}", port_name);
        let mut port = serialport::new(port_name, 115200)
            .timeout(Duration::from_millis(120))
            .data_bits(serialport::DataBits::Eight)
            .flow_control(serialport::FlowControl::None)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .open()?;
        // Critical: Windows Bluetooth SPP (BTHENUM) virtual COM ports only
        // deliver data when DTR and RTS are asserted. pyserial does this by
        // default; serialport-rs does not, so without this every read returns
        // zero bytes and the earbuds appear dead.
        port.write_data_terminal_ready(true)?;
        port.write_request_to_send(true)?;
        eprintln!("[proto] {} opened OK (DTR/RTS set)", port_name);
        Ok(Self {
            port,
            feed: FrameFeed::new(),
        })
    }

    /// Send one frame, collect replies until `stop_on` is seen or timeout.
    fn transact(&mut self, frame: &[u8], timeout: Duration, stop_on: Option<(u8, u8)>) -> Result<Vec<Frame>> {
        self.port.clear(serialport::ClearBuffer::Input)?;
        eprintln!("[proto] TX {} bytes: {:02x?}", frame.len(), frame);
        self.port.write_all(frame)?;
        self.port.flush()?;
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.port.read(&mut buf) {
                Ok(n) if n > 0 => {
                    eprintln!("[proto] RX {} bytes: {:02x?}", n, &buf[..n]);
                    for f in self.feed.push(&buf[..n]) {
                        let hit = stop_on == Some((f.0, f.1));
                        out.push(f);
                        if hit {
                            return Ok(out);
                        }
                    }
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(5)),
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) => return Err(e.into()),
            }
        }
        eprintln!("[proto] timeout after {:?}, got {} frames", timeout, out.len());
        Ok(out)
    }

    /// FreeClip 2 does not always ACK the handshake, but the hello frame is
    /// still required before 0xB4 commands are honoured. Never fail on it.
    pub fn handshake(&mut self) -> Result<()> {
        for _ in 0..3 {
            let _ = self.transact(
                &hello_frame(),
                Duration::from_millis(300),
                Some((SERVICE_GENERIC, CMD_SESSION_HELLO)),
            );
        }
        Ok(())
    }

    /// `quick` shortens the reply timeout for background polling, where a
    /// non-earbud COM port must not cost 800ms per cycle.
    pub fn read_battery_quick(&mut self, quick: bool) -> Result<Battery> {
        let timeout = if quick { 400 } else { 800 };
        eprintln!("[proto] read_battery quick={} timeout={}ms", quick, timeout);
        let replies = self.transact(
            &battery_frame(),
            Duration::from_millis(timeout),
            Some((SERVICE_BATTERY, CMD_READ_BATTERY)),
        )?;
        for (svc, cmd, params) in replies {
            if (svc, cmd) == (SERVICE_BATTERY, CMD_READ_BATTERY) {
                let t = parse_tlvs(&params);
                let val = |k: u8, idx: usize| -> u8 {
                    t.get(&k).and_then(|v| v.get(idx)).copied().unwrap_or(0)
                };
                let b = Battery {
                    left: val(2, 0),
                    right: val(2, 1),
                    case: val(2, 2),
                };
                eprintln!("[proto] battery OK: {:?}", b);
                return Ok(b);
            }
        }
        eprintln!("[proto] battery NoReply");
        Err(Error::NoReply)
    }

    pub fn read_battery(&mut self) -> Result<Battery> {
        self.read_battery_quick(false)
    }

    pub fn read_spatial(&mut self) -> Result<u8> {
        let replies = self.transact(
            &settings_read_frame(FEATURE_SPATIAL_AUDIO),
            Duration::from_millis(800),
            Some((SERVICE_GENERIC, CMD_GENERIC_SETTINGS)),
        )?;
        for (svc, cmd, params) in replies {
            if (svc, cmd) == (SERVICE_GENERIC, CMD_GENERIC_SETTINGS) {
                let t = parse_tlvs(&params);
                if t.get(&1).map(|v| v.as_slice()) == Some(&[FEATURE_SPATIAL_AUDIO][..]) {
                    return t.get(&2).and_then(|v| v.first()).copied().ok_or(Error::NoReply);
                }
            }
        }
        Err(Error::NoReply)
    }

    pub fn write_spatial(&mut self, mode: u8) -> Result<()> {
        let _ = self.transact(
            &settings_write_frame(FEATURE_SPATIAL_AUDIO, mode),
            Duration::from_millis(500),
            Some((SERVICE_GENERIC, CMD_GENERIC_SETTINGS)),
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Battery {
    pub left: u8,
    pub right: u8,
    pub case: u8,
}

/// Enumerate Bluetooth RFCOMM virtual COM ports (Windows BTHENUM devices).
pub fn bluetooth_ports() -> Vec<PortInfo> {
    let mut out = Vec::new();
    match serialport::available_ports() {
        Ok(ports) => {
            for p in ports {
                // On Windows, serialport-rs reports BTHENUM (Bluetooth SPP)
                // virtual COM ports as `Unknown` with the VID/PID hidden in
                // the USB-info variant, so filtering on `BluetoothPort`
                // silently drops them. Instead, keep every port and let the
                // battery-frame probe in `find_earbuds` decide which one is
                // the earbuds. `is_device` marks ports that look like real
                // Bluetooth links (as opposed to COM1 / loopback).
                let info = match &p.port_type {
                    serialport::SerialPortType::BluetoothPort => Some(true),
                    serialport::SerialPortType::UsbPort(u) => {
                        // Huawei's Bluetooth VID is 0x027D (635); the SPP
                        // dongle itself may present as a USB port.
                        Some(u.vid == 0x027d || u.vid == 0x0a12)
                    }
                    serialport::SerialPortType::PciPort => Some(false),
                    serialport::SerialPortType::Unknown => None,
                };
                out.push(PortInfo {
                    name: p.port_name.clone(),
                    is_device: info.unwrap_or(false),
                });
            }
        }
        Err(_) => {}
    }
    out
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PortInfo {
    pub name: String,
    pub is_device: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_sample() {
        let sample = hex("5a0009000108010002000300");
        assert_eq!(crc16_xmodem(&sample), 0xfbb9);
    }

    #[test]
    fn frames_match_python() {
        assert_eq!(hex_str(&battery_frame()), "5a0009000108010002000300fbb9");
        assert_eq!(hex_str(&hello_frame()), "5a0006002b70010101f084");
        assert_eq!(
            hex_str(&settings_read_frame(FEATURE_SPATIAL_AUDIO)),
            "5a000a002bb4010118020003009b3f"
        );
        assert_eq!(
            hex_str(&settings_write_frame(FEATURE_SPATIAL_AUDIO, 2)),
            "5a0009002bb401011802010240af"
        );
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn hex_str(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }
    #[test]
    #[ignore = "需要耳机连着 COM9，手动跑: cargo test live_ -- --ignored --nocapture"]
    fn live_read_status() {
        let ports = bluetooth_ports();
        println!("bluetooth ports: {:?}", ports);
        let mut buds = Buds::open("COM9").expect("open COM9");
        buds.handshake().expect("handshake");
        let bat = buds.read_battery().expect("battery");
        let mode = buds.read_spatial().expect("spatial");
        println!("battery: {:?}  spatial mode: {}", bat, mode);
        assert!(mode <= 2);
    }

}
