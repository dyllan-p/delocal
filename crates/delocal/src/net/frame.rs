//! Framing (DESIGN.md §6.3): each message on a connection is a `u32`
//! big-endian length, then that many bytes of postcard.
//!
//! **The cap.** A length over [`MAX_FRAME`], 16 MiB, is refused as soon as
//! it is read, before anything is allocated for it, so a peer cannot make
//! this machine hold more than the cap by claiming a large frame. The body
//! is read into the [`Reader`]'s one buffer, which is sized to the length
//! once the length is known to be within the cap, and given back after a
//! frame larger than [`KEEP`]. Decoding the body allocates only for what
//! its bytes hold: postcard reads every length from the body and fails if
//! the bytes run out, and serde caps what it reserves for a sequence before
//! reading it at 1 MiB.
//!
//! **Silence.** A reader may be given a limit on silence: each read of the
//! socket must bring some bytes within it, or the reader stops with
//! [`FrameError::Idle`]. It is measured in bytes, not frames, so a large
//! frame arriving slowly over a relay is traffic all the while (§6.3).
//!
//! **Trailing bytes.** A body may hold more than the message it decodes
//! to. The rest is ignored, so that a message may gain optional fields at
//! its end (§12, [`super::wire`]).

use std::fmt;
use std::io;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::wire::Message;

/// The largest body a frame may have: 16 MiB (§6.3). File data is sent in
/// chunks well below it (§12), and a batch is split long before it (§7.4).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The largest buffer a [`Reader`] keeps between frames: room for a
/// `FileData` of 1 MiB with its fields (§12). A larger frame's buffer is
/// given back once it is decoded.
pub const KEEP: usize = 2 * 1024 * 1024;

/// The length prefix: a big-endian `u32`.
const HEADER: usize = 4;

