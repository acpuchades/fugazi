/*
 * The native tier of the TA-Lib comparison: TA-Lib's C API, no Python.
 *
 * Why this exists. `tools/bench_three_tier.py` measures TA-Lib through `talib`,
 * the Cython bindings, and that is the right baseline for *fugazi's Python
 * bindings* — both sides cross a Python boundary, so the comparison is
 * apples to apples. It is the wrong baseline for **fugazi's Rust engine**: the
 * wrapper's per-call cost, however small, would be credited to fugazi.
 *
 * Measured, that cost is small: 1.47 vs 1.37 ns/sample on SMA, 5.40 vs 4.83 on
 * ATR — roughly 5-12%. Small enough that it changes no verdict, large enough
 * that quoting the wrong one is a thumb on the scale for free.
 *
 * So the Rust row is compared against this, and the Python row against `talib`.
 * One baseline per tier, each matched to what it is measuring.
 *
 * Deliberately standalone C rather than a Rust FFI shim: linking TA-Lib into the
 * workspace would make a C library a build dependency of `cargo test` for
 * everyone, to serve one benchmark. `tools/bench_three_tier.py` compiles this on
 * demand and skips the tier when the toolchain or the library is absent.
 *
 * Emits one JSON record per indicator on stdout, the same shape the Rust tier
 * emits, so the driver parses both the same way.
 *
 * Built and run by tools/bench_three_tier.py; see there for the flags.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <ta-lib/ta_libc.h>

/* Keep in sync with benches/three_tier.rs and tools/bench_three_tier.py. */
#define SMA_P 10
#define EMA_P 10
#define RSI_P 14
#define STDDEV_P 10
#define ATR_P 14
/* Multi-output. TA-Lib emits every line of these in one call, which is the
 * shape fugazi's own multi-output `update` has — so the comparison is like for
 * like. The two exceptions are noted at their BENCH lines. */
#define MACD_FAST 12
#define MACD_SLOW 26
#define MACD_SIGNAL 9
#define BBANDS_P 20
#define BBANDS_K 2.0
#define AROON_P 14
#define DMI_P 14
#define CORREL_P 20
#define LINREG_P 14

#define REPS 7

