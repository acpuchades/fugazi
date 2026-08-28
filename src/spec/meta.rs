//! `meta:` — the open-schema key every fugazi YAML document accepts and
//! **never interprets**.
//!
//! Every spec document is `deny_unknown_fields`, deliberately: a typo'd
//! `symbl:` or `rebalance_of:` that silently became a no-op would be a far
//! worse failure than a rejected load. That leaves nowhere for an external
//! service — a UI, a scheduler, a strategy registry — to stash its own record
//! alongside a strategy it generated or stores. `meta:` is that place: one
//! reserved key, arbitrary contents, owned entirely by whoever wrote the
//! document.
//!
//! ```yaml
//! symbol: BTC/USDT
//! meta:
//!   author: strategy-lab
//!   id: 4f1c-…
//!   tags: [momentum, crypto]
//! long:
//!   enter: !gt { lhs: !close, rhs: !sma { period: 20 } }
//! ```
//!
//! The contract, in both directions:
//!
//! - **fugazi never reads it.** No field name under `meta:` is reserved, none
//!   affects a build, a run, or a metric. Adding one cannot change a backtest.
//! - **fugazi never invents it.** Future fugazi fields go at the document root,
//!   next to `meta:`, so a service's `meta.tags` can never collide with a
//!   `tags:` fugazi adds later.
//! - **It survives the load** and is readable back — [`StrategySpec::meta`] /
//!   [`StrategyRef::meta`] and the per-shape `meta` fields, mirrored on Python's
//!   `StrategySpec.meta`.
//!
//! [`StrategySpec::meta`]: crate::spec::runnable::StrategySpec::meta
//! [`StrategyRef::meta`]: crate::spec::preset::StrategyRef::meta
//!
//! **One caveat.** `meta:` rides the same load pipeline as the rest of the
//! document, so `!import` and `!param` resolve inside it — usually what you
//! want (`meta: !import shared-meta.yml`), but it means a *literal* single-key
//! map spelled `{param: …}`, `{import: …}`, `{slot: …}` or `{undefined: …}`
//! inside `meta:` is treated as a placeholder rather than data. Nest external
//! data one level under a vendor key and the question never comes up.

/// The value of a document's `meta:` key: any JSON/YAML value, uninterpreted.
///
/// An alias rather than a newtype on purpose — the whole point is that fugazi
/// imposes no schema, so there is nothing for a wrapper to enforce. See the
/// [module docs](self) for the contract.
pub type Meta = serde_json::Value;
