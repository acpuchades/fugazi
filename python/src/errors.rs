//! The exception hierarchy the bindings raise.
//!
//! Rust distinguishes these at the type level — `WalletError`, `LiveError`,
//! `SourceError`, and the spec layer's `Err(String)` with its `!tag > `
//! breadcrumb — and the boundary used to flatten all four into `ValueError`.
//! That left a caller string-matching a message to tell "the venue refused the
//! order" from "your document doesn't build", which is the sort of thing that
//! works until the message is reworded.
//!
//! Every class here **subclasses `ValueError`**, so `except ValueError` keeps
//! catching exactly what it caught before. The hierarchy only adds resolution:
//!
//! ```text
//! ValueError
//! └── fugazi.FugaziError
//!     ├── fugazi.SpecError     — a document that won't load or build
//!     ├── fugazi.WalletError   — an order the account refused
//!     └── fugazi.FetchError    — a provider that wouldn't serve the request
//! ```
//!
//! **`TypeError` stays `TypeError`.** Passing a `Candle` where a `float` was
//! wanted is a Python call error, not a fugazi condition, and rehoming it under
//! this tree would make `except FugaziError` swallow ordinary bugs.
//!
//! Argument validation that fugazi can answer *without* consulting a document,
//! an account or a provider stays a bare `ValueError` too — `period must be
//! greater than 0` is not a spec error just because a spec might contain it.

use pyo3::create_exception;
use pyo3::exceptions::PyValueError;

create_exception!(
    fugazi,
    FugaziError,
    PyValueError,
    "Base class for every error condition fugazi raises.\n\n\
     Subclasses `ValueError`, so existing `except ValueError` handlers are \
     unaffected. Catch this to mean \"fugazi refused something\" without \
     caring which layer did."
);

create_exception!(
    fugazi,
    SpecError,
    FugaziError,
    "A strategy or cost document that parses but cannot be used.\n\n\
     Raised by `load_spec`, `optimize`, and every `run`/`evaluate` that builds \
     a document first: an unknown `!get` column, a slot handed the wrong type, \
     `!portfolio_book` outside a portfolio. The message carries the spec \
     layer's `!tag > ` breadcrumb, so the path to the offending node is in the \
     text."
);

create_exception!(
    fugazi,
    WalletError,
    FugaziError,
    "An order or account operation the wallet refused.\n\n\
     Insufficient funds, a short on a spot account, a debit that would take \
     the balance negative, or a live venue's REST failure. Distinct from \
     `SpecError` because it is a property of the account at this moment, not of \
     the strategy — the same order may well succeed on the next bar."
);

create_exception!(
    fugazi,
    FetchError,
    FugaziError,
    "A data provider would not serve the request.\n\n\
     An unknown symbol, a cadence the venue does not publish, a rate limit, a \
     transport failure. Raised only once the provider has been consulted; a \
     malformed `since=` string is a plain `ValueError`, since fugazi rejects \
     that before any request goes out."
);
