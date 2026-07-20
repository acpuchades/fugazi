//! Smoke-test the live [`BinanceFuturesWallet`] against the Binance USDⓈ-M
//! Futures **testnet** — proving the exact same [`Wallet`] surface a backtest
//! drives also routes real orders to a venue.
//!
//! Get free testnet keys (GitHub login) at <https://testnet.binancefuture.com>,
//! then run:
//!
//! ```text
//! BINANCE_TESTNET_KEY=… BINANCE_TESTNET_SECRET=… \
//!   cargo run --example binance_testnet --features live
//! ```
//!
//! It reads the account, opens a tiny `BTCUSDT` position with a market order,
//! polls for the fill, then flattens — leaving the account as it started. The
//! wallet owns a `tokio` runtime and blocks on each REST call, so `main` stays
//! an ordinary synchronous function.

use std::time::Duration;

use fugazi::live::BinanceFuturesWallet;
use fugazi::wallet::{Units, Wallet};
use fugazi::Candle;

const SYMBOL: &str = "BTCUSDT";
/// A tiny order size — one `LOT_SIZE` step on testnet BTCUSDT.
const QTY: f64 = 0.002;

fn main() {
    let (key, secret) = match (
        std::env::var("BINANCE_TESTNET_KEY"),
        std::env::var("BINANCE_TESTNET_SECRET"),
    ) {
        (Ok(k), Ok(s)) => (k, s),
        _ => {
            eprintln!(
                "set BINANCE_TESTNET_KEY and BINANCE_TESTNET_SECRET (free keys at \
                 https://testnet.binancefuture.com)"
            );
            std::process::exit(1);
        }
    };

    let symbol = SYMBOL.to_string();
    let mut wallet = BinanceFuturesWallet::testnet(key, secret);

    wallet.refresh_account().expect("account reachable on testnet");
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
        .set_position(Units { symbol: symbol.clone(), amount: target })
        .expect("market order accepted");

    settle_to(&mut wallet, &symbol, target, "reached target");

    println!("\nclosing: flattening back to {start:+.4} …");
    wallet
        .set_position(Units { symbol: symbol.clone(), amount: start })
        .expect("flatten accepted");
    settle_to(&mut wallet, &symbol, start, "flattened");

    println!("\ndone — final {SYMBOL} position {:+.4}", wallet.position(&symbol).amount);
    for err in wallet.errors() {
        eprintln!("note: {err}");
    }
}

/// Poll a few times (feeding a bar each round so the wallet refreshes account
/// state and drains fills) until the position reaches `want`, printing fills.
fn settle_to(wallet: &mut BinanceFuturesWallet, symbol: &str, want: f64, ok_msg: &str) {
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(500));
        // A synthetic bar only carries a mark; the position comes from the
        // account refresh `update` performs.
        for fill in wallet.update(symbol.to_string(), Candle::new(0.0, 0.0, 0.0, 0.0, 0.0)) {
            println!(
                "  fill: {:<4} {:.4} @ {:.2}  (order #{})",
                format!("{:?}", fill.side).to_uppercase(),
                fill.units,
                fill.price,
                fill.id.0,
            );
        }
        if (wallet.position(&symbol.to_string()).amount - want).abs() < 1e-6 {
            println!("  {ok_msg}: position {:+.4}", wallet.position(&symbol.to_string()).amount);
            return;
        }
    }
    eprintln!("  timed out waiting for position to reach {want:+.4}");
}