/// Why a frame was not read, or not written.
#[derive(Debug)]
pub enum FrameError {
    /// The stream ended between frames: the other end closed.
    Closed,
    /// The stream ended inside a frame.
    Truncated,
    /// The length prefix was over [`MAX_FRAME`].
    TooLarge(u32),
    /// A message to send encodes to more than [`MAX_FRAME`].
    Oversize(usize),
    /// The body is not a message.
    Malformed(postcard::Error),
    /// No bytes arrived within the reader's limit on silence.
    Idle,
    Io(io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("the connection closed"),
            Self::Truncated => f.write_str("the connection closed inside a frame"),
            Self::TooLarge(len) => write!(f, "a frame of {len} bytes is over the cap"),
            Self::Oversize(len) => write!(f, "a message of {len} bytes is over the cap"),
            Self::Malformed(e) => write!(f, "a frame is not a message: {e}"),
            Self::Idle => f.write_str("the connection was silent too long"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// `message` as a frame: its length, then its postcard bytes.
pub fn encode(message: &Message) -> Result<Vec<u8>, FrameError> {
    let mut frame = postcard::to_extend(message, vec![0; HEADER]).map_err(FrameError::Malformed)?;
    let len = frame.len() - HEADER;
    if len > MAX_FRAME {
        return Err(FrameError::Oversize(len));
    }
    // `len` is at most MAX_FRAME, which fits a u32.
    let prefix = u32::try_from(len).map_err(|_| FrameError::Oversize(len))?;
    frame[..HEADER].copy_from_slice(&prefix.to_be_bytes());
    Ok(frame)
}

/// Write `message` to `out` as one frame.
pub async fn write<W: AsyncWrite + Unpin>(
    out: &mut W,
    message: &Message,
) -> Result<(), FrameError> {
    out.write_all(&encode(message)?).await?;
    Ok(())
}

/// Reads frames from a stream; see the module docs.
#[derive(Debug)]
pub struct Reader<R> {
    inner: R,
    buf: Vec<u8>,
    silence: Option<Duration>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    /// A reader with no limit on silence.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            silence: None,
        }
    }

    /// A reader that stops with [`FrameError::Idle`] once a read has
    /// waited `limit` without a byte.
    pub fn with_silence(inner: R, limit: Duration) -> Self {
        Self {
            silence: Some(limit),
            ..Self::new(inner)
        }
    }

    /// The next message. Not cancel safe: a frame read in part is lost.
    pub async fn read(&mut self) -> Result<Message, FrameError> {
        let mut header = [0; HEADER];
        let mut got = 0;
        while got < HEADER {
            match fill(&mut self.inner, self.silence, &mut header[got..]).await? {
                0 if got == 0 => return Err(FrameError::Closed),
                0 => return Err(FrameError::Truncated),
                n => got += n,
            }
        }
        let claimed = u32::from_be_bytes(header);
        let len = usize::try_from(claimed).map_err(|_| FrameError::TooLarge(claimed))?;
        if len > MAX_FRAME {
            return Err(FrameError::TooLarge(claimed));
        }
        self.buf.clear();
        // Exact, so the buffer never holds more than the largest frame.
        self.buf.reserve_exact(len);
        self.buf.resize(len, 0);
        let mut got = 0;
        while got < len {
            match fill(&mut self.inner, self.silence, &mut self.buf[got..]).await? {
                0 => return Err(FrameError::Truncated),
                n => got += n,
            }
        }
        let decoded =
            postcard::take_from_bytes::<Message>(&self.buf).map(|(message, _rest)| message);
        if self.buf.capacity() > KEEP {
            self.buf = Vec::new();
        }
        decoded.map_err(FrameError::Malformed)
    }

    /// The bytes this reader holds for frames now.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }
}

/// One read of `inner` into `buf`, within the limit on `silence`: the
/// bytes it brought, 0 at the end of the stream. A function rather than a
/// method, so that it can borrow the reader's stream while the caller
/// borrows its buffer.
async fn fill<R: AsyncRead + Unpin>(
    inner: &mut R,
    silence: Option<Duration>,
    buf: &mut [u8],
) -> Result<usize, FrameError> {
    let read = inner.read(buf);
    let n = match silence {
        Some(limit) => tokio::time::timeout(limit, read)
            .await
            .map_err(|_| FrameError::Idle)??,
        None => read.await?,
    };
    Ok(n)
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::net::wire::tests::message;
    use crate::net::wire::{Reason, RequestId};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    /// Read `bytes` as a stream of frames until the reader stops, checking
    /// after every frame that the reader holds no more than the cap.
    /// Returns the messages read and why it stopped.
    fn read_all(rt: &tokio::runtime::Runtime, bytes: &[u8]) -> (Vec<Message>, FrameError) {
        rt.block_on(async {
            let mut reader = Reader::new(bytes);
            let mut messages = Vec::new();
            loop {
                let read = reader.read().await;
                assert!(
                    reader.capacity() <= MAX_FRAME,
                    "the reader holds {} bytes",
                    reader.capacity()
                );
                match read {
                    Ok(message) => messages.push(message),
                    Err(e) => return (messages, e),
                }
            }
        })
    }

    /// Bytes for the decoder: anything at all; a length prefix of any size
    /// in front of anything, so lengths over the cap and lengths the bytes
    /// fall short of both come up; and anything framed with its own true
    /// length, which reaches postcard.
    fn noise() -> impl Strategy<Value = Vec<u8>> {
        let body = || vec(any::<u8>(), 0..256);
        prop_oneof![
            body(),
            (any::<u32>(), body()).prop_map(|(len, body)| {
                let mut bytes = len.to_be_bytes().to_vec();
                bytes.extend(body);
                bytes
            }),
            body().prop_map(|body| {
                let mut bytes = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
                bytes.extend(body);
                bytes
            }),
        ]
    }

    proptest! {
        /// Arbitrary bytes get an error, never a panic, and the reader
        /// never holds more than the cap. The bytes then end, so the
        /// stream as a whole always ends in an error; a frame that happens
        /// to decode on the way there is a message postcard could have
        /// written.
        #[test]
        fn arbitrary_bytes_get_an_error_without_a_panic_or_holding_past_the_cap(
            bytes in noise(),
        ) {
            let rt = runtime();
            let (messages, error) = read_all(&rt, &bytes);
            prop_assert!(
                matches!(
                    error,
                    FrameError::Closed
                        | FrameError::Truncated
                        | FrameError::TooLarge(_)
                        | FrameError::Malformed(_)
                ),
                "{error:?}"
            );
            for message in messages {
                prop_assert!(encode(&message).is_ok());
            }
        }

        /// Every message comes back through a frame as it went in,
        /// several to a stream, and the stream then ends cleanly.
        #[test]
        fn messages_round_trip_through_frames(messages in vec(message(), 0..8)) {
            let rt = runtime();
            let mut bytes = Vec::new();
            for message in &messages {
                bytes.extend(encode(message).unwrap());
            }
            let (read, error) = read_all(&rt, &bytes);
            prop_assert_eq!(read, messages);
            prop_assert!(matches!(error, FrameError::Closed), "{error:?}");
        }
    }

    /// A body of `len` bytes behind its length, not otherwise a message.
    fn frame_of(len: usize) -> Vec<u8> {
        let mut bytes = u32::try_from(len).unwrap().to_be_bytes().to_vec();
        bytes.resize(HEADER + len, 0xff);
        bytes
    }

    /// The cap is on the length prefix: a frame of exactly 16 MiB is read
    /// (and found not to be a message), one byte more is refused.
    #[test]
    fn the_cap_admits_16_mib_and_refuses_a_byte_more() {
        let rt = runtime();
        let (_, error) = read_all(&rt, &frame_of(MAX_FRAME));
        assert!(matches!(error, FrameError::Malformed(_)), "{error:?}");
        let mut over = (u32::try_from(MAX_FRAME).unwrap() + 1)
            .to_be_bytes()
            .to_vec();
        over.extend([0xff; 64]);
        let (_, error) = read_all(&rt, &over);
        assert!(
            matches!(error, FrameError::TooLarge(n) if n as usize == MAX_FRAME + 1),
            "{error:?}"
        );
    }

    /// A length over the cap is refused as soon as the prefix is in,
    /// without waiting for a body that will never come, and without
    /// allocating for it.
    #[tokio::test]
    async fn a_length_over_the_cap_is_refused_at_once() {
        let (mut peer, ours) = tokio::io::duplex(64);
        peer.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        let mut reader = Reader::new(ours);
        let read = tokio::time::timeout(Duration::from_secs(10), reader.read())
            .await
            .expect("the reader waited for a body");
        assert!(
            matches!(read, Err(FrameError::TooLarge(u32::MAX))),
            "{read:?}"
        );
        assert_eq!(reader.capacity(), 0);
        drop(peer);
    }

    /// The end of the stream between frames is `Closed`; inside the prefix
    /// or the body it is `Truncated`.
    #[test]
    fn where_the_stream_ends_says_how() {
        let rt = runtime();
        let ping = encode(&Message::Ping).unwrap();
        assert!(matches!(read_all(&rt, &[]).1, FrameError::Closed));
        assert!(matches!(read_all(&rt, &ping).1, FrameError::Closed));
        for cut in 1..ping.len() {
            let (read, error) = read_all(&rt, &ping[..cut]);
            assert!(read.is_empty());
            assert!(matches!(error, FrameError::Truncated), "{cut}: {error:?}");
        }
        let (read, error) = read_all(&rt, &frame_of(10)[..HEADER + 4]);
        assert!(read.is_empty());
        assert!(matches!(error, FrameError::Truncated), "{error:?}");
    }

    /// A body longer than its message is the message: the rest is a field
    /// this version does not know (§12).
    #[test]
    fn bytes_after_a_message_are_ignored() {
        let rt = runtime();
        let mut body = postcard::to_stdvec(&Message::Goodbye {
            reason: Reason::Duplicate,
        })
        .unwrap();
        body.extend([7, 7, 7]);
        let mut bytes = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
        bytes.extend(body);
        bytes.extend(encode(&Message::Ping).unwrap());
        let (read, error) = read_all(&rt, &bytes);
        assert_eq!(
            read,
            [
                Message::Goodbye {
                    reason: Reason::Duplicate
                },
                Message::Ping
            ]
        );
        assert!(matches!(error, FrameError::Closed));
    }

    /// A message over the cap is not sent; a `FileData` of 1 MiB, the
    /// chunk §12 sends, is.
    #[test]
    fn a_message_over_the_cap_is_refused_on_the_way_out() {
        let data = |len| Message::FileData {
            req: RequestId(1),
            offset: 0,
            bytes: vec![0; len],
        };
        assert!(encode(&data(1024 * 1024)).is_ok());
        assert!(matches!(
            encode(&data(MAX_FRAME)),
            Err(FrameError::Oversize(n)) if n > MAX_FRAME
        ));
    }

    /// A buffer grown for a frame larger than `KEEP` is given back once
    /// the frame is read; one within it is kept for the next.
    #[test]
    fn a_large_frames_buffer_is_given_back() {
        let rt = runtime();
        rt.block_on(async {
            let big = Message::FileData {
                req: RequestId(1),
                offset: 0,
                bytes: vec![1; KEEP],
            };
            let chunk = Message::FileData {
                req: RequestId(1),
                offset: 0,
                bytes: vec![1; 1024 * 1024],
            };
            let mut bytes = encode(&chunk).unwrap();
            bytes.extend(encode(&big).unwrap());
            let mut reader = Reader::new(bytes.as_slice());
            assert_eq!(reader.read().await.unwrap(), chunk);
            assert!(reader.capacity() > 1024 * 1024);
            assert_eq!(reader.read().await.unwrap(), big);
            assert_eq!(reader.capacity(), 0);
        });
    }

    /// With a limit on silence, a read that waits that long without a
    /// byte is `Idle`; bytes that keep coming, however slowly, are not.
    #[tokio::test(start_paused = true)]
    async fn silence_is_measured_in_bytes() {
        let limit = Duration::from_secs(45);
        let (mut peer, ours) = tokio::io::duplex(64);
        let mut reader = Reader::with_silence(ours, limit);
        let frame = encode(&Message::Ping).unwrap();
        let writer = tokio::spawn(async move {
            // One byte every 40 s: the frame takes 200 s, never silent
            // for 45.
            for byte in frame {
                tokio::time::sleep(Duration::from_secs(40)).await;
                peer.write_all(&[byte]).await.unwrap();
            }
            peer
        });
        let started = tokio::time::Instant::now();
        assert_eq!(reader.read().await.unwrap(), Message::Ping);
        assert_eq!(started.elapsed(), Duration::from_secs(200));
        let _peer = writer.await.unwrap();
        // Without the limit this read would wait forever.
        let read = tokio::time::timeout(Duration::from_secs(60), reader.read())
            .await
            .expect("still waiting after a minute of silence");
        assert!(matches!(read, Err(FrameError::Idle)), "{read:?}");
        assert_eq!(started.elapsed(), Duration::from_secs(245));
    }
}
