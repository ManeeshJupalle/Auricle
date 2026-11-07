//! A listen-only client for the engine's `/ws/live`: just enough of RFC 6455
//! to read the server's text frames. The engine never pings and ignores what
//! clients send, so nothing is ever written back.

use futures_util::future::{AbortRegistration, Abortable};
use reqwest::header::{CONNECTION, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE};
use tokio::io::AsyncReadExt;

/// Larger than any event the engine sends; a length past it means the
/// stream is not what it claims to be.
const MAX_MESSAGE: usize = 16 << 20;

/// What a run of bytes from the server amounted to.
#[derive(Debug, PartialEq)]
pub enum Read {
    Text(String),
    Close,
}

/// Reassembles frames that can arrive split across reads in any place.
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
    /// The text message being assembled from fragments, if one is.
    text: Option<Vec<u8>>,
}

impl Frames {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Read> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some((fin, opcode, payload, used)) = self.frame() else {
                return out;
            };
            self.buf.drain(..used);
            match opcode {
                0x8 => {
                    out.push(Read::Close);
                    return out;
                }
                0x1 => self.text = Some(payload),
                0x0 => {
                    if let Some(text) = &mut self.text {
                        text.extend_from_slice(&payload);
                    }
                }
                // Binary, ping and pong carry nothing for a listener.
                _ => continue,
            }
            if fin {
                if let Some(text) = self.text.take() {
                    out.push(Read::Text(String::from_utf8_lossy(&text).into_owned()));
                }
            }
            if self.text.as_ref().is_some_and(|t| t.len() > MAX_MESSAGE) {
                out.push(Read::Close);
                return out;
            }
        }
    }

    /// The first whole frame in the buffer: (fin, opcode, payload, bytes used).
    /// An impossible length reads as a close frame, so the caller stops.
    fn frame(&self) -> Option<(bool, u8, Vec<u8>, usize)> {
        let b = &self.buf;
        if b.len() < 2 {
            return None;
        }
        let (fin, opcode, masked) = (b[0] & 0x80 != 0, b[0] & 0x0f, b[1] & 0x80 != 0);
        let (len, mut at) = match b[1] & 0x7f {
            126 => (u16::from_be_bytes(b.get(2..4)?.try_into().ok()?) as u64, 4),
            127 => (u64::from_be_bytes(b.get(2..10)?.try_into().ok()?), 10),
            n => (n as u64, 2),
        };
        if len > MAX_MESSAGE as u64 {
            return Some((true, 0x8, Vec::new(), 0));
        }
        let mask: Option<[u8; 4]> = if masked {
            let key = b.get(at..at + 4)?.try_into().ok()?;
            at += 4;
            Some(key)
        } else {
            None
        };
        let end = at + len as usize;
        let mut payload = b.get(at..end)?.to_vec();
        if let Some(key) = mask {
            payload
                .iter_mut()
                .enumerate()
                .for_each(|(i, byte)| *byte ^= key[i % 4]);
        }
        Some((fin, opcode, payload, end))
    }
}

/// Connect to `/ws/live` and hand each JSON event to `on_event` until the
/// engine closes the stream, it breaks, or `stop` aborts it.
pub async fn listen(
    http: &reqwest::Client,
    base_url: &str,
    stop: AbortRegistration,
    on_event: impl FnMut(serde_json::Value),
) -> Result<(), String> {
    Abortable::new(read_events(http, base_url, on_event), stop)
        .await
        .unwrap_or(Ok(()))
}

