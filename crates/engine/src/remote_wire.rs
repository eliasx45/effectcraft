//! The binary frame channel between the desktop app and a cloud frame server
//! (`effectcraft-cli serve --frames PORT`, `plan/cloud-render.md` CR2).
//!
//! Next to the JSON-lines control channel, frames need a channel that carries pixels without
//! base64 or line limits. Each message is one length-prefixed frame:
//!
//! ```text
//! "ECF1" | header length (u32 LE) | payload length (u32 LE) | header (JSON) | payload (bytes)
//! ```
//!
//! The header is an [`Up`] (client → server) or a [`Down`] (server → client), wrapping the
//! [`FrameMsg`] / [`FrameReply`] of [`crate::remote`]; the payload is a frame's encoded pixels
//! ([`crate::remote_encode`]) or, for [`Up::Echo`], any bytes.
//!
//! A connection starts with [`Up::Hello`] carrying the server's token: anything else, or a wrong
//! token, gets [`Down::Refused`] and the connection closes. v1 runs over an SSH tunnel (the
//! server binds loopback only), so the token only keeps a stray local process out; no TLS here.
//!
//! [`FrameClient`] (native) is the client: one background connection that reconnects with
//! backoff, reporting each (re)connection so the caller resends its project. A lost connection
//! is an [`Event`], never an error for the document.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::remote::{FrameMsg, FrameReply};
use crate::remote_encode::{Pixels, Want};

/// The first bytes of every frame.
pub const MAGIC: [u8; 4] = *b"ECF1";
/// The protocol version in [`Up::Hello`] / [`Down::Welcome`].
pub const VERSION: u32 = 1;
/// Largest header or payload of one frame (a whole project as JSON fits).
pub const MAX_PART: usize = 1 << 30;
/// Largest frame before the handshake (a hello is tiny).
pub const MAX_HELLO: usize = 64 << 10;

/// Client → server.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Up {
    /// The first message: the server's token.
    Hello {
        token: String,
        #[serde(default)]
        version: u32,
        /// Who connects (logs only).
        #[serde(default)]
        client: String,
    },
    /// A sync or a render request. `urgent`: the viewer waits for it (scrubbing), so it renders
    /// before prefetch; the newest urgent request first.
    Msg {
        msg: FrameMsg,
        #[serde(default)]
        want: Want,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        urgent: bool,
    },
    /// Forget these render requests (queued ones never render; rendering ones are not sent).
    Cancel { ids: Vec<u64> },
    /// A control-channel request run on the server's host (`docs/control-protocol.md`).
    Control { id: u64, method: String, params: Value },
    /// Send the payload back (round-trip and throughput checks).
    Echo { id: u64 },
}

/// Server → client.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Down {
    /// The token was right. `revision`: the project the server already holds (one it opened at
    /// start), if any.
    Welcome {
        version: u32,
        server: String,
        #[serde(default)]
        gpu: Option<String>,
        threads: u32,
        #[serde(default)]
        revision: Option<u64>,
    },
    /// The connection is refused (wrong token, too many wrong tokens, no hello).
    Refused { error: String },
    /// A reply to [`Up::Msg`]; a frame's pixels are in the payload, described by `pixels`.
    Reply {
        reply: FrameReply,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pixels: Option<Pixels>,
    },
    /// The control reply (`{"ok": …}`) to [`Up::Control`] `id`.
    Control { id: u64, reply: Value },
    /// [`Up::Echo`]'s payload, back.
    Echo { id: u64 },
}

/// Write one frame.
pub fn write_frame(w: &mut impl Write, header: &impl Serialize, payload: &[u8]) -> std::io::Result<()> {
    let head = serde_json::to_vec(header).map_err(std::io::Error::other)?;
    let len =
        |n: usize| u32::try_from(n).ok().filter(|_| n <= MAX_PART).ok_or_else(|| std::io::Error::other(format!("a frame part of {n} bytes is too large")));
    let mut pre = [0u8; 12];
    pre[..4].copy_from_slice(&MAGIC);
    pre[4..8].copy_from_slice(&len(head.len())?.to_le_bytes());
    pre[8..].copy_from_slice(&len(payload.len())?.to_le_bytes());
    w.write_all(&pre)?;
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one frame: its header (parsed) and payload. `max`: the largest header or payload taken.
/// Memory grows with the bytes that actually arrive, not with the lengths a peer claims.
pub fn read_frame<T: for<'de> Deserialize<'de>>(r: &mut impl Read, max: usize) -> std::io::Result<(T, Vec<u8>)> {
    let mut pre = [0u8; 12];
    r.read_exact(&mut pre)?;
    if pre[..4] != MAGIC {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "not an EffectCraft frame channel"));
    }
    let len = |b: &[u8]| -> std::io::Result<usize> {
        let n = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        if n > max.min(MAX_PART) {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("a frame part of {n} bytes is over the limit")));
        }
        Ok(n)
    };
    let (hn, pn) = (len(&pre[4..8])?, len(&pre[8..12])?);
    let head = read_n(r, hn)?;
    let payload = read_n(r, pn)?;
    let header = serde_json::from_slice(&head).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad frame header: {e}")))?;
    Ok((header, payload))
}

