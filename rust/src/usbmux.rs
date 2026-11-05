//! USB mux transport: a minimal, single-connection client for
//! the protocol an iPhone speaks on its USB usbmux interface,
//! written from the protocol as implemented by usbmuxd's
//! `device.c` (libimobiledevice). idevice 0.1.68 ships no
//! direct-USB provider — only a client for a desktop usbmuxd
//! daemon, which Android does not have — so the wire layer
//! lives here.
//!
//! Wire shape (all integers big-endian):
//! - Every packet is framed by a mux header: `{u32 protocol,
//!   u32 length}` (8 bytes) before the protocol version is
//!   known, growing to 16 bytes at version 2 with `{u32 magic
//!   = 0xfeedface, u16 tx_seq, u16 rx_seq}` appended. `length`
//!   counts the whole packet including the header. A packet
//!   may be split across several USB transfers and is
//!   reassembled by that length.
//! - Handshake: we send a VERSION packet (protocol 0)
//!   carrying `{major: 2, minor: 0, padding: 0}`; the device
//!   answers in kind. At version 2 we then send a SETUP
//!   packet (protocol 2, one payload byte 0x07) with the mux
//!   seq counters reset (tx = 0, rx = 0xFFFF), after which
//!   the link is ACTIVE.
//! - Connections are a TCP lookalike (protocol 6) with a
//!   plain 20-byte TCP header: SYN -> SYN/ACK -> ACK with
//!   sequence numbers starting at 0 on both sides, the
//!   device preferring prompt ACKs of received data, and
//!   RST (with a reason string as payload) for resets.
//!   Deliberately out of scope, per the approved minimal
//!   design: connection multiplexing, multiple devices, the
//!   CONTROL protocol (inbound messages are logged and
//!   dropped), and any retransmission state machine.
//!
//! The byte pipe underneath is the Kotlin `UsbMuxLink`
//! (Android UsbManager bulk transfers), reached through JNI
//! callbacks on a dedicated IO thread; the async stream
//! surface adapts the thread's state machine to the
//! `ReadWrite` idevice consumes. All mux-level events
//! (version result, SYN/SYNACK, seq/ack movement, RST
//! reasons, framing splits) go to the app log: on real
//! hardware this log *is* the debugger. Payload bytes are
//! never logged (pairing traffic carries key material).

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use idevice::provider::IdeviceProvider;
use idevice::{Idevice, IdeviceError};
use jni::objects::{GlobalRef, JByteArray, JString, JValue};
use jni::{JNIEnv, JavaVM};

use crate::log_line;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const PROTO_VERSION: u32 = 0;
const PROTO_CONTROL: u32 = 1;
const PROTO_SETUP: u32 = 2;
const PROTO_TCP: u32 = 6; // IPPROTO_TCP

const FLAG_FIN: u8 = 0x01;
const FLAG_SYN: u8 = 0x02;
const FLAG_RST: u8 = 0x04;
const FLAG_ACK: u8 = 0x10;

const MUX_MAGIC: u32 = 0xfeedface;
/// usbmuxd's USB_MTU: the largest mux packet we may emit,
/// header included.
const USB_MTU: usize = 3 * 16384;
/// Largest mux packet we accept (usbmuxd's DEV_MRU).
const MAX_PACKET: usize = 65536;
/// Advertised TCP window, as in usbmuxd (sent >> 8 on wire).
const TX_WIN: u32 = 131072;
/// USB read size per transfer attempt.
const READ_CHUNK: usize = 16384;

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn jerr(e: jni::errors::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("jni: {}", e))
}

// ----------------------------------------------------------------
// JNI bridge to the Kotlin bulk-transfer object
// ----------------------------------------------------------------

/// Thin wrapper over the Kotlin `UsbMuxLink`. All methods are
/// called with the IO thread's attached `JNIEnv`; the pairing
/// entry point takes the device note once for the log.
pub struct UsbBridge {
    vm: JavaVM,
    link: GlobalRef,
}

