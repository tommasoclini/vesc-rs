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

mod command;
mod decoder;
mod packer;

pub use command::{
    //
    Command,
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

use core::num::NonZeroU32;
use embedded_hal_async::delay::DelayNs;
use embedded_io_async::{BufRead, Write};
use thiserror::Error;

pub struct Vesc<W: Write, R: BufRead> {
    tx: W,
    tx_timeout_ms: Option<NonZeroU32>,
    rx: R,
    rx_timeout_ms: Option<NonZeroU32>,
    out_buf: [u8; 518],
    decoder: Decoder<518>,
}

#[derive(Error, Debug)]
pub enum VescError<TxErr, RxErr> {
    Other,
    Encoding(#[from] EncodeError),
    Decoding(#[from] DecodeError),
    Tx(TxErr),
    TxTimeout,
    Rx(RxErr),
    RxTimeout,
    RxEof,
}

impl<W: Write, R: BufRead> Vesc<W, R> {
    pub const fn new(
        tx: W,
        rx: R,
        rx_timeout_ms: Option<NonZeroU32>,
        tx_timeout_ms: Option<NonZeroU32>,
    ) -> Self {
        Self {
            tx,
            rx,
            out_buf: [0; 518],
            decoder: Decoder::new(),
            tx_timeout_ms,
            rx_timeout_ms,
        }
    }

    pub fn set_timeouts(
        &mut self,
        rx_timeout_ms: Option<NonZeroU32>,
        tx_timeout_ms: Option<NonZeroU32>,
    ) {
        self.rx_timeout_ms = rx_timeout_ms;
        self.tx_timeout_ms = tx_timeout_ms;
    }

    #[allow(clippy::missing_errors_doc)]
    pub async fn command(
        &mut self,
        cmd: Command<'_>,
        mut delay: Option<impl DelayNs>,
    ) -> Result<Option<CommandReply>, VescError<W::Error, R::Error>> {
        let s = encode(cmd, &mut self.out_buf)?;

        let tx_fut = self.tx.write_all(&self.out_buf[..s]);

        (if let Some(delay) = &mut delay
            && let Some(tx_timeout) = self.tx_timeout_ms
        {
            with_timeout_ms(delay, tx_timeout.get(), tx_fut)
                .await
                .ok_or(VescError::TxTimeout)?
        } else {
            tx_fut.await
        })
        .map_err(VescError::Tx)?;

        if cmd.has_reply() {
            let rx_fut = async {
                loop {
                    let data = self.rx.fill_buf().await.map_err(VescError::Rx)?;
                    if data.is_empty() {
                        break Err(VescError::RxEof);
                    }
                    let size = self.decoder.feed(data)?;
                    self.rx.consume(size);
                    if let Some(r) = self.decoder.next() {
                        break Ok(Some(r));
                    }
                }
            };

            if let Some(delay) = &mut delay
                && let Some(rx_timeout) = self.rx_timeout_ms
            {
                with_timeout_ms(delay, rx_timeout.get(), rx_fut)
                    .await
                    .ok_or(VescError::RxTimeout)?
            } else {
                rx_fut.await
            }
        } else {
            Ok(None)
        }
    }
}

async fn with_timeout_ms<T, F: Future<Output = T>>(
    mut delay: impl DelayNs,
    timeout_ms: u32,
    fut: F,
) -> Option<T> {
    use embassy_futures::select::Either;

    match embassy_futures::select::select(delay.delay_ms(timeout_ms), fut).await {
        Either::Second(out) => Some(out),
        Either::First(()) => None,
    }
}
