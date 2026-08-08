//! A [`Portfolio`] can be driven against a live broker account.
//!
//! The composite trades exactly one wallet — its **substrate** — and children
//! trade notional ledgers over it, so the account can be a
//! [`PaperWallet`](fugazi::PaperWallet) or a real venue with nothing else
//! changing. That is the whole point of the netting layer: the thing a backtest
//! exercises is the thing that deploys.
//!
//! These tests **construct** such a portfolio and never drive it — there is no
//! network here, and no credentials. What they pin is the part that can break
//! silently at a distance: that a live wallet still satisfies the substrate
//! factory's `Box<dyn Wallet<Sym> + Send>` bound. `Send` is the fragile one — it
//! is what lets `Portfolio` cross a thread boundary (`backtest::run_many`, a
//! rayon worker), and a live impl that picked up a non-`Send` client internally
//! would fail here rather than in a user's build.
//!
//! Behind the off-by-default `live` feature, so this file is empty without it.
#![cfg(feature = "live")]

use std::sync::Arc;

use fugazi::Wallet;
use fugazi::live::BinanceFuturesWallet;
use fugazi::portfolio::{Portfolio, SubstrateFactory};
use fugazi::strategies::SingleAssetStrategy;

#[test]
fn a_live_wallet_satisfies_the_substrate_factory() {
    let factory: SubstrateFactory<String> = Arc::new(|_seed| {
        // `seed` is advisory live — the venue holds the real balance — so a
        // live factory ignores it and lets `equity()` report the account.
        Box::new(BinanceFuturesWallet::testnet("key", "secret")) as Box<dyn Wallet<String> + Send>
    });

    let portfolio: Portfolio<String> = Portfolio::builder()
        .with_initial_equity(1_000.0)
        .add(
            "btc",
            SingleAssetStrategy::<String>::buy_and_hold("BTCUSDT".to_string()),
        )
        .add(
            "eth",
            SingleAssetStrategy::<String>::buy_and_hold("ETHUSDT".to_string()),
        )
        .substrate(factory)
        .build();

    assert_eq!(portfolio.child_count(), 2);
}

#[test]
fn a_live_backed_portfolio_is_still_send() {
    // The property `Send` on the substrate bound exists to protect. If this
    // stops compiling, `backtest::run_many` and every parallel-optimize path
    // lose the live portfolio.
    fn assert_send<T: Send>(_: &T) {}

    let portfolio: Portfolio<String> = Portfolio::builder()
        .with_initial_equity(1_000.0)
        .add(
            "btc",
            SingleAssetStrategy::<String>::buy_and_hold("BTCUSDT".to_string()),
        )
        .substrate(Arc::new(|_seed| {
            Box::new(BinanceFuturesWallet::testnet("key", "secret"))
                as Box<dyn Wallet<String> + Send>
        }))
        .build();
    assert_send(&portfolio);
}