impl fmt::Debug for UsbBridge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UsbBridge")
    }
}

impl UsbBridge {
    pub fn new(vm: JavaVM, link: GlobalRef) -> Self {
        Self { vm, link }
    }

    fn check_exception(env: &mut JNIEnv, what: &str) -> io::Result<()> {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{}: Kotlin side threw", what),
            ));
        }
        Ok(())
    }

    /// One-line description of the opened USB link (device,
    /// configuration, interface, endpoints) for the log.
    pub fn note(&self, env: &mut JNIEnv) -> String {
        let out = (|| -> Result<String, jni::errors::Error> {
            let v = env.call_method(
                self.link.as_obj(),
                "usbNote",
                "()Ljava/lang/String;",
                &[],
            )?;
            let s = JString::from(v.l()?);
            let text = String::from(env.get_string(&s)?);
            Ok(text)
        })();
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        out.unwrap_or_else(|e| format!("<usbNote failed: {}>", e))
    }

    /// Write one whole mux packet. The Kotlin side loops over
    /// partial bulk transfers; anything but a full-length
    /// positive return is a hard error.
    pub fn write_packet(&self, env: &mut JNIEnv, data: &[u8]) -> io::Result<()> {
        let arr = env.byte_array_from_slice(data).map_err(jerr)?;
        let res = env.call_method(
            self.link.as_obj(),
            "usbWrite",
            "([B)I",
            &[JValue::Object(&arr)],
        );
        Self::check_exception(env, "usbWrite")?;
        let n = res.map_err(jerr)?.i().map_err(jerr)?;
        if n < 0 || n as usize != data.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("usbWrite returned {} for {} bytes", n, data.len()),
            ));
        }
        Ok(())
    }

    /// One read attempt: `Ok(None)` = timed out with no data,
    /// `Ok(Some)` = bytes read, `Err` = hard failure (Kotlin
    /// returned null, e.g. the connection is gone).
    pub fn read_chunk(
        &self,
        env: &mut JNIEnv,
        timeout_ms: i32,
    ) -> io::Result<Option<Vec<u8>>> {
        let res = env.call_method(
            self.link.as_obj(),
            "usbRead",
            "(II)[B",
            &[JValue::Int(READ_CHUNK as i32), JValue::Int(timeout_ms)],
        );
        Self::check_exception(env, "usbRead")?;
        let obj = res.map_err(jerr)?.l().map_err(jerr)?;
        if obj.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "usb link closed by the Kotlin side",
            ));
        }
        let arr = JByteArray::from(obj);
        let bytes = env.convert_byte_array(&arr).map_err(jerr)?;
        if bytes.is_empty() {
            Ok(None)
        } else {
            Ok(Some(bytes))
        }
    }
}

// ----------------------------------------------------------------
// Mux state machine (all fields guarded by one mutex; the IO
// thread and the async surface meet here)
// ----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TcpState {
    SynSent,
    Established,
}

struct MuxState {
    /// Negotiated mux protocol version (0 until the reply).
    version: u32,
    /// Mux-level counters echoed in the v2 header; ours is
    /// bumped per packet sent, theirs mirrors the last packet
    /// received (usbmuxd semantics).
    dev_tx_seq: u16,
    dev_rx_seq: u16,
    version_replied: bool,
    tcp_state: Option<TcpState>,
    sport: u16,
    dport: u16,
    tx_seq: u32,
    tx_ack: u32,
    /// Device's last ack of our bytes and advertised window
    /// (already shifted back << 8).
    rx_ack: u32,
    rx_win: u32,
    peer_fin: bool,
    /// Framing accumulator and the reassembled stream bytes.
    accum: Vec<u8>,
    rx_app: VecDeque<u8>,
    /// Whole mux packets waiting for the IO thread to write,
    /// and app bytes not yet packetized under the window.
    out_frames: VecDeque<Vec<u8>>,
    tx_pending: VecDeque<u8>,
    connected: bool,
    conn_error: Option<String>,
    closed: bool,
    rst_sent: bool,
    io_dead: bool,
    conn_waker: Option<Waker>,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
}

