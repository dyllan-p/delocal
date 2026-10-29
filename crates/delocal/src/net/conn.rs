//! One connection (DESIGN.md §6.3): its handshake, then its life until it
//! ends. Generic over its two halves, so tests can run it over an
//! in-memory pipe on a paused clock.
//!
//! **The handshake** ([`handshake`]). Each end sends `Hello` first and
//! reads the other's. Another protocol version is refused with
//! `Goodbye { Protocol }`; so is anything but `Hello` first, without the
//! `Goodbye`; and so is this machine itself, which a dial to its own
//! address would reach.
//!
//! **Its life** ([`run`]). The connection reads and writes at once. It
//! answers each `Ping` with a `Pong`, and passes every other message on.
//! It pings every [`PING`], 15 s, whatever else it sends, and a connection
//! that brings no bytes for [`SILENCE`], 45 s, has lost the other end and
//! ends. Since the other end pings as often, only a machine that is gone,
//! or a path that is, is ever that silent.
//!
//! **Goodbye.** An end that closes with `Goodbye` shuts its half and then
//! reads on, for up to [`LINGER`], until the other end closes too. A
//! socket closed with bytes still unread resets the connection, and a
//! reset can lose the `Goodbye` before the other end reads it.

use std::fmt;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};

use super::frame::{self, FrameError, Reader};
use super::wire::{Hello, Message, Reason};

/// How often a connection pings (§6.3).
pub const PING: Duration = Duration::from_secs(15);

/// How long a connection may bring no bytes before it is dropped (§6.3).
pub const SILENCE: Duration = Duration::from_secs(45);

/// How long an end that said `Goodbye` waits for the other to close.
pub const LINGER: Duration = Duration::from_secs(5);

/// Something for a connection to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Message(Message),
    /// Say `Goodbye` and close.
    Goodbye(Reason),
}

/// Why a handshake failed.
#[derive(Debug)]
pub enum Refused {
    /// The other end speaks another protocol version, and was told so
    /// with `Goodbye`.
    Protocol(Hello),
    /// The other end said `Goodbye` instead of `Hello`.
    Goodbye(Reason),
    /// The other end's first message was not `Hello`.
    NotHello,
    /// The other end is this machine.
    Myself,
    Frame(FrameError),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Protocol(theirs) => write!(
                f,
                "{} speaks protocol {} (delocal {})",
                theirs.hostname.as_str(),
                theirs.proto,
                theirs.version
            ),
            Self::Goodbye(reason) => write!(f, "the other end said goodbye: {reason:?}"),
            Self::NotHello => f.write_str("the other end did not begin with hello"),
            Self::Myself => f.write_str("the other end is this machine"),
            Self::Frame(e) => write!(f, "{e}"),
        }
    }
}

/// How a connection ended.
#[derive(Debug)]
pub enum End {
    /// The other end said `Goodbye`.
    TheySaid(Reason),
    /// This end said `Goodbye`.
    WeSaid(Reason),
    /// No bytes came for [`SILENCE`].
    Silent,
    /// The other end sent `Hello` again.
    SecondHello,
    /// The other end closed, or the connection failed.
    Lost(FrameError),
    /// Whoever took this end's messages is gone.
    Dropped,
}

/// Send `hello` and read the other end's. See the module docs.
pub async fn handshake<R, W>(
    reader: &mut Reader<R>,
    writer: &mut W,
    hello: &Hello,
) -> Result<Hello, Refused>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    frame::write(writer, &Message::Hello(hello.clone()))
        .await
        .map_err(Refused::Frame)?;
    let theirs = match reader.read().await.map_err(Refused::Frame)? {
        Message::Hello(theirs) => theirs,
        Message::Goodbye { reason } => return Err(Refused::Goodbye(reason)),
        _ => return Err(Refused::NotHello),
    };
    if theirs.proto != hello.proto {
        farewell(reader, writer, Reason::Protocol).await;
        return Err(Refused::Protocol(theirs));
    }
    if theirs.node == hello.node {
        return Err(Refused::Myself);
    }
    Ok(theirs)
}

