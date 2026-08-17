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
    double *times = malloc((size_t)reps * sizeof(double));
    if (!o || !h || !l || !c || !out || !times) return 1;

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
        printf("{\"name\":\"%s\",\"ns_per_sample\":%.4f}\n", name,            \
               med * 1e9 / (double)n);                                        \
    } while (0)

    BENCH("sma", TA_SMA(0, n - 1, c, SMA_P, &beg, &cnt, out));
    BENCH("ema", TA_EMA(0, n - 1, c, EMA_P, &beg, &cnt, out));
    BENCH("rsi", TA_RSI(0, n - 1, c, RSI_P, &beg, &cnt, out));
    BENCH("stddev", TA_STDDEV(0, n - 1, c, STDDEV_P, 1.0, &beg, &cnt, out));
    BENCH("atr", TA_ATR(0, n - 1, h, l, c, ATR_P, &beg, &cnt, out));

    TA_Shutdown();
    free(o); free(h); free(l); free(c); free(out); free(times);
    return 0;
}