impl MuxState {
    fn new(sport: u16, dport: u16) -> Self {
        Self {
            version: 0,
            dev_tx_seq: 0,
            dev_rx_seq: 0,
            version_replied: false,
            tcp_state: None,
            sport,
            dport,
            tx_seq: 0,
            tx_ack: 0,
            rx_ack: 0,
            rx_win: 0,
            peer_fin: false,
            accum: Vec::new(),
            rx_app: VecDeque::new(),
            out_frames: VecDeque::new(),
            tx_pending: VecDeque::new(),
            connected: false,
            conn_error: None,
            closed: false,
            rst_sent: false,
            io_dead: false,
            conn_waker: None,
            read_waker: None,
            write_waker: None,
        }
    }

    fn header_size(&self) -> usize {
        if self.version >= 2 {
            16
        } else {
            8
        }
    }

    fn fail(&mut self, msg: String) {
        logf!("usbmux: fatal: {}", msg);
        if self.conn_error.is_none() {
            self.conn_error = Some(msg);
        }
    }

    fn build_frame(&mut self, proto: u32, payload: &[u8]) -> Vec<u8> {
        let hs = self.header_size();
        let total = hs + payload.len();
        let mut f = Vec::with_capacity(total);
        f.extend_from_slice(&proto.to_be_bytes());
        f.extend_from_slice(&(total as u32).to_be_bytes());
        if self.version >= 2 {
            f.extend_from_slice(&MUX_MAGIC.to_be_bytes());
            f.extend_from_slice(&self.dev_tx_seq.to_be_bytes());
            f.extend_from_slice(&self.dev_rx_seq.to_be_bytes());
            self.dev_tx_seq = self.dev_tx_seq.wrapping_add(1);
        }
        f.extend_from_slice(payload);
        f
    }

    fn build_tcp(&mut self, flags: u8, data: &[u8]) -> Vec<u8> {
        let mut th = [0u8; 20];
        th[0..2].copy_from_slice(&self.sport.to_be_bytes());
        th[2..4].copy_from_slice(&self.dport.to_be_bytes());
        th[4..8].copy_from_slice(&self.tx_seq.to_be_bytes());
        th[8..12].copy_from_slice(&self.tx_ack.to_be_bytes());
        th[12] = 5 << 4;
        th[13] = flags;
        th[14..16].copy_from_slice(&((TX_WIN >> 8) as u16).to_be_bytes());
        let mut payload = th.to_vec();
        payload.extend_from_slice(data);
        self.build_frame(PROTO_TCP, &payload)
    }
}

impl MuxState {
    /// Reassemble mux packets from the raw USB byte stream.
    /// Packets may be split across several transfers; the
    /// length field (offset 4, present in both header sizes)
    /// is the only framing.
    fn feed(&mut self, bytes: &[u8]) {
        self.accum.extend_from_slice(bytes);
        loop {
            if self.accum.len() < 8 {
                break;
            }
            let total = be32(&self.accum[4..8]) as usize;
            if !(8..=MAX_PACKET).contains(&total) {
                let head: Vec<String> = self
                    .accum
                    .iter()
                    .take(16)
                    .map(|b| format!("{:02x}", b))
                    .collect();
                self.fail(format!(
                    "framing error: packet length {} (header {})",
                    total,
                    head.join(" ")
                ));
                self.accum.clear();
                break;
            }
            if self.accum.len() < total {
                logf!(
                    "usbmux: packet split across transfers ({}/{} bytes so far)",
                    self.accum.len(),
                    total
                );
                break;
            }
            let pkt: Vec<u8> = self.accum.drain(..total).collect();
            self.process_packet(&pkt);
            if self.conn_error.is_some() {
                break;
            }
        }
    }