/// Say `Goodbye`, shut this half, and read whatever comes until the other
/// end closes, for up to [`LINGER`].
async fn farewell<R, W>(reader: &mut Reader<R>, writer: &mut W, reason: Reason)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // The connection is ending either way: a failure here only means the
    // other end will not hear why.
    let _ = frame::write(writer, &Message::Goodbye { reason }).await;
    let _ = writer.shutdown().await;
    let _ = tokio::time::timeout(LINGER, async { while reader.read().await.is_ok() {} }).await;
}

/// Run a connection whose handshake is done, until it ends. It sends what
/// arrives on `out`, and `outs` is a sender to the same queue, for its
/// `Pong`s. Every message it receives but `Ping`, `Pong` and `Goodbye`
/// goes to `to`, as `wrap` makes it.
pub async fn run<R, W, T>(
    reader: Reader<R>,
    writer: W,
    outs: mpsc::UnboundedSender<Out>,
    out: mpsc::UnboundedReceiver<Out>,
    to: &mpsc::Sender<T>,
    wrap: impl Fn(Message) -> T,
) -> End
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let reading = read(reader, outs, to, wrap);
    let writing = write(writer, out);
    tokio::pin!(reading);
    let end = tokio::select! {
        end = &mut reading => end,
        end = writing => end,
    };
    if let End::WeSaid(_) = end {
        let _ = tokio::time::timeout(LINGER, &mut reading).await;
    }
    end
}

/// The reading half of [`run`].
async fn read<R, T>(
    mut reader: Reader<R>,
    outs: mpsc::UnboundedSender<Out>,
    to: &mpsc::Sender<T>,
    wrap: impl Fn(Message) -> T,
) -> End
where
    R: AsyncRead + Unpin,
{
    loop {
        let message = match reader.read().await {
            Ok(message) => message,
            Err(FrameError::Idle) => return End::Silent,
            Err(e) => return End::Lost(e),
        };
        match message {
            Message::Ping => {
                // Only fails once the writing half is gone, and with it
                // the connection.
                let _ = outs.send(Out::Message(Message::Pong));
            }
            Message::Pong => {}
            Message::Goodbye { reason } => return End::TheySaid(reason),
            Message::Hello(_) => return End::SecondHello,
            message => {
                if to.send(wrap(message)).await.is_err() {
                    return End::Dropped;
                }
            }
        }
    }
}