async fn read_events(
    http: &reqwest::Client,
    base_url: &str,
    mut on_event: impl FnMut(serde_json::Value),
) -> Result<(), String> {
    let resp = http
        .get(format!("{base_url}/ws/live"))
        .header(CONNECTION, "upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_VERSION, "13")
        // The RFC's sample nonce. A random one guards against caching
        // proxies, and there are none on loopback.
        .header(SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .map_err(|e| format!("engine unreachable: {e}"))?;
    if resp.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!(
            "the engine refused the event stream ({})",
            resp.status()
        ));
    }
    let mut io = resp.upgrade().await.map_err(|e| e.to_string())?;
    let mut frames = Frames::default();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = io.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        for read in frames.push(&buf[..n]) {
            match read {
                Read::Text(text) => {
                    if let Ok(event) = serde_json::from_str(&text) {
                        on_event(event);
                    }
                }
                Read::Close => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::AbortHandle;
    use std::io::{Read as _, Write};
    use std::time::Duration;

    fn text_frame(text: &str) -> Vec<u8> {
        let mut f = vec![0x81];
        let len = text.len();
        if len < 126 {
            f.push(len as u8);
        } else {
            f.push(126);
            f.extend_from_slice(&(len as u16).to_be_bytes());
        }
        f.extend_from_slice(text.as_bytes());
        f
    }

    #[test]
    fn a_frame_split_at_every_byte_still_reads_whole() {
        let frame = text_frame(r#"{"type":"dictation_stopped","id":"d1"}"#);
        for split in 1..frame.len() {
            let mut frames = Frames::default();
            let mut out = frames.push(&frame[..split]);
            out.extend(frames.push(&frame[split..]));
            assert_eq!(
                out,
                vec![Read::Text(
                    r#"{"type":"dictation_stopped","id":"d1"}"#.into()
                )],
                "split {split}"
            );
        }
    }

    #[test]
    fn a_long_frame_uses_the_two_byte_length() {
        let long = "x".repeat(300);
        let mut frames = Frames::default();
        assert_eq!(frames.push(&text_frame(&long)), vec![Read::Text(long)]);
    }

    #[test]
    fn fragments_join_and_pings_in_between_are_skipped() {
        let mut bytes = vec![0x01, 3];
        bytes.extend_from_slice(b"hel");
        bytes.extend_from_slice(&[0x89, 0]);
        bytes.extend_from_slice(&[0x80, 2]);
        bytes.extend_from_slice(b"lo");
        let mut frames = Frames::default();
        assert_eq!(frames.push(&bytes), vec![Read::Text("hello".into())]);
    }

    #[test]
    fn a_masked_frame_is_unmasked() {
        let key = [1u8, 2, 3, 4];
        let mut bytes = vec![0x81, 0x80 | 2];
        bytes.extend_from_slice(&key);
        bytes.extend_from_slice(&[b'o' ^ 1, b'k' ^ 2]);
        let mut frames = Frames::default();
        assert_eq!(frames.push(&bytes), vec![Read::Text("ok".into())]);
    }

    #[test]
    fn close_ends_the_stream_and_an_absurd_length_counts_as_one() {
        let mut frames = Frames::default();
        assert_eq!(frames.push(&[0x88, 0]), vec![Read::Close]);
        let mut huge = vec![0x81, 127];
        huge.extend_from_slice(&u64::MAX.to_be_bytes());
        let mut frames = Frames::default();
        assert_eq!(frames.push(&huge), vec![Read::Close]);
    }

    /// An engine whose /ws/live accepts the upgrade and sends `frames`.
    fn fake_ws(frames: Vec<u8>, hold_open: bool) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = [0u8; 2048];
            let n = s.read(&mut req).unwrap();
            let head = String::from_utf8_lossy(&req[..n]).to_ascii_lowercase();
            assert!(head.starts_with("get /ws/live"), "{head}");
            assert!(head.contains("upgrade: websocket"), "{head}");
            s.write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nconnection: upgrade\r\nupgrade: websocket\r\n\
                  sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n",
            )
            .unwrap();
            s.write_all(&frames).unwrap();
            while hold_open && s.write_all(&[0x8a, 0]).is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        base
    }

    #[tokio::test]
    async fn events_arrive_until_the_engine_closes() {
        let mut bytes = text_frame(r#"{"type":"vu","rms":0.1}"#);
        bytes.extend(text_frame(r#"{"type":"dictation_stopped","id":"d7"}"#));
        bytes.extend_from_slice(&[0x88, 0]);
        let base = fake_ws(bytes, false);
        let mut seen = Vec::new();
        let (_handle, stop) = AbortHandle::new_pair();
        let http = reqwest::Client::new();
        listen(&http, &base, stop, |v| seen.push(v["type"].to_string()))
            .await
            .unwrap();
        assert_eq!(seen, vec!["\"vu\"", "\"dictation_stopped\""]);
    }

    #[tokio::test]
    async fn stopping_a_listen_ends_it_at_once() {
        let base = fake_ws(Vec::new(), true);
        let (handle, stop) = AbortHandle::new_pair();
        let http = reqwest::Client::new();
        let run = tokio::spawn(async move { listen(&http, &base, stop, |_| {}).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let ended = tokio::time::timeout(Duration::from_secs(5), run).await;
        assert_eq!(ended.expect("ends once stopped").unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_refused_upgrade_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0u8; 2048]);
            let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n");
        });
        let (_handle, stop) = AbortHandle::new_pair();
        let http = reqwest::Client::new();
        let err = listen(&http, &base, stop, |_| {}).await.unwrap_err();
        assert!(err.contains("403"), "{err}");
    }
}
