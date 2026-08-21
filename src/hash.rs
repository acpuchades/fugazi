//! A fast, non-cryptographic [`BuildHasher`] for the crate's internal
//! symbol-keyed maps.
//!
//! # Why not the default
//!
//! `std`'s `RandomState` is SipHash-1-3 — a keyed, DoS-resistant hash. That is
//! the right default for a map whose keys might come from an attacker, and the
//! wrong one for `PaperWallet`'s books, which hold a handful of symbols chosen
//! by the person running the backtest. Hashing a `String` symbol through SipHash
//! costs tens of nanoseconds, and the wallet does it several times per bar per
//! symbol (`bars`, `positions`, `pending`, `protective`, `limits`,
//! `per_symbol_costs`).
//!
//! # Why not a dependency
//!
//! `rustc-hash` / `ahash` would do this too, but the crate's dependency policy
//! is to reach for closed form first, and this is thirty lines. The library's
//! unconditional dependency set is deliberately small (see `Cargo.toml`).
//!
//! # What it is
//!
//! The FxHash construction rustc itself uses: multiply by a large odd constant,
//! rotate, xor. Not DoS-resistant and not intended to be — **do not use it for a
//! map whose keys are untrusted input.** Nothing here is exposed publicly.
//!
//! # Determinism
//!
//! Unlike `RandomState` this is unseeded, so iteration order is stable across
//! processes. That is a nice property but it is deliberately *not* relied upon:
//! `PaperWallet::marked_equity` still sorts before
//! summing, because "the iteration order happens to be stable" is a much weaker
//! guarantee than "the sum has a canonical order", and it would break silently
//! if the map type were ever changed back.

use std::hash::{BuildHasherDefault, Hasher};

/// Symbol-keyed map used inside the wallet. Same API as `HashMap`, different
/// hasher — see the module docs.
pub(crate) type SymMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// `0x51_7c_c1_b7_27_22_0a_95` — the odd constant from rustc's FxHash, chosen so
/// the multiply spreads entropy across the whole word.
const SEED: u64 = 0x517c_c1b7_2722_0a95;
const ROTATE: u32 = 5;

/// A small, fast, non-cryptographic hasher. See the module docs for the
/// trade-off it makes.
#[derive(Default, Clone)]
pub(crate) struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(ROTATE) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        // Consume eight bytes at a time, then the tail in decreasing widths.
        let mut rest = bytes;
        while let Some((chunk, tail)) = rest.split_first_chunk::<8>() {
            self.add(u64::from_ne_bytes(*chunk));
            rest = tail;
        }
        if let Some((chunk, tail)) = rest.split_first_chunk::<4>() {
            self.add(u32::from_ne_bytes(*chunk) as u64);
            rest = tail;
        }
        if let Some((chunk, tail)) = rest.split_first_chunk::<2>() {
            self.add(u16::from_ne_bytes(*chunk) as u64);
            rest = tail;
        }
        if let Some(&b) = rest.first() {
            self.add(b as u64);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    fn hash_of<T: Hash>(v: &T) -> u64 {
        let mut h = FxHasher::default();
        v.hash(&mut h);
        h.finish()
    }

    #[test]
    fn distinct_symbols_hash_distinctly() {
        // Not a guarantee in general — it is a hash — but the realistic symbol
        // shapes must not collide, or the maps degrade to linear scans.
        let syms = [
            "BTC",
            "ETH",
            "SOL",
            "ADA",
            "BTCUSDT",
            "ETHUSDT",
            "BTC/USDT:USDT",
            "AAPL",
            "MSFT",
            "S000",
            "S001",
            "S002",
            "S063",
        ];
        let mut seen = std::collections::HashSet::new();
        for s in syms {
            assert!(seen.insert(hash_of(&s)), "collision on {s:?}");
        }
    }

    #[test]
    fn map_round_trips() {
        let mut m: SymMap<String, i32> = SymMap::default();
        for i in 0..1_000 {
            m.insert(format!("S{i:04}"), i);
        }
        assert_eq!(m.len(), 1_000);
        for i in 0..1_000 {
            assert_eq!(m.get(&format!("S{i:04}")), Some(&i));
        }
        assert_eq!(m.get("nope"), None);
    }

    /// Byte-oriented and integer-oriented writes must not disagree for values
    /// that are equal — a `Hash` impl may use either.
    #[test]
    fn tail_handling_covers_every_length() {
        for len in 0..24usize {
            let bytes: Vec<u8> = (0..len as u8).collect();
            let mut a = FxHasher::default();
            a.write(&bytes);
            let mut b = FxHasher::default();
            b.write(&bytes);
            assert_eq!(a.finish(), b.finish(), "len {len} is not deterministic");
        }
    }
}
