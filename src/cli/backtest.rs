//! The backtest driver: walk one symbol's candles through a strategy and a
//! [`PaperWallet`], writing the result files.
//!
//! Each bar: price the wallet at the candle's `close`, `update` the strategy,
//! then `trade` it; any orders the trade appended to the blotter are emitted to
//! `trades.csv` stamped with this bar's `time` and fill price (the close), and
//! the running equity is emitted to `returns.csv`. Both result files are written
//! `;`-delimited for Excel.

use std::path::Path;

use anyhow::{Context, Result};
use fugazi::prelude::*;

use crate::data::DataFrame;
use crate::spec::StrategySpec;

/// Headline numbers printed after a run.
pub struct Summary {
    pub final_equity: Real,
    pub return_pct: Real,
    pub trades: usize,
    pub bars: usize,
}

/// Run `spec` over the dataframe, seeded with `cash`, writing `trades.csv` and
/// `returns.csv` into `out_dir`.
pub fn run(spec: &StrategySpec, frame: &DataFrame, cash: Real, out_dir: &Path) -> Result<Summary> {
    let symbol = spec.symbol.clone();
    let mut strategy = spec.build();
    let candles = frame.candles(&symbol)?;

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating output dir `{}`", out_dir.display()))?;
    let mut trades = writer(&out_dir.join("trades.csv"))?;
    trades.write_record(["time", "symbol", "side", "quantity", "price"])?;
    let mut returns = writer(&out_dir.join("returns.csv"))?;
    returns.write_record(["time", "equity", "return"])?;

    let mut wallet = PaperWallet::new(cash);
    let mut prev_equity = cash;

    for (time, candle) in &candles {
        wallet.update(symbol.clone(), Reference(candle.close));
        strategy.update(*candle);

        let before = wallet.orders().len();
        strategy.trade(&mut wallet);
        for order in &wallet.orders()[before..] {
            let side = match order.side {
                Side::Buy => "buy",
                Side::Sell => "sell",
            };
            trades.write_record([
                time,
                &order.symbol,
                side,
                &order.quantity.to_string(),
                &candle.close.to_string(),
            ])?;
        }

        let equity = wallet.equity().0;
        let ret = if prev_equity != 0.0 {
            (equity - prev_equity) / prev_equity * 100.0
        } else {
            0.0
        };
        returns.write_record([time, &equity.to_string(), &ret.to_string()])?;
        prev_equity = equity;
    }

    trades.flush()?;
    returns.flush()?;

    let final_equity = wallet.equity().0;
    Ok(Summary {
        final_equity,
        return_pct: if cash != 0.0 {
            (final_equity - cash) / cash * 100.0
        } else {
            0.0
        },
        trades: wallet.orders().len(),
        bars: candles.len(),
    })
}

/// A `;`-delimited CSV writer at `path`.
fn writer(path: &Path) -> Result<csv::Writer<std::fs::File>> {
    csv::WriterBuilder::new()
        .delimiter(b';')
        .from_path(path)
        .with_context(|| format!("creating `{}`", path.display()))
}