    fn process_packet(&mut self, pkt: &[u8]) {
        let hs = self.header_size();
        if pkt.len() < hs {
            logf!("usbmux: runt packet {} bytes, dropped", pkt.len());
            return;
        }
        let proto = be32(&pkt[0..4]);
        if self.version >= 2 {
            let magic = be32(&pkt[8..12]);
            if magic != MUX_MAGIC {
                logf!(
                    "usbmux: bad magic {:#010x} on protocol {} (continuing)",
                    magic,
                    proto
                );
            }
            self.dev_rx_seq = be16(&pkt[14..16]);
        }
        let payload = &pkt[hs..];
        match proto {
            PROTO_VERSION => self.on_version(payload),
            PROTO_CONTROL => {
                if payload.is_empty() {
                    logf!("usbmux: empty CONTROL packet");
                } else {
                    let text = String::from_utf8_lossy(&payload[1..]);
                    logf!(
                        "usbmux: device control message type {}: {}",
                        payload[0],
                        text.trim()
                    );
                }
            }
            PROTO_SETUP => {
                logf!(
                    "usbmux: unexpected SETUP packet from device ({} bytes), ignoring",
                    payload.len()
                );
            }
            PROTO_TCP => self.on_tcp(payload),
            other => {
                logf!(
                    "usbmux: unknown protocol {} ({} bytes), ignoring",
                    other,
                    payload.len()
                );
            }
        }
    }

    fn on_version(&mut self, payload: &[u8]) {
        if self.version_replied {
            logf!("usbmux: duplicate version packet, ignoring");
            return;
        }
        if payload.len() < 12 {
            self.fail(format!(
                "version packet too small: {} bytes",
                payload.len()
            ));
            return;
        }
        let major = be32(&payload[0..4]);
        let minor = be32(&payload[4..8]);
        logf!("usbmux: version reply major={} minor={}", major, minor);
        if major != 1 && major != 2 {
            self.fail(format!(
                "device mux version {}.{} unsupported",
                major, minor
            ));
            return;
        }
        self.version_replied = true;
        if major >= 2 {
            self.version = 2;
            // usbmuxd resets both mux counters before SETUP;
            // the SETUP packet itself consumes tx_seq 0.
            self.dev_tx_seq = 0;
            self.dev_rx_seq = 0xFFFF;
            let setup = self.build_frame(PROTO_SETUP, &[0x07]);
            self.out_frames.push_back(setup);
            logf!("usbmux [OUT] setup (mux v2 active, seq counters reset)");
        } else {
            self.version = 1;
            logf!("usbmux: staying at mux v1 (8-byte headers)");
        }
        let syn = self.build_tcp(FLAG_SYN, &[]);
        logf!(
            "usbmux [OUT] tcp syn sport={} dport={} seq=0 ack=0 win={}",
            self.sport,
            self.dport,
            TX_WIN
        );
        self.out_frames.push_back(syn);
        self.tcp_state = Some(TcpState::SynSent);
    }
}