/// The writing half of [`run`].
async fn write<W>(mut writer: W, mut out: mpsc::UnboundedReceiver<Out>) -> End
where
    W: AsyncWrite + Unpin,
{
    let mut ping = tokio::time::interval_at(Instant::now() + PING, PING);
    // A ping that could not be sent on time is sent once, late, rather
    // than several at once.
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let next = tokio::select! {
            next = out.recv() => match next {
                Some(next) => next,
                None => return End::Dropped,
            },
            _ = ping.tick() => Out::Message(Message::Ping),
        };
        let message = match next {
            Out::Message(message) => message,
            Out::Goodbye(reason) => {
                // As in `farewell`: the connection ends either way.
                let _ = frame::write(&mut writer, &Message::Goodbye { reason }).await;
                let _ = writer.shutdown().await;
                return End::WeSaid(reason);
            }
        };
        match frame::write(&mut writer, &message).await {
            Ok(()) => {}
            // A message too large to frame is the sender's bug, not the
            // connection's: it is dropped, and the connection goes on.
            Err(FrameError::Oversize(len)) => {
                tracing::error!(len, "a message over the frame cap was not sent");
            }
            Err(e) => return End::Lost(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use delocal_engine::HostName;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    use super::*;
    use crate::net::wire::PROTOCOL;
    use crate::store::sample;

    fn hello(n: u8) -> Hello {
        Hello {
            proto: PROTOCOL,
            node: sample::node(n),
            hostname: HostName::new(format!("m{n}")).unwrap(),
            version: "0.1.0".to_owned(),
            now: sample::at(0),
        }
    }

    type Half = (Reader<ReadHalf<DuplexStream>>, WriteHalf<DuplexStream>);

    /// Two ends of an in-memory connection, each with a reader that drops
    /// it after 45 s of silence.
    fn pipe() -> (Half, Half) {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        (
            (Reader::with_silence(a_read, SILENCE), a_write),
            (Reader::with_silence(b_read, SILENCE), b_write),
        )
    }

    /// One end, with its handshake done, run on a task of its own: what
    /// it ends with, what it passes on, and a sender to its queue.
    struct Running {
        end: tokio::task::JoinHandle<End>,
        received: mpsc::Receiver<Message>,
        out: mpsc::UnboundedSender<Out>,
    }

    fn start((reader, writer): Half) -> Running {
        let (outs, out) = mpsc::unbounded_channel();
        let (to, received) = mpsc::channel(16);
        let queue = outs.clone();
        let end = tokio::spawn(async move { run(reader, writer, queue, out, &to, |m| m).await });
        Running {
            end,
            received,
            out: outs,
        }
    }

    /// Both ends' handshakes, at once: ours runs from then on, the other
    /// is the test's to drive by hand.
    async fn connected() -> (Running, Half) {
        let ((mut a_read, mut a_write), (mut b_read, mut b_write)) = pipe();
        let (a, b) = (hello(1), hello(2));
        let (ours, theirs) = tokio::join!(
            handshake(&mut a_read, &mut a_write, &a),
            handshake(&mut b_read, &mut b_write, &b),
        );
        assert_eq!(ours.unwrap(), hello(2));
        assert_eq!(theirs.unwrap(), hello(1));
        (start((a_read, a_write)), (b_read, b_write))
    }

    /// §6.3: a connection that brings no bytes for 45 s is dropped, and
    /// not a moment before. It pinged all the while; the other end never
    /// answered.
    #[tokio::test(start_paused = true)]
    async fn forty_five_seconds_of_silence_drop_the_connection() {
        let (ours, (mut their_read, _their_write)) = connected().await;
        let started = Instant::now();
        // On the paused clock a connection that is never dropped would
        // ping on forever; a minute is long enough to say it was not.
        let end = tokio::time::timeout(Duration::from_secs(60), ours.end)
            .await
            .expect("still up after a minute of silence")
            .unwrap();
        assert!(matches!(end, End::Silent), "{end:?}");
        assert_eq!(started.elapsed(), Duration::from_secs(45));
        // It pinged at 15 s and 30 s, and at 45 s if that tick won the
        // race with the silence.
        let mut pings = 0;
        while let Ok(message) = their_read.read().await {
            assert_eq!(message, Message::Ping);
            pings += 1;
        }
        assert!((2..=3).contains(&pings), "{pings} pings");
    }

    /// Silence is measured from the last bytes, not from the start: an
    /// end that answers every ping keeps the connection for as long as it
    /// likes, here 10 minutes, and its `Goodbye` then ends it.
    #[tokio::test(start_paused = true)]
    async fn an_end_that_answers_pings_is_never_silent() {
        let (ours, (mut their_read, mut their_write)) = connected().await;
        let started = Instant::now();
        let mut pongs = 0;
        while started.elapsed() < Duration::from_secs(600) {
            assert_eq!(their_read.read().await.unwrap(), Message::Ping);
            frame::write(&mut their_write, &Message::Pong)
                .await
                .unwrap();
            pongs += 1;
        }
        assert_eq!(pongs, 40);
        assert!(!ours.end.is_finished());
        frame::write(
            &mut their_write,
            &Message::Goodbye {
                reason: Reason::Duplicate,
            },
        )
        .await
        .unwrap();
        let end = ours.end.await.unwrap();
        assert!(matches!(end, End::TheySaid(Reason::Duplicate)), "{end:?}");
    }

    /// A ping is answered with a pong; other messages pass on, both ways.
    #[tokio::test(start_paused = true)]
    async fn pings_are_answered_and_messages_pass_on() {
        let (mut ours, (mut their_read, mut their_write)) = connected().await;
        frame::write(&mut their_write, &Message::Ping)
            .await
            .unwrap();
        assert_eq!(their_read.read().await.unwrap(), Message::Pong);

        let data = Message::FileDone {
            req: crate::net::wire::RequestId(3),
        };
        frame::write(&mut their_write, &data).await.unwrap();
        assert_eq!(ours.received.recv().await.unwrap(), data);

        let back = Message::NotAvailable {
            req: crate::net::wire::RequestId(3),
        };
        ours.out.send(Out::Message(back.clone())).unwrap();
        assert_eq!(their_read.read().await.unwrap(), back);
        // Neither the ping nor the pong was passed on.
        assert!(ours.received.try_recv().is_err());
    }

    /// §6.3: another protocol version is refused with `Goodbye`. The end
    /// that refuses sends its own `Hello` first, then `Goodbye`, then
    /// closes; and the refusal names the other end's version.
    #[tokio::test(start_paused = true)]
    async fn a_version_mismatch_gives_goodbye() {
        let ((mut our_read, mut our_write), (mut their_read, mut their_write)) = pipe();
        let newer = Hello {
            proto: PROTOCOL + 1,
            version: "9.0.0".to_owned(),
            ..hello(2)
        };
        frame::write(&mut their_write, &Message::Hello(newer.clone()))
            .await
            .unwrap();
        let refused = handshake(&mut our_read, &mut our_write, &hello(1)).await;
        assert!(
            matches!(&refused, Err(Refused::Protocol(theirs)) if *theirs == newer),
            "{refused:?}"
        );
        assert_eq!(their_read.read().await.unwrap(), Message::Hello(hello(1)));
        assert_eq!(
            their_read.read().await.unwrap(),
            Message::Goodbye {
                reason: Reason::Protocol
            }
        );
        assert!(matches!(their_read.read().await, Err(FrameError::Closed)));
    }

    /// Anything but `Hello` first is refused, and so is this machine
    /// itself.
    #[tokio::test(start_paused = true)]
    async fn only_another_machines_hello_begins_a_connection() {
        let ((mut our_read, mut our_write), (_, mut their_write)) = pipe();
        frame::write(&mut their_write, &Message::Ping)
            .await
            .unwrap();
        let refused = handshake(&mut our_read, &mut our_write, &hello(1)).await;
        assert!(matches!(refused, Err(Refused::NotHello)), "{refused:?}");

        let ((mut our_read, mut our_write), (_, mut their_write)) = pipe();
        frame::write(
            &mut their_write,
            &Message::Goodbye {
                reason: Reason::Protocol,
            },
        )
        .await
        .unwrap();
        let refused = handshake(&mut our_read, &mut our_write, &hello(1)).await;
        assert!(
            matches!(refused, Err(Refused::Goodbye(Reason::Protocol))),
            "{refused:?}"
        );

        let ((mut our_read, mut our_write), (_, mut their_write)) = pipe();
        frame::write(&mut their_write, &Message::Hello(hello(1)))
            .await
            .unwrap();
        let refused = handshake(&mut our_read, &mut our_write, &hello(1)).await;
        assert!(matches!(refused, Err(Refused::Myself)), "{refused:?}");
    }

    /// An end told to say `Goodbye` says it, shuts its half, and waits for
    /// the other end to close: at once if it does, after `LINGER` if it
    /// does not.
    #[tokio::test(start_paused = true)]
    async fn goodbye_waits_for_the_other_end_to_close() {
        let (ours, (mut their_read, their_write)) = connected().await;
        let started = Instant::now();
        ours.out.send(Out::Goodbye(Reason::Duplicate)).unwrap();
        assert_eq!(
            their_read.read().await.unwrap(),
            Message::Goodbye {
                reason: Reason::Duplicate
            }
        );
        assert!(matches!(their_read.read().await, Err(FrameError::Closed)));
        drop((their_read, their_write));
        let end = ours.end.await.unwrap();
        assert!(matches!(end, End::WeSaid(Reason::Duplicate)), "{end:?}");
        assert_eq!(started.elapsed(), Duration::ZERO);

        let (ours, _theirs) = connected().await;
        let started = Instant::now();
        ours.out.send(Out::Goodbye(Reason::Duplicate)).unwrap();
        let end = ours.end.await.unwrap();
        assert!(matches!(end, End::WeSaid(Reason::Duplicate)), "{end:?}");
        assert_eq!(started.elapsed(), LINGER);
    }

    /// A second `Hello` ends the connection.
    #[tokio::test(start_paused = true)]
    async fn a_second_hello_ends_the_connection() {
        let (ours, (_their_read, mut their_write)) = connected().await;
        frame::write(&mut their_write, &Message::Hello(hello(2)))
            .await
            .unwrap();
        let end = ours.end.await.unwrap();
        assert!(matches!(end, End::SecondHello), "{end:?}");
    }
}
