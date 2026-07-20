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
//! Ships one backend today — [`BinanceFuturesWallet`], for Binance USDⓈ-M
//! Futures (and its free public testnet at `testnet.binancefuture.com`). It
//! reuses the async `reqwest`/`tokio` stack the [`sources`](crate::sources)
//! providers already pull in, and adds HMAC-SHA256 request signing. Gated behind
//! the `live` feature.
//!
//! **Synchronous over async.** The [`Wallet`](crate::Wallet) trait is a
//! synchronous `&mut self` surface; a venue REST API is async. Each live wallet
//! owns a private `tokio` runtime and bridges the two by blocking on each
//! request — so it must be driven from a *synchronous* context (as the backtest
//! driver is), not from inside an existing async runtime.

mod binance;

pub use binance::BinanceFuturesWallet;

use std::fmt;

/// The detail behind a [`WalletError::Venue`](crate::WalletError::Venue): why a
/// live REST call failed.
///
/// The trait-facing [`WalletError`](crate::WalletError) is a small `Copy` enum
/// with no room for an endpoint / status / body, so a live wallet returns the
/// `Venue` category there and stashes one of these on an internal log the caller
/// can inspect (see [`BinanceFuturesWallet::errors`]).
#[derive(Debug, Clone)]
pub enum LiveError {
    /// The request never completed (DNS, connect, timeout, TLS, …).
    Network(String),
    /// The venue answered with a non-2xx status; the body usually carries a
    /// Binance `{ "code": …, "msg": … }` explanation.
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