fn read_n(r: &mut impl Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(n.min(1 << 20));
    let got = r.take(n as u64).read_to_end(&mut out)?;
    if got != n {
        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the frame ended early"));
    }
    Ok(out)
}

#[cfg(not(target_arch = "wasm32"))]
pub use client::{Backoff, Event, FrameClient};

#[cfg(not(target_arch = "wasm32"))]
mod client {
    use std::net::{Shutdown, TcpStream, ToSocketAddrs};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use super::*;

    /// What happened on a [`FrameClient`]'s connection.
    #[derive(Debug)]
    pub enum Event {
        /// Connected (again): the server holds nothing of this client, so resend the project
        /// (a fresh [`crate::remote::Mirror`]).
        Connected { welcome: Down },
        /// A message from the server.
        Down(Down, Vec<u8>),
        /// The connection was lost; the client reconnects with backoff.
        Disconnected { error: String },
        /// The server refused the token: the client stops (it would be refused again).
        Refused { error: String },
    }

    /// Reconnect delays: `first`, doubling to `max`, back to `first` after a good connection.
    #[derive(Clone, Copy, Debug)]
    pub struct Backoff {
        pub first: Duration,
        pub max: Duration,
    }

    impl Default for Backoff {
        fn default() -> Self {
            Backoff { first: Duration::from_millis(100), max: Duration::from_secs(5) }
        }
    }

    impl Backoff {
        /// The delay after `failures` failed attempts in a row.
        pub fn delay(&self, failures: u32) -> Duration {
            self.first.saturating_mul(1u32.checked_shl(failures.min(16)).unwrap_or(u32::MAX)).min(self.max)
        }
    }

    struct Shared {
        stop: AtomicBool,
        /// The live socket (shut down to stop or to drop a connection).
        live: Mutex<Option<TcpStream>>,
        /// Bumps on every connection: messages queued for an older one are dropped.
        generation: AtomicU64,
        connected: AtomicBool,
        /// Bytes read and written on every connection (the wire, frame headers included).
        down: Arc<AtomicU64>,
        up: Arc<AtomicU64>,
    }

    /// Counts the bytes that go through a reader or writer.
    struct Counted<T> {
        inner: T,
        n: Arc<AtomicU64>,
    }

