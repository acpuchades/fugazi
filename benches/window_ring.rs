//! Does the ring-buffer conversion that fixed `WindowStats` pay on the windows
//! that still use a `VecDeque`?
//!
//! `docs/PERFORMANCE.md` trick #3 records `WindowStats` going from a `VecDeque`
//! to a hand-rolled fixed ring, worth `Sma` 5.25 -> 1.38 ns/sample. Eight other
//! fixed-capacity windows in the crate were never converted. This prices the
//! two hottest shapes **in one binary**, which is the only arrangement that
//! survives trap 10/11 in that document: a control that the change cannot touch
//! sits beside the variants, so a reading of zero on the control is what makes
//! the other rows trustworthy.
//!
//! Shapes measured:
//!
//! * **lookback** — `ops.rs::Lookback`, backing `Lag` / `Diff` / `Ratio`. Its
//!   buffer is `VecDeque<Option<Real>>`, so it is the one shape where the
//!   element type is *also* wrong: `Option<Real>` is 16 bytes with no niche.
//!   Variant C drops the `Option` for a plain `Real` ring plus a fill counter.
//! * **wma** — `stats.rs::WmaState`, backing `Wma` and (three at a time) `Hma`.
//!
//! Run with `cargo bench --bench window_ring`.

use std::collections::VecDeque;
use std::hint::black_box;
use std::time::Instant;

use fugazi::indicators::{Correlation, Current, Hma, Identity, Vwap, Wma};
use fugazi::prelude::*;
use fugazi::types::{Atom, Candle};

const REPS: usize = 9;
const N: usize = 200_000;

// ---------------------------------------------------------------------------
// lookback — what ships today
// ---------------------------------------------------------------------------

/// Exactly `Lookback::update`'s body, minus the source call and the op.
struct LookbackDeque {
    period: usize,
    buffer: VecDeque<Option<Real>>,
}

impl LookbackDeque {
    fn new(period: usize) -> Self {
        Self {
            period,
            buffer: VecDeque::with_capacity(period + 1),
        }
    }

    fn update(&mut self, current: Option<Real>) -> Option<Real> {
        self.buffer.push_back(current);
        let past = if self.buffer.len() > self.period {
            self.buffer.pop_front().flatten()
        } else {
            None
        };
        match (current, past) {
            (Some(current), Some(past)) => Some(current - past),
            _ => None,
        }
    }
}

/// Same element type, fixed ring instead of a deque.
struct LookbackRingOpt {
    period: usize,
    buf: Box<[Option<Real>]>,
    head: usize,
    len: usize,
}

