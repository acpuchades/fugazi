//! What the O(period) variance actually costs, and what it buys.
//!
//! `WindowStats::variance` makes one centred pass over the retained window on
//! every *query*. The O(1) alternative carries a running sum of squares and
//! computes `E[X²] − E[X]²`, which is what TA-Lib's `STDDEV` does — and why
//! `stddev` is the one indicator where fugazi is meaningfully slower than
//! TA-Lib (2.7× at the time of writing).
//!
//! `src/indicators/stats.rs` justifies the choice with a table of relative
//! errors. This benchmark *re-derives* both halves of that trade so the
//! decision rests on numbers that can be re-run rather than on a comment:
//!
//! * **accuracy** — both algorithms against a high-precision reference, across
//!   the (mean, σ) range real instruments span;
//! * **cost** — ns/sample for each, on the same window.
//!
//! Run with `cargo bench --bench stddev_tradeoff`.

use std::hint::black_box;
use std::time::Instant;

use fugazi::indicators::{Identity, StdDev};
use fugazi::prelude::*;

const PERIOD: usize = 20;
const REPS: usize = 7;

/// The shortcut fugazi does **not** use: one running sum and one running
/// sum-of-squares, variance as `E[X²] − E[X]²`. O(1) per query.
struct ShortcutVar {
    period: usize,
    buf: Box<[Real]>,
    head: usize,
    len: usize,
    sum: Real,
    sum_sq: Real,
}

impl ShortcutVar {
    fn new(period: usize) -> Self {
        Self {
            period,
            buf: vec![0.0; period].into_boxed_slice(),
            head: 0,
            len: 0,
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    fn update(&mut self, x: Real) -> Option<Real> {
        if self.len == self.period {
            let old = self.buf[self.head];
            self.sum -= old;
            self.sum_sq -= old * old;
            self.buf[self.head] = x;
            self.head = (self.head + 1) % self.period;
        } else {
            let at = (self.head + self.len) % self.period;
            self.buf[at] = x;
            self.len += 1;
        }
        self.sum += x;
        self.sum_sq += x * x;
        if self.len < self.period {
            return None;
        }
        let n = self.period as Real;
        let mean = self.sum / n;
        // The cancellation lives here: two large, nearly-equal terms.
        let var = self.sum_sq / n - mean * mean;
        Some(var.max(0.0).sqrt())
    }
}

/// Reference standard deviation in extended precision: a centred two-pass sum
/// over `f64` promoted through `i128`-free Kahan compensation. Accurate enough
/// to score both candidates.
fn reference_stddev(xs: &[Real]) -> Real {
    let n = xs.len() as f64;
    // Kahan-compensated mean.
    let (mut sum, mut comp) = (0.0f64, 0.0f64);
    for &x in xs {
        let y = x - comp;
        let t = sum + y;
        comp = (t - sum) - y;
        sum = t;
    }
    let mean = sum / n;
    let (mut ss, mut c2) = (0.0f64, 0.0f64);
    for &x in xs {
        let d = x - mean;
        let y = d * d - c2;
        let t = ss + y;
        c2 = (t - ss) - y;
        ss = t;
    }
    (ss / n).sqrt()
}

/// A deterministic window with a chosen mean and dispersion.
fn window(mean: Real, sigma: Real, n: usize, seed: u64) -> Vec<Real> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let u = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            mean + sigma * u * 3.464_101_615_137_754 // ~unit variance for U(-.5,.5)
        })
        .collect()
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

fn bench(n: usize, mut f: impl FnMut()) -> f64 {
    let mut times = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64());
    }
    median(times) * 1e9 / n as f64
}

fn main() {
    println!("accuracy — relative error in stddev vs a Kahan-compensated reference");
    println!("period = {PERIOD}\n");
    println!(
        "{:>12}{:>10}{:>18}{:>20}",
        "mean", "sigma", "centred (ours)", "shortcut (TA-Lib)"
    );

    // The range real instruments span: a $30 stock through a five-figure crypto
    // pair quoted to the cent.
    let cases: [(Real, Real); 6] = [
        (1e2, 1.0),
        (1e2, 0.01),
        (1e5, 1e2),
        (1e5, 0.01),
        (1e9, 1.0),
        (1e9, 0.01),
    ];

    for (mean, sigma) in cases {
        let xs = window(mean, sigma, PERIOD, 0x5eed_1234);
        let want = reference_stddev(&xs);

        let mut ours = StdDev::new(Identity::new(), PERIOD);
        let mut got_ours = None;
        for &x in &xs {
            got_ours = ours.update(x);
        }
        let mut short = ShortcutVar::new(PERIOD);
        let mut got_short = None;
        for &x in &xs {
            got_short = short.update(x);
        }

        let rel = |got: Option<Real>| match got {
            Some(g) if want != 0.0 => ((g - want) / want).abs(),
            _ => f64::NAN,
        };
        let fmt = |e: f64| {
            if e == 0.0 {
                "exact".to_string()
            } else if e >= 1.0 {
                format!("{e:.0e} (!!)")
            } else {
                format!("{e:.1e}")
            }
        };
        println!(
            "{mean:>12.0e}{sigma:>10.0e}{:>18}{:>20}",
            fmt(rel(got_ours)),
            fmt(rel(got_short)),
        );
    }

    // ---- cost ---------------------------------------------------------------
    const N: usize = 200_000;
    let xs = window(1e5, 1e2, N, 0xabcd_1234);

    let centred = bench(N, || {
        let mut ind = StdDev::new(Identity::new(), PERIOD);
        for &x in &xs {
            black_box(ind.update(x));
        }
    });
    let shortcut = bench(N, || {
        let mut ind = ShortcutVar::new(PERIOD);
        for &x in &xs {
            black_box(ind.update(x));
        }
    });

    println!("\ncost — ns/sample, period = {PERIOD}, {N} samples, median of {REPS}");
    println!("  centred (ours)      {centred:>7.2}");
    println!("  shortcut (TA-Lib)   {shortcut:>7.2}");
    println!("  ratio               {:>7.2}x", centred / shortcut);
    println!(
        "\nThe shortcut is O(1) per query and ours is O(period), so the ratio\n\
         grows with the window. What it buys is the accuracy column above."
    );
}
