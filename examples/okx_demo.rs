//! Smoke-test the live [`OkxWallet`] against OKX's **demo trading**
//! environment — proving the exact same [`Wallet`] surface a backtest drives
//! also routes real orders to a venue.
//!
//! Create a demo API key (key + secret + passphrase) under "Demo trading" in
//! your OKX account, then run:
//!
//! ```text
//! OKX_DEMO_KEY=… OKX_DEMO_SECRET=… OKX_DEMO_PASSPHRASE=… \
//!   cargo run --example okx_demo --features live
//! ```
//!
//! It reads the account, opens a tiny `BTC-USDT-SWAP` position with a market
//! order, polls for the fill, then flattens — leaving the account as it started.
//! The wallet owns a `tokio` runtime and blocks on each REST call, so `main`
//! stays an ordinary synchronous function.

use std::time::Duration;

use fugazi::Candle;
use fugazi::live::OkxWallet;
use fugazi::types::Symbol;
use fugazi::wallet::{Units, Wallet};

const SYMBOL: &str = "BTC-USDT-SWAP";
/// A tiny order size, in base units — one contract of BTC-USDT-SWAP is
/// `ctVal = 0.01 BTC`, so this is a single contract.
const QTY: f64 = 0.01;

fn main() {
    let (key, secret, passphrase) = match (
        std::env::var("OKX_DEMO_KEY"),
        std::env::var("OKX_DEMO_SECRET"),
        std::env::var("OKX_DEMO_PASSPHRASE"),
    ) {
        (Ok(k), Ok(s), Ok(p)) => (k, s, p),
        _ => {
            eprintln!(
                "set OKX_DEMO_KEY, OKX_DEMO_SECRET and OKX_DEMO_PASSPHRASE (create a \
                 demo-trading API key in your OKX account)"
            );
            std::process::exit(1);
        }
    };

    let symbol = fugazi::types::symbol(SYMBOL);
    let mut wallet = OkxWallet::demo(key, secret, passphrase);

    wallet
        .refresh_account()
        .expect("account reachable on demo trading");
    println!(
        "connected — funds {:.2}  equity {:.2}  {SYMBOL} position {:+.4}",
        wallet.funds().0,
        wallet.equity().0,
        wallet.position(&symbol).amount,
    );

    let start = wallet.position(&symbol).amount;
    let target = start + QTY;
    println!("\nopening: market order to {target:+.4} {SYMBOL} …");
    wallet
        .set_position(Units {
            symbol: symbol.clone(),
            amount: target,
        })
        .expect("market order accepted");

    settle_to(&mut wallet, &symbol, target, "reached target");

    println!("\nclosing: flattening back to {start:+.4} …");
    wallet
        .set_position(Units {
            symbol: symbol.clone(),
            amount: start,
        })
        .expect("flatten accepted");
    settle_to(&mut wallet, &symbol, start, "flattened");

    println!(
        "\ndone — final {SYMBOL} position {:+.4}",
        wallet.position(&symbol).amount
    );
    for err in wallet.errors() {
        eprintln!("note: {err}");
    }
}

/// Poll a few times (feeding a bar each round so the wallet refreshes account
/// state and drains fills) until the position reaches `want`, printing fills.
fn settle_to(wallet: &mut OkxWallet, symbol: &Symbol, want: f64, ok_msg: &str) {
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(500));
        // A synthetic bar only carries a mark; the position comes from the
        // account refresh `update` performs.
        for fill in wallet.update(symbol.clone(), Candle::new(0.0, 0.0, 0.0, 0.0, 0.0)) {
            println!(
                "  fill: {:<4} {:.4} @ {:.2}  (order #{})",
                format!("{:?}", fill.side).to_uppercase(),
                fill.units,
                fill.price,
                fill.id.0,
            );
        }
        if (wallet.position(symbol).amount - want).abs() < 1e-6 {
            println!(
                "  {ok_msg}: position {:+.4}",
                wallet.position(symbol).amount
            );
            return;
        }
    }
    eprintln!("  timed out waiting for position to reach {want:+.4}");
}