impl MuxState {
    fn on_tcp(&mut self, body: &[u8]) {
        if body.len() < 20 {
            logf!("usbmux: runt tcp packet {} bytes, dropped", body.len());
            return;
        }
        let dev_port = be16(&body[0..2]);
        let our_port = be16(&body[2..4]);
        let seq = be32(&body[4..8]);
        let ack = be32(&body[8..12]);
        let off = ((body[12] >> 4) as usize) * 4;
        let flags = body[13];
        let win = (be16(&body[14..16]) as u32) << 8;
        let data: &[u8] = if off >= 20 && body.len() >= off {
            &body[off..]
        } else if body.len() > 20 {
            &body[20..]
        } else {
            &[]
        };
        logf!(
            "usbmux [IN] tcp {}->{} seq={} ack={} flags={:#04x} win={} len={}",
            dev_port,
            our_port,
            seq,
            ack,
            flags,
            win,
            data.len()
        );
        if our_port != self.sport || dev_port != self.dport {
            logf!(
                "usbmux: packet for unknown connection {}->{}, answering anonymous RST",
                dev_port, our_port
            );
            let mut th = [0u8; 20];
            th[0..2].copy_from_slice(&our_port.to_be_bytes());
            th[2..4].copy_from_slice(&dev_port.to_be_bytes());
            th[8..12].copy_from_slice(&seq.to_be_bytes());
            th[12] = 5 << 4;
            th[13] = FLAG_RST;
            let frame = self.build_frame(PROTO_TCP, &th);
            self.out_frames.push_back(frame);
            return;
        }
        self.rx_ack = ack;
        self.rx_win = win;
        if flags & FLAG_RST != 0 {
            let reason = String::from_utf8_lossy(data).trim().to_string();
            let suffix = if reason.is_empty() {
                String::new()
            } else {
                format!(": {}", reason)
            };
            match self.tcp_state {
                Some(TcpState::SynSent) => {
                    self.fail(format!("connection refused by device{}", suffix))
                }
                _ => self.fail(format!("connection reset by device{}", suffix)),
            }
            return;
        }
        match self.tcp_state {
            Some(TcpState::SynSent) => {
                if flags == (FLAG_SYN | FLAG_ACK) {
                    // Our SYN consumed one of our sequence
                    // numbers; their SYN consumed one of
                    // theirs (ISN is 0 in practice).
                    self.tx_seq = 1;
                    self.tx_ack = seq.wrapping_add(1);
                    let frame = self.build_tcp(FLAG_ACK, &[]);
                    logf!(
                        "usbmux [OUT] tcp ack seq={} ack={}",
                        self.tx_seq, self.tx_ack
                    );
                    self.out_frames.push_back(frame);
                    self.tcp_state = Some(TcpState::Established);
                    self.connected = true;
                    logf!(
                        "usbmux: connected (sport={} dport={} device isn={})",
                        self.sport, self.dport, seq
                    );
                } else {
                    self.fail(format!(
                        "connect failed: unexpected flags {:#04x} during handshake",
                        flags
                    ));
                }
            }
            Some(TcpState::Established) => {
                if flags & FLAG_ACK == 0 {
                    self.fail(format!(
                        "unexpected non-ACK packet flags {:#04x} on established connection",
                        flags
                    ));
                    return;
                }
                if flags & (FLAG_SYN | FLAG_FIN | FLAG_RST) != 0 {
                    logf!(
                        "usbmux: unusual flag bits {:#04x} on established connection",
                        flags
                    );
                }
                if !data.is_empty() {
                    self.rx_app.extend(data.iter());
                    self.tx_ack = self.tx_ack.wrapping_add(data.len() as u32);
                    // The device prefers prompt ACKs.
                    let frame = self.build_tcp(FLAG_ACK, &[]);
                    logf!(
                        "usbmux [OUT] tcp ack seq={} ack={}",
                        self.tx_seq, self.tx_ack
                    );
                    self.out_frames.push_back(frame);
                }
                if flags & FLAG_FIN != 0 {
                    self.peer_fin = true;
                    logf!("usbmux: FIN from device");
                }
                self.flush_tx_pending();
            }
            None => {
                logf!("usbmux: tcp packet before SYN was sent, ignoring");
            }
        }
    }