    impl<T: std::io::Read> std::io::Read for Counted<T> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let k = self.inner.read(buf)?;
            self.n.fetch_add(k as u64, Ordering::Relaxed);
            Ok(k)
        }
    }

    impl<T: std::io::Write> std::io::Write for Counted<T> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let k = self.inner.write(buf)?;
            self.n.fetch_add(k as u64, Ordering::Relaxed);
            Ok(k)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }

    /// A frame-channel client: one connection on a background thread, reconnecting with
    /// [`Backoff`]. Messages sent while disconnected are dropped ([`FrameClient::send`] says so).
    pub struct FrameClient {
        shared: Arc<Shared>,
        out: Sender<(u64, Up, Vec<u8>)>,
        events: Receiver<Event>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl FrameClient {
        /// Connect to `addr` (`host:port`; through an SSH tunnel, `127.0.0.1:<local port>`)
        /// with `token`, in the background.
        pub fn connect(addr: &str, token: &str, client: &str, backoff: Backoff) -> FrameClient {
            let shared = Arc::new(Shared {
                stop: AtomicBool::new(false),
                live: Mutex::new(None),
                generation: AtomicU64::new(0),
                connected: AtomicBool::new(false),
                down: Arc::default(),
                up: Arc::default(),
            });
            let (out, out_rx) = channel();
            let (ev_tx, events) = channel();
            let link =
                Link { addr: addr.to_string(), token: token.to_string(), client: client.to_string(), backoff, shared: Arc::clone(&shared), events: ev_tx };
            let thread = std::thread::Builder::new().name("ec-frames-link".into()).spawn(move || link.run(out_rx)).ok();
            FrameClient { shared, out, events, thread }
        }

        /// Bytes received and sent since the client started, over every connection: (down, up).
        pub fn bytes(&self) -> (u64, u64) {
            (self.shared.down.load(Ordering::Relaxed), self.shared.up.load(Ordering::Relaxed))
        }

        /// Whether a connection is up now.
        pub fn connected(&self) -> bool {
            self.shared.connected.load(Ordering::Acquire)
        }

        /// Queue a message for the current connection; `false` when there is none (the message
        /// is dropped: after [`Event::Connected`], resync and resend).
        pub fn send(&self, up: Up, payload: Vec<u8>) -> bool {
            if !self.connected() {
                return false;
            }
            self.out.send((self.shared.generation.load(Ordering::Acquire), up, payload)).is_ok()
        }

        /// The next event, waiting up to `timeout`.
        pub fn recv(&self, timeout: Duration) -> Option<Event> {
            self.events.recv_timeout(timeout).ok()
        }

        /// The next event without waiting.
        pub fn try_recv(&self) -> Option<Event> {
            self.events.try_recv().ok()
        }

        /// Drop the current connection (tests: as if the tunnel died); the client reconnects.
        pub fn drop_connection(&self) {
            if let Some(s) = self.shared.live.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }

    impl Drop for FrameClient {
        /// Stops the link without waiting for it (a connect in progress may take seconds to
        /// time out; the UI thread must not wait for that).
        fn drop(&mut self) {
            self.shared.stop.store(true, Ordering::Release);
            self.drop_connection();
            drop(self.thread.take());
        }
    }

    struct Link {
        addr: String,
        token: String,
        client: String,
        backoff: Backoff,
        shared: Arc<Shared>,
        events: Sender<Event>,
    }

    /// How long a connect or the handshake may take.
    const CONNECT: Duration = Duration::from_secs(5);
    /// How often the writer checks for a stop or a dead connection.
    const TICK: Duration = Duration::from_millis(100);

    impl Link {
        fn stopped(&self) -> bool {
            self.shared.stop.load(Ordering::Acquire)
        }

        /// Sleep `d` in short steps, waking for a stop.
        fn wait(&self, d: Duration) {
            let end = std::time::Instant::now() + d;
            while !self.stopped() {
                let now = std::time::Instant::now();
                if now >= end {
                    return;
                }
                std::thread::sleep((end - now).min(TICK));
            }
        }

        fn run(self, out: Receiver<(u64, Up, Vec<u8>)>) {
            let mut failures = 0u32;
            while !self.stopped() {
                match self.open() {
                    Ok((stream, welcome)) => {
                        failures = 0;
                        if let Some(error) = self.serve(stream, welcome, &out) {
                            let _ = self.events.send(Event::Disconnected { error });
                        }
                    }
                    Err(Opened::Refused(error)) => {
                        let _ = self.events.send(Event::Refused { error });
                        return;
                    }
                    Err(Opened::Failed(error)) => {
                        // Only the first failure in a row is reported (no flood while down).
                        if failures == 0 {
                            let _ = self.events.send(Event::Disconnected { error });
                        }
                        self.wait(self.backoff.delay(failures));
                        failures = failures.saturating_add(1);
                    }
                }
            }
        }

        fn open(&self) -> Result<(TcpStream, Down), Opened> {
            let failed = |e: String| Opened::Failed(format!("{}: {e}", self.addr));
            let sa = self.addr.to_socket_addrs().map_err(|e| failed(e.to_string()))?.next().ok_or_else(|| failed("cannot resolve".into()))?;
            let mut s = TcpStream::connect_timeout(&sa, CONNECT).map_err(|e| failed(e.to_string()))?;
            s.set_nodelay(true).ok();
            s.set_read_timeout(Some(CONNECT)).ok();
            let hello = Up::Hello { token: self.token.clone(), version: VERSION, client: self.client.clone() };
            write_frame(&mut s, &hello, &[]).map_err(|e| failed(e.to_string()))?;
            match read_frame::<Down>(&mut s, MAX_HELLO) {
                Ok((w @ Down::Welcome { .. }, _)) => {
                    s.set_read_timeout(None).ok();
                    Ok((s, w))
                }
                Ok((Down::Refused { error }, _)) => Err(Opened::Refused(error)),
                Ok((other, _)) => Err(failed(format!("unexpected handshake reply {other:?}"))),
                Err(e) => Err(failed(e.to_string())),
            }
        }

        /// Run one connection until it drops (`Some(why)`) or the client stops (`None`).
        fn serve(&self, stream: TcpStream, welcome: Down, out: &Receiver<(u64, Up, Vec<u8>)>) -> Option<String> {
            let (Ok(reader), Ok(keep), Ok(shut)) = (stream.try_clone(), stream.try_clone(), stream.try_clone()) else {
                return Some("cannot share the socket".into());
            };
            let mut reader = Counted { inner: reader, n: Arc::clone(&self.shared.down) };
            let mut writer = Counted { inner: stream, n: Arc::clone(&self.shared.up) };
            let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
            *self.shared.live.lock().unwrap_or_else(PoisonError::into_inner) = Some(keep);
            // Messages queued for an earlier connection are dropped.
            while out.try_recv().is_ok() {}
            self.shared.connected.store(true, Ordering::Release);
            let _ = self.events.send(Event::Connected { welcome });
            let alive = Arc::new(AtomicBool::new(true));
            let reader_thread = {
                let (events, alive) = (self.events.clone(), Arc::clone(&alive));
                std::thread::Builder::new()
                    .name("ec-frames-read".into())
                    .spawn(move || {
                        let error = loop {
                            match read_frame::<Down>(&mut reader, MAX_PART) {
                                Ok((d, p)) => {
                                    if events.send(Event::Down(d, p)).is_err() {
                                        break "the client is gone".to_string();
                                    }
                                }
                                Err(e) => break e.to_string(),
                            }
                        };
                        alive.store(false, Ordering::Release);
                        error
                    })
                    .ok()
            };
            let mut error = None;
            while !self.stopped() && alive.load(Ordering::Acquire) {
                match out.recv_timeout(TICK) {
                    Ok((g, up, payload)) if g == generation => {
                        if let Err(e) = write_frame(&mut writer, &up, &payload) {
                            error = Some(e.to_string());
                            break;
                        }
                    }
                    Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            self.shared.connected.store(false, Ordering::Release);
            let _ = shut.shutdown(Shutdown::Both);
            *self.shared.live.lock().unwrap_or_else(PoisonError::into_inner) = None;
            let read_error = reader_thread.and_then(|t| t.join().ok());
            if self.stopped() {
                return None;
            }
            Some(error.or(read_error).unwrap_or_else(|| "the connection closed".into()))
        }
    }

    enum Opened {
        Refused(String),
        Failed(String),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_limits_hold() {
        let mut buf = vec![];
        let up = Up::Echo { id: 7 };
        write_frame(&mut buf, &up, &[1, 2, 3]).unwrap();
        let (back, payload): (Up, _) = read_frame(&mut buf.as_slice(), MAX_PART).unwrap();
        assert!(matches!(back, Up::Echo { id: 7 }));
        assert_eq!(payload, vec![1, 2, 3]);
        // Over the limit: refused before reading the body.
        assert!(read_frame::<Up>(&mut buf.as_slice(), 2).is_err());
        // A peer that claims a huge payload and sends little: an error, no huge allocation.
        let mut liar = buf[..12].to_vec();
        liar[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_frame::<Up>(&mut liar.as_slice(), MAX_PART).is_err());
        let mut short = buf.clone();
        short[8..12].copy_from_slice(&1000u32.to_le_bytes());
        assert!(read_frame::<Up>(&mut short.as_slice(), MAX_PART).is_err());
        // Not our protocol (an HTTP request, a JSON line).
        assert!(read_frame::<Up>(&mut &b"GET / HTTP/1.1\r\n\r\n"[..], MAX_PART).is_err());
        // A header that is not an Up.
        let mut bad = vec![];
        write_frame(&mut bad, &serde_json::json!({"type": "nope"}), &[]).unwrap();
        assert!(read_frame::<Up>(&mut bad.as_slice(), MAX_PART).is_err());
    }

    #[test]
    fn messages_keep_their_shape() {
        let want = Want { max_size: Some([960, 540]), ..Want::default() };
        let up = Up::Msg { msg: FrameMsg::Purge, want, urgent: true };
        let v = serde_json::to_value(&up).unwrap();
        assert_eq!(v["type"], "msg");
        assert_eq!(v["want"]["codec"], "jpeg");
        assert_eq!(v["want"]["maxSize"], serde_json::json!([960, 540]));
        // Defaults: a bare request is JPEG q85 with refine, not urgent.
        let bare: Up = serde_json::from_value(serde_json::json!({"type": "msg", "msg": {"type": "purge"}})).unwrap();
        assert!(matches!(bare, Up::Msg { urgent: false, want: Want { quality: 85, refine: true, .. }, .. }));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn backoff_doubles_to_its_cap() {
        let b = Backoff::default();
        assert_eq!(b.delay(0), std::time::Duration::from_millis(100));
        assert_eq!(b.delay(3), std::time::Duration::from_millis(800));
        assert_eq!(b.delay(10), b.max);
        assert_eq!(b.delay(u32::MAX), b.max);
    }
}