static int cmp_double(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

static double median(double *xs, int n) {
    qsort(xs, n, sizeof(double), cmp_double);
    return xs[n / 2];
}

static double now_sec(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

/*
 * The same deterministic LCG walk every other tier is fed, so all three see
 * identical numbers. Mirrors `synth_candles` in benches/common/mod.rs and
 * `synth` in tools/bench_three_tier.py.
 */
static void synth(int n, double *o, double *h, double *l, double *c) {
    double px = 100.0;
    unsigned long long s = 0x5EED123456789ABCULL;
    for (int i = 0; i < n; i++) {
        s = s * 6364136223846793005ULL + 1442695040888963407ULL;
        double noise = (double)(s >> 33) / 4294967295.0 - 0.5;
        double ret = 0.0002 + 0.01 * noise;
        double open = px, close = px * (1.0 + ret);
        o[i] = open;
        c[i] = close;
        h[i] = (open > close ? open : close) * 1.001;
        l[i] = (open < close ? open : close) * 0.999;
        px = close;
    }
}

int main(int argc, char **argv) {
    int n = 200000;
    /*
     * `--only` / `--reps` / `--warmup` exist for callgrind. Wall-clock on a
     * contended machine cannot compare this against fugazi, but instruction
     * counts can — and for that the process must do exactly one timed pass of
     * exactly one indicator, with a `--only=none` control to subtract the
     * synth + startup cost. Defaults are the benchmark's own settings.
     */
    const char *only = NULL;
    int reps = REPS, warmup = 2;
    for (int i = 1; i < argc; i++) {
        if (strncmp(argv[i], "--n=", 4) == 0) n = atoi(argv[i] + 4);
        else if (strncmp(argv[i], "--only=", 7) == 0) only = argv[i] + 7;
        else if (strncmp(argv[i], "--reps=", 7) == 0) reps = atoi(argv[i] + 7);
        else if (strncmp(argv[i], "--warmup=", 9) == 0) warmup = atoi(argv[i] + 9);
    }
    if (reps < 1) reps = 1;

    double *o = malloc((size_t)n * sizeof(double));
    double *h = malloc((size_t)n * sizeof(double));
    double *l = malloc((size_t)n * sizeof(double));
    double *c = malloc((size_t)n * sizeof(double));
    double *out = malloc((size_t)n * sizeof(double));
    /* Multi-output calls write one array per line, so they need their own
     * buffers: TA-Lib will not alias them and a real caller would not either. */
    double *out2 = malloc((size_t)n * sizeof(double));
    double *out3 = malloc((size_t)n * sizeof(double));
    double *times = malloc((size_t)reps * sizeof(double));
    if (!o || !h || !l || !c || !out || !out2 || !out3 || !times) return 1;

    synth(n, o, h, l, c);
    TA_Initialize();

    int beg, cnt;

/*
 * Time `call` over REPS runs and print its median ns/sample.
 *
 * The output buffer is written every rep, so the measurement covers the same
 * work the Python tier's `talib.SMA(...)` covers — the C kernel plus the store
 * — and excludes only the array *allocation*, which the Python tier also
 * excludes by reusing NumPy's own buffer.
 *
 * WARMUP is load-bearing, not politeness. Measured on this machine, a cold
 * process reports SMA at 1.99 ns/sample and a warm one at 1.38 — a 44% error,
 * and it inflates the *baseline*, so it flatters whatever is being compared
 * against TA-Lib. It is CPU frequency ramp plus cold caches, and it decays over
 * the first run or two. Discard them.
 */
#define BENCH(name, call)                                                     \
    do {                                                                      \
        if (only && strcmp(only, name) != 0) break;                           \
        for (int r = 0; r < warmup; r++) { call; }                            \
        for (int r = 0; r < reps; r++) {                                      \
            double t0 = now_sec();                                            \
            call;                                                             \
            times[r] = now_sec() - t0;                                        \
        }                                                                     \
        double med = median(times, reps);                                      \
        /* Every sample, not just the summary: the driver keeps them so a       \
         * distribution can be re-analysed or plotted with error bars later     \
         * without re-running anything. `times` is sorted by `median` above,    \
         * so these come out ascending. */                                     \
        printf("{\"name\":\"%s\",\"ns_per_sample\":%.4f,\"samples\":[", name, \
               med * 1e9 / (double)n);                                        \
        for (int r = 0; r < reps; r++)                                         \
            printf("%s%.4f", r ? "," : "", times[r] * 1e9 / (double)n);        \
        printf("]}\n");                                                       \
    } while (0)

    BENCH("sma", TA_SMA(0, n - 1, c, SMA_P, &beg, &cnt, out));
    BENCH("ema", TA_EMA(0, n - 1, c, EMA_P, &beg, &cnt, out));
    BENCH("rsi", TA_RSI(0, n - 1, c, RSI_P, &beg, &cnt, out));
    BENCH("stddev", TA_STDDEV(0, n - 1, c, STDDEV_P, 1.0, &beg, &cnt, out));
    BENCH("atr", TA_ATR(0, n - 1, h, l, c, ATR_P, &beg, &cnt, out));

    /* Two-source rolling statistics. Both legs are the close series in all
     * three tiers: the paired window's cost does not depend on the values, and
     * making the second leg the cheapest possible source keeps the row a
     * measurement of the window rather than of its operands.
     *
     * `correlation` stands for the whole
     * paired family: `Covariance` and `Beta` are the same `WindowCovariance`
     * core reading a different field out of one shared `moments()` pass, so
     * their cost is this row's. (`TA_BETA` is deliberately *not* benched: it
     * differences its two inputs into returns internally, so it is doing work
     * fugazi's `Beta` is not handed, and the row would compare two different
     * amounts of work.) */
    BENCH("correlation", TA_CORREL(0, n - 1, c, c, CORREL_P, &beg, &cnt, out));

    /* `TA_LINEARREG_SLOPE` fills one array; fugazi's `LinReg` produces slope,
     * intercept, value and r² from one window every bar and the accessor
     * projects one out. So this row charges fugazi for four readings against
     * TA-Lib's one — the honest comparison for a caller who wants a slope,
     * and the reason `linreg` is *not* in the multi-output block above (TA-Lib
     * has no single call that fills all four). */
    BENCH("linreg_slope",
          TA_LINEARREG_SLOPE(0, n - 1, c, LINREG_P, &beg, &cnt, out));

    /* ---- multi-output ---------------------------------------------------
     *
     * One call, every line. That is the fair shape to put against a fugazi
     * multi-output `update`, which also produces the whole value struct per
     * bar — and it is what a caller who wants two lines of the same indicator
     * actually writes on both sides.
     */
    BENCH("macd", TA_MACD(0, n - 1, c, MACD_FAST, MACD_SLOW, MACD_SIGNAL,
                          &beg, &cnt, out, out2, out3));
    BENCH("bbands", TA_BBANDS(0, n - 1, c, BBANDS_P, BBANDS_K, BBANDS_K,
                              TA_MAType_SMA, &beg, &cnt, out, out2, out3));
    BENCH("aroon", TA_AROON(0, n - 1, h, l, AROON_P, &beg, &cnt, out, out2));

    /* `dmi` and `adx` are the two workloads where TA-Lib has no combined
     * entry point and fugazi does, so the call *counts* differ. That is the
     * measurement, not a flaw in it: TA-Lib's PLUS_DI and MINUS_DI each
     * re-derive the same Wilder-smoothed true range from scratch, and ADX
     * re-derives both DI lines on top of that, while `Dmi`/`Adx` carry one set
     * of Wilder states and emit the lines together. A caller wanting the pair
     * pays for both calls, so both calls are timed. */
    BENCH("dmi", do {
        TA_PLUS_DI(0, n - 1, h, l, c, DMI_P, &beg, &cnt, out);
        TA_MINUS_DI(0, n - 1, h, l, c, DMI_P, &beg, &cnt, out2);
    } while (0));
    BENCH("adx", do {
        TA_PLUS_DI(0, n - 1, h, l, c, DMI_P, &beg, &cnt, out);
        TA_MINUS_DI(0, n - 1, h, l, c, DMI_P, &beg, &cnt, out2);
        TA_ADX(0, n - 1, h, l, c, DMI_P, &beg, &cnt, out3);
    } while (0));

    TA_Shutdown();
    free(o); free(h); free(l); free(c); free(out); free(out2); free(out3);
    free(times);
    return 0;
}
