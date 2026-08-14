//! Live-execution [`Wallet`](crate::Wallet) implementations — downstream
//! wallets that route order flow to a real broker instead of an in-memory paper
//! book, so a [`Strategy`](crate::Strategy) driven by
//! [`backtest::run`](crate::backtest::run) trades live without any change to the
//! strategy or the driver.
//!
//! This module is the concrete proof of the seam the [`Wallet`](crate::Wallet)
//! trait promises: everything market-specific and side-effecting (HTTP, signing,
//! venue order encoding, fill polling) lives here, behind the same trait a
//! [`PaperWallet`](crate::PaperWallet) satisfies.
//!
//! Ships two backends today: [`OkxWallet`], for OKX V5 perpetual swaps (and its
//! free demo-trading environment), and [`CoinbaseWallet`], for Coinbase Advanced
//! Trade **spot**. Both reuse the async `reqwest`/`tokio` stack the
//! [`sources`](crate::sources) providers already pull in; they differ only in
//! request signing — OKX uses base64-HMAC over `timestamp+method+path+body`,
//! Coinbase a per-request ES256 (ECDSA P-256) JWT. Gated behind the `live`
//! feature.
//!
//! **Synchronous over async.** The [`Wallet`](crate::Wallet) trait is a
//! synchronous `&mut self` surface; a venue REST API is async. Each live wallet
//! owns a private `tokio` runtime and bridges the two by blocking on each
//! request — so it must be driven from a *synchronous* context (as the backtest
//! driver is), not from inside an existing async runtime.

mod coinbase;
mod okx;

pub use coinbase::CoinbaseWallet;
pub use okx::OkxWallet;

use std::fmt;

/// The detail behind a [`WalletError::Venue`](crate::WalletError::Venue): why a
/// live REST call failed.
///
/// The trait-facing [`WalletError`](crate::WalletError) is a small `Copy` enum
/// with no room for an endpoint / status / body, so a live wallet returns the
/// `Venue` category there and stashes one of these on an internal log the caller
/// can inspect (see [`OkxWallet::errors`]).
#[derive(Debug, Clone)]
pub enum LiveError {
    /// The request never completed (DNS, connect, timeout, TLS, …).
    Network(String),
    /// The venue answered with a non-2xx status — or, on OKX, a 2xx envelope
    /// whose `code` / `sCode` reported a business rejection; the body usually
    /// carries a `{ "code": …, "msg": … }` explanation.
    Http { status: u16, body: String },
    /// The response completed but didn't parse into the expected shape.
    Decode(String),
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiveError::Network(e) => write!(f, "network error: {e}"),
            LiveError::Http { status, body } => write!(f, "http {status}: {body}"),
            LiveError::Decode(e) => write!(f, "decode error: {e}"),
        }
    }
}

impl std::error::Error for LiveError {}
