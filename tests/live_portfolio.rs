//! A [`Portfolio`] can be driven against a live broker account.
//!
//! A portfolio is an ordinary strategy that trades the wallet it is handed, so
//! the account can be a [`PaperWallet`](fugazi::PaperWallet) or a real venue with
//! nothing else changing — `backtest::run(&mut portfolio, &mut account, snaps)`.
//! That is the whole point of the netting layer: the thing a backtest exercises
//! is the thing that deploys.
//!
//! These tests **construct** the pieces and never drive them — no network, no
//! credentials. What they pin is the part that can break silently at a distance:
//! that a live wallet is a `Wallet<Symbol>` a portfolio can be driven against,
//! and that both the portfolio and the account are `Send` (what lets
//! `backtest::run_many` / a rayon worker carry a live portfolio across a thread
//! boundary — a live impl that picked up a non-`Send` client would fail here
//! rather than in a user's build).
//!
//! Behind the off-by-default `live` feature, so this file is empty without it.
#![cfg(feature = "live")]

use fugazi::types::{Symbol, symbol as intern};
use fugazi::Wallet;
use fugazi::live::OkxWallet;
use fugazi::portfolio::Portfolio;
use fugazi::strategies::SingleAssetStrategy;

fn two_child_portfolio() -> Portfolio<Symbol> {
    Portfolio::builder()
        .with_initial_equity(1_000.0)
        .add(
            "btc",
            SingleAssetStrategy::<Symbol>::buy_and_hold(intern("BTCUSDT")),
        )
        .add(
            "eth",
            SingleAssetStrategy::<Symbol>::buy_and_hold(intern("ETHUSDT")),
        )
        .build()
}

#[test]
fn a_live_wallet_can_drive_a_portfolio() {
    // An `OkxWallet` is a `Wallet<Symbol>`, which is exactly what
    // `backtest::run(&mut portfolio, &mut account, snaps)` requires — the same
    // call a `PaperWallet` takes. Construct-only (no network); the type bound is
    // what we're pinning.
    fn drives<W: Wallet<Symbol>>(_: &W) {}
    let portfolio = two_child_portfolio();
    let account = OkxWallet::demo("key", "secret", "pass");
    drives(&account);
    assert_eq!(portfolio.child_count(), 2);
}

#[test]
fn a_live_backed_portfolio_is_still_send() {
    // `Send` on both the portfolio and the account is what lets
    // `backtest::run_many` and every parallel-optimize path carry a live
    // portfolio across a thread boundary. If either stops being `Send`, they
    // fail here rather than in a user's build.
    fn assert_send<T: Send>(_: &T) {}
    let portfolio = two_child_portfolio();
    let account = OkxWallet::demo("key", "secret", "pass");
    assert_send(&portfolio);
    assert_send(&account);
}