    /// Packetize queued app bytes under the device-advertised
    /// window. Mirrors usbmuxd's `sendable` computation.
    fn flush_tx_pending(&mut self) {
        while !self.tx_pending.is_empty() {
            let sent = self.tx_seq.wrapping_sub(self.rx_ack) as i64;
            let sendable = (self.rx_win as i64).saturating_sub(sent);
            if sendable <= 0 {
                logf!(
                    "usbmux: device window full (win={} in-flight={}), holding {} bytes",
                    self.rx_win,
                    sent.max(0),
                    self.tx_pending.len()
                );
                break;
            }
            let max_payload = USB_MTU - self.header_size() - 20;
            let n = (sendable as usize)
                .min(max_payload)
                .min(self.tx_pending.len());
            let chunk: Vec<u8> = self.tx_pending.drain(..n).collect();
            let seq_before = self.tx_seq;
            let frame = self.build_tcp(FLAG_ACK, &chunk);
            self.tx_seq = self.tx_seq.wrapping_add(n as u32);
            logf!(
                "usbmux [OUT] tcp data seq={} ack={} len={}",
                seq_before,
                self.tx_ack,
                n
            );
            self.out_frames.push_back(frame);
        }
    }
}

// ----------------------------------------------------------------
// Async stream surface + dedicated USB IO thread
// ----------------------------------------------------------------

struct Shared {
    m: Mutex<MuxState>,
    bridge: Arc<UsbBridge>,
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, MuxState> {
    shared.m.lock().unwrap_or_else(|p| p.into_inner())
}

fn wake_all(st: &mut MuxState) {
    if let Some(w) = st.conn_waker.take() {
        w.wake();
    }
    if let Some(w) = st.read_waker.take() {
        w.wake();
    }
    if let Some(w) = st.write_waker.take() {
        w.wake();
    }
}

/// clonable handle to the single mux connection. Creating it
/// queues the VERSION packet and spawns the IO thread.
#[derive(Clone)]
pub struct UsbMuxStream {
    shared: Arc<Shared>,
}

impl fmt::Debug for UsbMuxStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UsbMuxStream")
    }
}

impl UsbMuxStream {
    pub fn new(bridge: Arc<UsbBridge>, sport: u16, dport: u16) -> Self {
        let shared = Arc::new(Shared {
            m: Mutex::new(MuxState::new(sport, dport)),
            bridge,
        });
        {
            let mut st = lock(&shared);
            let mut vh = Vec::with_capacity(12);
            vh.extend_from_slice(&2u32.to_be_bytes());
            vh.extend_from_slice(&0u32.to_be_bytes());
            vh.extend_from_slice(&0u32.to_be_bytes());
            let frame = st.build_frame(PROTO_VERSION, &vh);
            st.out_frames.push_back(frame);
        }
        let io_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("ipaloader-usbmux-io".to_string())
            .spawn(move || io_main(io_shared));
        if let Err(e) = spawned {
            let mut st = lock(&shared);
            st.io_dead = true;
            st.fail(format!("could not spawn mux io thread: {}", e));
        }
        Self { shared }
    }

    /// The fatal transport error, if one has been recorded
    /// (used to classify failures for the UI).
    pub fn error_detail(&self) -> Option<String> {
        lock(&self.shared).conn_error.clone()
    }

    pub async fn wait_connected(&self, timeout: Duration) -> io::Result<()> {
        let fut = WaitConnected {
            shared: self.shared.clone(),
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(r) => r,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for the mux connection",
            )),
        }
    }

    /// Best-effort teardown: RST for an established connection,
    /// then the IO thread drains the queue and exits.
    pub fn close(&self) {
        let mut st = lock(&self.shared);
        if st.closed {
            return;
        }
        st.closed = true;
        if st.tcp_state == Some(TcpState::Established) && !st.rst_sent {
            st.rst_sent = true;
            let frame = st.build_tcp(FLAG_RST, &[]);
            logf!("usbmux [OUT] tcp rst (shutdown)");
            st.out_frames.push_back(frame);
        }
        wake_all(&mut st);
    }
}

struct WaitConnected {
    shared: Arc<Shared>,
}

impl Future for WaitConnected {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut st = lock(&self.shared);
        if st.connected {
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = &st.conn_error {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.clone())));
        }
        if st.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "mux stream closed before connecting",
            )));
        }
        if st.io_dead {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "mux io thread died before connecting",
            )));
        }
        st.conn_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