impl LookbackRingOpt {
    fn new(period: usize) -> Self {
        Self {
            period,
            buf: vec![None; period + 1].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    fn update(&mut self, current: Option<Real>) -> Option<Real> {
        let cap = self.period + 1;
        let at = {
            let a = self.head + self.len;
            if a >= cap { a - cap } else { a }
        };
        self.buf[at] = current;
        let past = if self.len == self.period {
            let old = self.buf[self.head];
            self.head += 1;
            if self.head == cap {
                self.head = 0;
            }
            old
        } else {
            self.len += 1;
            None
        };
        match (current, past) {
            (Some(current), Some(past)) => Some(current - past),
            _ => None,
        }
    }
}

/// Ring of bare `Real`, with a counter standing in for the `Option`.
///
/// The `Option` in the buffer only ever distinguishes "source had not produced
/// yet"; once a source has yielded a value it does not go back. Tracking the
/// count of *consecutive* live samples reproduces the same `None` prefix at half
/// the footprint and without a `flatten` per bar.
struct LookbackRingReal {
    period: usize,
    buf: Box<[Real]>,
    head: usize,
    live: usize,
}

impl LookbackRingReal {
    fn new(period: usize) -> Self {
        Self {
            period,
            buf: vec![0.0; period + 1].into_boxed_slice(),
            head: 0,
            live: 0,
        }
    }

    fn update(&mut self, current: Option<Real>) -> Option<Real> {
        let cap = self.period + 1;
        let Some(x) = current else {
            self.live = 0;
            return None;
        };
        let at = {
            let a = self.head + self.live.min(self.period);
            if a >= cap { a - cap } else { a }
        };
        self.buf[at] = x;
        if self.live > self.period {
            self.head += 1;
            if self.head == cap {
                self.head = 0;
            }
        }
        self.live += 1;
        if self.live > self.period {
            Some(x - self.buf[self.head])
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// wma — what ships today
// ---------------------------------------------------------------------------

struct WmaDeque {
    period: usize,
    window: VecDeque<Real>,
    sum: Real,
    weighted: Real,
}

impl WmaDeque {
    fn new(period: usize) -> Self {
        Self {
            period,
            window: VecDeque::with_capacity(period),
            sum: 0.0,
            weighted: 0.0,
        }
    }

    fn update(&mut self, x: Real) -> Option<Real> {
        if self.window.len() == self.period {
            let old = self.window.pop_front().expect("window is full");
            self.weighted = self.weighted - self.sum + self.period as Real * x;
            self.sum = self.sum - old + x;
            self.window.push_back(x);
        } else {
            self.window.push_back(x);
            self.weighted += self.window.len() as Real * x;
            self.sum += x;
        }
        if self.window.len() == self.period {
            let denom = (self.period * (self.period + 1) / 2) as Real;
            Some(self.weighted / denom)
        } else {
            None
        }
    }
}

struct WmaRing {
    period: usize,
    buf: Box<[Real]>,
    head: usize,
    len: usize,
    sum: Real,
    weighted: Real,
}

impl WmaRing {
    fn new(period: usize) -> Self {
        Self {
            period,
            buf: vec![0.0; period].into_boxed_slice(),
            head: 0,
            len: 0,
            sum: 0.0,
            weighted: 0.0,
        }
    }

    fn update(&mut self, x: Real) -> Option<Real> {
        if self.len == self.period {
            let old = self.buf[self.head];
            self.weighted = self.weighted - self.sum + self.period as Real * x;
            self.sum = self.sum - old + x;
            self.buf[self.head] = x;
            self.head += 1;
            if self.head == self.period {
                self.head = 0;
            }
        } else {
            let at = {
                let a = self.head + self.len;
                if a >= self.period { a - self.period } else { a }
            };
            self.buf[at] = x;
            self.len += 1;
            self.weighted += self.len as Real * x;
            self.sum += x;
        }
        if self.len == self.period {
            let denom = (self.period * (self.period + 1) / 2) as Real;
            Some(self.weighted / denom)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------

fn walk(n: usize) -> Vec<Real> {
    let mut out = Vec::with_capacity(n);
    let mut px = 100.0_f64;
    let mut s: u64 = 0x5eed_1234_5678_9abc;
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let noise = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
        px *= 1.0 + 0.0002 + 0.01 * noise;
        out.push(px);
    }
    out
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn bench(n: usize, mut f: impl FnMut()) -> f64 {
    // One untimed pass so the first rep does not carry page faults the others
    // do not.
    f();
    let mut times = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64());
    }
    median(times) * 1e9 / n as f64
}

/// One workload, run **exactly once** at a small sample count, for
/// `valgrind --tool=callgrind`. Selected by argv so each is its own process.
///
/// Wall-clock could not settle `vwap` and `correlation`: both moved ~10%, which
/// is inside the band where code layout and cache luck dominate (trap 6 in
/// `docs/PERFORMANCE.md`), and the control itself drifted 13% between the two
/// runs. Instruction counts are immune to both. `control` must read *identical*
/// before and after — it is the workload the change cannot touch.
fn icount_one(name: &str) {
    const M: usize = 20_000;
    let xs = walk(M);
    let candles = candle_walk(M);
    match name {
        "control" => {
            let mut acc = 0.0;
            for &x in &xs {
                acc += x;
            }
            black_box(acc);
        }
        "wma_14" => {
            let mut ind = Wma::new(Identity::new(), 14);
            for &x in &xs {
                black_box(ind.update(x));
            }
        }
        "hma_14" => {
            let mut ind = Hma::new(Identity::new(), 14);
            for &x in &xs {
                black_box(ind.update(x));
            }
        }
        "diff_1" => {
            let mut ind = Identity::<Real>::new().diff(1);
            for &x in &xs {
                black_box(ind.update(x));
            }
        }
        "vwap_20" => {
            let mut ind = Vwap::new(Current::candle(), 20);
            for &c in &candles {
                black_box(ind.update(Atom::new(c)));
            }
        }
        "correlation_20" => {
            let mut ind = Correlation::new(Identity::new(), Identity::new(), 20);
            for &x in &xs {
                black_box(ind.update(x));
            }
        }
        // The four windows left on `VecDeque` after Phase 13, skipped there on
        // the argument that their updates are already O(period) so the deque is
        // a small fraction. Measured here rather than argued.
        "percentile_100" => {
            let mut ind = fugazi::indicators::Percentile::new(Identity::new(), 100, 0.5);
            for &x in &xs {
                black_box(ind.update(x));
            }
        }
        "variance_ratio_20" => {
            let mut ind = fugazi::indicators::VarianceRatio::new(Identity::new(), 20, 4);
            for &x in &xs {
                black_box(ind.update(x));
            }
        }
        // The last open layout question from Phase 13: `WindowStats` keeps its
        // samples in a `Box<[Real]>`, one heap block per windowed indicator,
        // separate from the struct that owns it. Inline storage would remove an
        // indirection; it would also make `Sma` ~300 bytes. Both shapes here,
        // same binary, so the answer is a measurement rather than an argument.
        "win_heap" | "win_inline" => {
            const P: usize = 20;
            let heap = name == "win_heap";
            let mut hb = HeapWindow::new(P);
            let mut ib = InlineWindow::new(P);
            for &x in &xs {
                if heap {
                    hb.update(x);
                    black_box(hb.variance());
                } else {
                    ib.update(x);
                    black_box(ib.variance());
                }
            }
        }
        other => panic!("unknown workload `{other}`"),
    }
}

/// The shipped shape: samples in a separate heap block.
struct HeapWindow {
    period: usize,
    buf: Box<[Real]>,
    head: usize,
    len: usize,
    sum: Real,
}

/// The candidate: samples inline in the struct, capped at `CAP`.
struct InlineWindow {
    period: usize,
    buf: [Real; 32],
    head: usize,
    len: usize,
    sum: Real,
}

macro_rules! window_impl {
    ($t:ty, $mk:expr) => {
        impl $t {
            fn new(period: usize) -> Self {
                $mk(period)
            }
            fn update(&mut self, x: Real) -> bool {
                if self.len == self.period {
                    self.sum -= self.buf[self.head];
                    self.buf[self.head] = x;
                    self.head += 1;
                    if self.head == self.period {
                        self.head = 0;
                    }
                } else {
                    let at = self.head + self.len;
                    let at = if at >= self.period {
                        at - self.period
                    } else {
                        at
                    };
                    self.buf[at] = x;
                    self.len += 1;
                }
                self.sum += x;
                self.len == self.period
            }
            fn variance(&self) -> Real {
                let mean = self.sum / self.period as Real;
                let xs = &self.buf[..self.period];
                let mut acc = [0.0 as Real; 4];
                let (chunks, remainder) = xs.as_chunks::<4>();
                for chunk in chunks {
                    for (a, &v) in acc.iter_mut().zip(chunk) {
                        let d = v - mean;
                        *a += d * d;
                    }
                }
                for &v in remainder {
                    let d = v - mean;
                    acc[0] += d * d;
                }
                ((acc[0] + acc[1]) + (acc[2] + acc[3])) / self.period as Real
            }
        }
    };
}

window_impl!(HeapWindow, |period| HeapWindow {
    period,
    buf: vec![0.0; period].into_boxed_slice(),
    head: 0,
    len: 0,
    sum: 0.0,
});
window_impl!(InlineWindow, |period| InlineWindow {
    period,
    buf: [0.0; 32],
    head: 0,
    len: 0,
    sum: 0.0,
});

fn main() {
    // `cargo bench --bench window_ring -- <workload>` runs one workload once,
    // for callgrind. With no argument it prints the full wall-clock table.
    if let Some(name) = std::env::args().nth(1).filter(|a| !a.starts_with('-')) {
        icount_one(&name);
        return;
    }

    let xs = walk(N);
    let opt: Vec<Option<Real>> = xs.iter().map(|&x| Some(x)).collect();

    println!("ns/sample, {N} samples, median of {REPS}, all variants in one binary\n");

    // The control: the change cannot touch it, so a non-zero delta here is the
    // instrument moving, not the code. See trap 7/10 in docs/PERFORMANCE.md.
    let control = bench(N, || {
        let mut acc = 0.0;
        for &x in &xs {
            acc += x;
        }
        black_box(acc);
    });
    println!("control (bare sum)        {control:>8.2}\n");

    println!(
        "{:>8}{:>12}{:>12}{:>12}{:>10}{:>10}",
        "period", "deque", "ring<Opt>", "ring<Real>", "B/A", "C/A"
    );
    for &period in &[5usize, 14, 20, 50] {
        let a = bench(N, || {
            let mut ind = LookbackDeque::new(period);
            for &x in &opt {
                black_box(ind.update(x));
            }
        });
        let b = bench(N, || {
            let mut ind = LookbackRingOpt::new(period);
            for &x in &opt {
                black_box(ind.update(x));
            }
        });
        let c = bench(N, || {
            let mut ind = LookbackRingReal::new(period);
            for &x in &opt {
                black_box(ind.update(x));
            }
        });
        println!(
            "{period:>8}{a:>12.2}{b:>12.2}{c:>12.2}{:>10.2}{:>10.2}",
            b / a,
            c / a
        );
    }
    println!("  ^ lookback (Lag / Diff / Ratio)\n");

    println!("{:>8}{:>12}{:>12}{:>10}", "period", "deque", "ring", "B/A");
    for &period in &[5usize, 14, 20, 50] {
        let a = bench(N, || {
            let mut ind = WmaDeque::new(period);
            for &x in &xs {
                black_box(ind.update(x));
            }
        });
        let b = bench(N, || {
            let mut ind = WmaRing::new(period);
            for &x in &xs {
                black_box(ind.update(x));
            }
        });
        println!("{period:>8}{a:>12.2}{b:>12.2}{:>10.2}", b / a);
    }
    println!("  ^ wma (Wma, and three at a time inside Hma)\n");

    // ---- the shipped types, after the conversion --------------------------
    //
    // The blocks above are stand-ins that isolate the container; these are the
    // real indicators, so they carry their source call and their `Option`
    // handling too. They are what the change actually delivers.
    let candles = candle_walk(N);
    let atoms: Vec<Atom> = candles.iter().map(|&c| Atom::new(c)).collect();

    println!("{:>24}{:>12}", "shipped indicator", "ns/sample");
    let wma14 = bench(N, || {
        let mut ind = Wma::new(Identity::new(), 14);
        for &x in &xs {
            black_box(ind.update(x));
        }
    });
    println!("{:>24}{wma14:>12.2}", "wma_14");
    let hma14 = bench(N, || {
        let mut ind = Hma::new(Identity::new(), 14);
        for &x in &xs {
            black_box(ind.update(x));
        }
    });
    println!("{:>24}{hma14:>12.2}", "hma_14 (3 WMAs)");
    let diff1 = bench(N, || {
        let mut ind = Identity::<Real>::new().diff(1);
        for &x in &xs {
            black_box(ind.update(x));
        }
    });
    println!("{:>24}{diff1:>12.2}", "diff_1");
    let vwap20 = bench(N, || {
        let mut ind = Vwap::new(Current::candle(), 20);
        for a in &atoms {
            black_box(ind.update(a.clone()));
        }
    });
    println!("{:>24}{vwap20:>12.2}", "vwap_20");
    let corr20 = bench(N, || {
        let mut ind = Correlation::new(Identity::new(), Identity::new(), 20);
        for &x in &xs {
            black_box(ind.update(x));
        }
    });
    println!("{:>24}{corr20:>12.2}", "correlation_20");
}

fn candle_walk(n: usize) -> Vec<Candle> {
    let mut out = Vec::with_capacity(n);
    let mut px = 100.0_f64;
    let mut s: u64 = 0x5eed_1234_5678_9abc;
    for _ in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let noise = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
        let open = px;
        let close = px * (1.0 + 0.0002 + 0.01 * noise);
        out.push(Candle {
            open,
            high: open.max(close) * 1.001,
            low: open.min(close) * 0.999,
            close,
            volume: 1_000.0,
        });
        px = close;
    }
    out
}
