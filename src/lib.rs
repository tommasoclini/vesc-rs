//! The VESC® firmware is an open source motor controller firmware, and this
//! library implements the necessary structures and functions to [`encode`]
//! commands and [`decode`] replies.
//!
//! # Examples
//!
//! ## Encoding a Command
//!
//! ```no_run
//! use vesc::{Command, ValuesMask};
//!
//! let mut buf = [0u8; 64];
//! let command = Command::GetValuesSelective(ValuesMask::RPM | ValuesMask::VOLTAGE_IN);
//! let frame_len = vesc::encode(command, &mut buf).unwrap();
//! let frame = &buf[..frame_len];
//! ```
//!
//! ## Decoding a Reply
//!
//! ```no_run
//! use vesc::CommandReply;
//!
//! match vesc::decode(&[2, 7, 50, 0, 0, 1, 128, 0, 0, 4, 210, 1, 176, 254, 22, 3]) {
//!     Ok((_, CommandReply::GetValuesSelective(values))) => {
//!         let rpm = values.rpm;
//!         let voltage_in = values.voltage_in;
//!     },
//!     _ => (),
//! }
//! ```
//!
//! ## Decoding a Stream
//!
//! ```no_run
//! use vesc::{Decoder, CommandReply};
//!
//! let mut decoder = Decoder::default();
//! decoder.feed(&[2, 7, 50, 0, 0, 1, 128, 0, 0, 4, 210, 1, 176, 254, 22, 3]);
//!
//! for reply in decoder.by_ref() {
//!     match reply {
//!         CommandReply::GetValuesSelective(values) => {
//!             let rpm = values.rpm;
//!             let voltage_in = values.voltage_in;
//!         },
//!         _ => (),
//!     }
//! }
//! ```
#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::missing_errors_doc)]

mod command;
mod decoder;
mod packer;

pub use command::{
    //
    Command,
    CommandId,
    CommandReply,
    DecodeError,
    EncodeError,
    FaultCode,
    FwInfo,
    FwVersion,
    HwType,
    NrfFlags,
    QmlAppFlags,
    QmlHw,
    SetupValues,
    Stats,
    StatsMask,
    Values,
    ValuesMask,
    ValuesSetupMask,
    decode,
    encode,
};
pub use decoder::Decoder;

use core::{pin::pin, task::Poll};

use embassy_sync::{
    blocking_mutex::raw::{NoopRawMutex, RawMutex},
    signal::Signal,
};
use maitake_sync::Mutex;
use mutex::{ConstInit, ScopedRawMutex};
use pinlist::blocking::{Node, PinList};

use embassy_futures::select::{Either, select};

use embedded_io_async::{BufRead, Write};
use thiserror::Error;

struct Subscriber<EM: RawMutex> {
    reply: Option<Signal<EM, CommandReply>>,
    id: CommandId,
}

impl<EM: RawMutex> Subscriber<EM> {
    pub fn new(id: CommandId) -> Self {
        Self {
            reply: Some(Signal::new()),
            id,
        }
    }
}

pub struct Vesc<W: Write, R: BufRead, M: ScopedRawMutex> {
    tx: Mutex<Tx<W>>,
    rx: Mutex<Rx<R>>,

    subscribers: PinList<M, Subscriber<NoopRawMutex>>,
}

impl<W: Write, R: BufRead, M: ScopedRawMutex + ConstInit> Vesc<W, R, M> {
    pub const fn new(rx: R, tx: W) -> Self {
        Self {
            tx: Mutex::new(Tx::new(tx)),
            rx: Mutex::new(Rx::new(rx)),
            subscribers: PinList::new(),
        }
    }

    /// sends command and does not wait for a reply
    pub async fn command(&self, cmd: Command<'_>) -> Result<(), VescError<W::Error, R::Error>> {
        self.tx
            .lock()
            .await
            .command(cmd)
            .await
            .map_err(VescError::Tx)
    }

    /// waits for a reply with provided id
    ///
    /// # Panics
    pub async fn wait_for_reply(
        &self,
        id: CommandId,
    ) -> Result<CommandReply, VescError<W::Error, R::Error>> {
        let sub = pin!(Node::new_for(&self.subscribers, Subscriber::new(id)));
        let hdl = sub.attach();

        match select(
            core::future::poll_fn(|cx| {
                hdl.with_lock_mut(|s| {
                    let reply = s
                        .reply
                        .as_mut()
                        .expect("this future must not be polled after it returned Ready");
                    let r = core::task::ready!(pin!(reply.wait()).poll(cx));
                    s.reply = None;
                    Poll::Ready(r)
                })
            }),
            async {
                let mut rx = self.rx.lock().await;

                loop {
                    let reply = rx.wait_for_reply().await?;
                    self.subscribers.with_iter(|subs| {
                        for sub in subs {
                            if sub.id == reply.id()
                                && let Some(sig) = sub.reply.as_ref()
                                && !sig.signaled()
                            {
                                sig.signal(reply);
                                break;
                            }
                        }
                    });
                }
            },
        )
        .await
        {
            Either::First(reply) => Ok(reply),
            Either::Second(err) => err,
        }
    }

    /// command with single reply
    ///
    /// # Arguments
    ///
    /// * `timeout_ms` - if set to None, it will use the same id as the sent
    ///   command, but another id can be chosen.
    pub async fn command_with_reply(
        &self,
        cmd: Command<'_>,
        reply_id: Option<CommandId>,
    ) -> Result<CommandReply, VescError<W::Error, R::Error>> {
        self.command(cmd).await?;
        self.wait_for_reply(reply_id.unwrap_or_else(|| cmd.id()))
            .await
    }
}

#[derive(Error, Debug)]
pub enum VescError<TxEio, RxEio> {
    Other,
    Tx(#[from] TxError<TxEio>),
    Rx(#[from] RxError<RxEio>),
}

struct Tx<W: Write> {
    tx: W,
    buf: [u8; 518],
}

#[derive(Error, Debug)]
pub enum TxError<EioErr> {
    Other,
    Encoding(#[from] EncodeError),
    Eio(EioErr),
}

impl<W: Write> Tx<W> {
    const fn new(tx: W) -> Self {
        Self { tx, buf: [0; 518] }
    }

    async fn command(&mut self, cmd: Command<'_>) -> Result<(), TxError<W::Error>> {
        let s = encode(cmd, &mut self.buf)?;
        self.tx
            .write_all(&self.buf[..s])
            .await
            .map_err(TxError::Eio)?;
        Ok(())
    }
}

struct Rx<W: BufRead> {
    rx: W,
    decoder: Decoder<518>,
}

#[derive(Error, Debug)]
pub enum RxError<EioErr> {
    Other,
    Decoding(#[from] DecodeError),
    Eio(EioErr),
    Eof,
}

impl<R: BufRead> Rx<R> {
    const fn new(rx: R) -> Self {
        Self {
            rx,
            decoder: Decoder::new(),
        }
    }

    async fn wait_for_reply(&mut self) -> Result<CommandReply, RxError<R::Error>> {
        loop {
            let data = self.rx.fill_buf().await.map_err(RxError::Eio)?;
            if data.is_empty() {
                break Err(RxError::Eof);
            }
            let size = self.decoder.feed(data)?;
            self.rx.consume(size);
            if let Some(r) = self.decoder.next() {
                break Ok(r);
            }
        }
    }
}