fn fatal(shared: &Shared, msg: String) {
    let mut st = lock(shared);
    st.io_dead = true;
    st.fail(msg);
    wake_all(&mut st);
}

fn io_main(shared: Arc<Shared>) {
    let mut env = match shared
        .bridge
        .vm
        .attach_current_thread_permanently()
    {
        Ok(e) => e,
        Err(e) => {
            fatal(&shared, format!("mux io thread could not attach to the JVM: {}", e));
            return;
        }
    };
    logf!(
        "usbmux io: thread started, requesting mux version ({})",
        shared.bridge.note(&mut env)
    );
    loop {
        let frames: Vec<Vec<u8>> = {
            let mut st = lock(&shared);
            st.out_frames.drain(..).collect()
        };
        let mut write_failed: Option<String> = None;
        for frame in &frames {
            if let Err(e) = shared.bridge.write_packet(&mut env, frame) {
                write_failed = Some(format!("usb write failed: {}", e));
                break;
            }
        }
        if let Some(msg) = write_failed {
            fatal(&shared, msg);
            return;
        }
        {
            let st = lock(&shared);
            if st.closed && st.out_frames.is_empty() {
                logf!("usbmux io: closed, thread exiting");
                drop(st);
                let mut st2 = lock(&shared);
                st2.io_dead = true;
                wake_all(&mut st2);
                return;
            }
        }
        match shared.bridge.read_chunk(&mut env, 25) {
            Ok(Some(bytes)) => {
                let mut st = lock(&shared);
                st.feed(&bytes);
                wake_all(&mut st);
            }
            Ok(None) => {}
            Err(e) => {
                fatal(&shared, format!("usb read failed: {}", e));
                return;
            }
        }
    }
}

impl AsyncRead for UsbMuxStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut st = lock(&self.shared);
        if !st.rx_app.is_empty() {
            let n = buf.remaining().min(st.rx_app.len());
            let chunk: Vec<u8> = st.rx_app.drain(..n).collect();
            buf.put_slice(&chunk);
            return Poll::Ready(Ok(()));
        }
        if let Some(e) = &st.conn_error {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.clone())));
        }
        if st.peer_fin || st.closed || st.io_dead {
            return Poll::Ready(Ok(()));
        }
        st.read_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for UsbMuxStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut st = lock(&self.shared);
        if let Some(e) = &st.conn_error {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.clone())));
        }
        if st.closed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "mux stream closed",
            )));
        }
        if st.tcp_state != Some(TcpState::Established) {
            st.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        st.tx_pending.extend(data.iter());
        st.flush_tx_pending();
        Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        self.close();
        Poll::Ready(Ok(()))
    }
}

// ----------------------------------------------------------------
// idevice provider
// ----------------------------------------------------------------

/// `IdeviceProvider` over the USB mux stream. The stream is a
/// single connection to the lockdownd port; pairing reads
/// device values itself, so no stored pairing file is served.
#[derive(Debug)]
pub struct UsbMuxProvider {
    pub stream: UsbMuxStream,
    pub label: String,
}

impl IdeviceProvider for UsbMuxProvider {
    fn connect(
        &self,
        port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<Idevice, IdeviceError>> + Send>> {
        let stream = self.stream.clone();
        let label = self.label.clone();
        Box::pin(async move {
            if port != 62078 {
                logf!(
                    "usbmux provider: connect to non-lockdown port {} requested; \
                     the USB stream only carries lockdownd",
                    port
                );
            }
            stream.wait_connected(Duration::from_secs(15)).await?;
            Ok(Idevice::new(Box::new(stream), label))
        })
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn get_pairing_file(
        &self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<idevice::pairing_file::PairingFile, IdeviceError>,
                > + Send,
        >,
    > {
        Box::pin(async {
            Err(IdeviceError::UnexpectedResponse(
                "USB pairing provider holds no stored pairing file".to_string(),
            ))
        })
    }
}
