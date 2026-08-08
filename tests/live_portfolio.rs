//! A [`Portfolio`] can be backed by live-broker sub-wallets.
//!
//! `Portfolio` holds `Box<dyn Wallet<Sym> + Send>` per child rather than a
//! concrete [`PaperWallet`](fugazi::PaperWallet), so the composite is
//! wallet-agnostic in the same way every other strategy shape is: a child
//! trades a real venue through exactly the [`SubWalletHandle`] path it trades
//! paper through.
//!
//! These tests **construct** such a portfolio and never drive it — there is no
//! network here, and no credentials. What they pin is the part that can break
//! silently at a distance: that a live wallet still satisfies the factory's
//! `Box<dyn Wallet<Sym> + Send>` bound. `Send` is the fragile one — it is what
//! lets `Portfolio` cross a thread boundary (`backtest::run_many`, a rayon
//! worker), and a live impl that picked up a non-`Send` client internally
//! would fail here rather than in a user's build.
//!
//! Behind the off-by-default `live` feature, so this file is empty without it.
#![cfg(feature = "live")]

use std::sync::Arc;

use fugazi::Wallet;
use fugazi::live::BinanceFuturesWallet;
use fugazi::portfolio::{Portfolio, SubWalletFactory};
use fugazi::strategies::SingleAssetStrategy;

#[test]
fn a_live_wallet_satisfies_the_sub_wallet_factory() {
    // One venue sub-account per child — the credentials are the seam that
    // keeps the books disjoint, which the composite's summing reads require.
    let credentials = [
        ("key-for-child-0", "secret-for-child-0"),
        ("key-for-child-1", "secret-for-child-1"),
    ];
    let factory: SubWalletFactory<String> = Arc::new(move |idx, _seed| {
        let (key, secret) = credentials[idx];
        // `seed` is ignored deliberately: a live wallet can't honour a cash
        // allocation, the venue holds the real balance.
        Box::new(BinanceFuturesWallet::testnet(key, secret)) as Box<dyn Wallet<String> + Send>
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
        .sub_wallets(factory)
        .build();

    assert_eq!(portfolio.child_count(), 2);
}

#[test]
fn a_live_backed_portfolio_is_still_send() {
    // The property `Send` on the sub-wallet bound exists to protect. If this
    // stops compiling, `backtest::run_many` and every parallel-optimize path
    // lose the live portfolio.
    fn assert_send<T: Send>(_: &T) {}

    let portfolio: Portfolio<String> = Portfolio::builder()
        .with_initial_equity(1_000.0)
        .add(
            "btc",
            SingleAssetStrategy::<String>::buy_and_hold("BTCUSDT".to_string()),
        )
        .sub_wallets(Arc::new(|_idx, _seed| {
            Box::new(BinanceFuturesWallet::testnet("key", "secret"))
                as Box<dyn Wallet<String> + Send>
        }))
        .build();
    assert_send(&portfolio);
}
