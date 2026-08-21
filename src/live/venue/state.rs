//! Bookkeeping every live backend keeps, independent of the venue.

use crate::types::Symbol;
use crate::wallet::{DEFAULT_RETENTION, OrderId, OrderKind, Rejection, WalletError, trim_front};

use super::super::LiveError;

/// A live wallet's error log and its buffer of refused orders.
///
/// Both were unbounded `Vec`s on both backends, and `errors()` is a
/// *non-draining* accessor — so a months-long live run, the deployment these
/// wallets exist for, never freed either. [`PaperWallet`](crate::PaperWallet)
/// bounds its own two logs at [`DEFAULT_RETENTION`] for exactly that reason,
/// and this is the same bound reached through the same amortized
/// [`trim_front`].
///
/// The rejection drain is deliberately **not** the one `PaperWallet` uses.
/// There, `take_rejections` advances a cursor over a history that
/// `PaperWallet::rejections()` also exposes; here there is no history accessor
/// and nothing to preserve, so the drain takes the buffer outright. Sharing the
/// bound and the trim arithmetic is worth it; sharing the drain would mean
/// either growing a live `rejections()` accessor nobody asked for or deleting a
/// bound Python method.
#[derive(Debug)]
pub(in crate::live) struct LiveLog {
    errors: Vec<LiveError>,
    rejections: Vec<Rejection<Symbol>>,
    retention: Option<usize>,
}

impl Default for LiveLog {
    fn default() -> Self {
        Self {
            errors: Vec::new(),
            rejections: Vec::new(),
            retention: Some(DEFAULT_RETENTION),
        }
    }
}

impl LiveLog {
    /// Record a failure that has no return channel — a best-effort account
    /// refresh or fill poll, where the caller carries on with stale state.
    pub(in crate::live) fn note(&mut self, err: LiveError) {
        self.errors.push(err);
        self.trim();
    }

    /// Record `err` and return the trait-facing [`WalletError::Venue`]
    /// category, which is all a `Copy` enum has room to say.
    pub(in crate::live) fn fail(&mut self, err: LiveError) -> WalletError {
        self.note(err);
        WalletError::Venue
    }

    /// A **refused order**: log the detail, buffer a [`Rejection`] for
    /// [`take_rejections`](crate::Wallet::take_rejections) so the driver can
    /// route it to [`Strategy::on_reject`](crate::Strategy::on_reject), and
    /// return [`WalletError::Venue`].
    ///
    /// Unlike [`fail`](Self::fail), this is for a submission the strategy
    /// expected to place — an entry the venue rejects leaves the strategy flat
    /// when it wanted a position, a rejected protective leg leaves it holding
    /// one it wanted out of.
    pub(in crate::live) fn refuse(
        &mut self,
        symbol: &str,
        id: OrderId,
        kind: OrderKind,
        err: LiveError,
    ) -> WalletError {
        self.note(err);
        self.reject(symbol, id, WalletError::Venue, kind);
        WalletError::Venue
    }

    /// Buffer a refusal the wallet itself decided, with no venue error behind
    /// it — the short remainder a spot account cannot hold, say.
    pub(in crate::live) fn reject(
        &mut self,
        symbol: &str,
        id: OrderId,
        error: WalletError,
        kind: OrderKind,
    ) {
        self.rejections.push(Rejection {
            symbol: crate::types::symbol(symbol),
            id,
            error,
            kind,
        });
        self.trim();
    }

    /// The errors recorded so far, oldest first, bounded by the retention.
    pub(in crate::live) fn errors(&self) -> &[LiveError] {
        &self.errors
    }

    /// Hand over every buffered refusal and clear the buffer.
    pub(in crate::live) fn take_rejections(&mut self) -> Vec<Rejection<Symbol>> {
        std::mem::take(&mut self.rejections)
    }

    fn trim(&mut self) {
        trim_front(&mut self.errors, self.retention);
        trim_front(&mut self.rejections, self.retention);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(i: usize) -> LiveError {
        LiveError::Decode(i.to_string())
    }

    /// Churning past the bound must drop the *oldest*, not stop recording.
    ///
    /// A live wallet's error log is written from `update()`, so a venue that is
    /// down for a week appends on every bar with nothing draining it. Before
    /// the bound this grew for the process's whole life.
    #[test]
    fn the_error_log_is_bounded_and_keeps_the_newest() {
        let mut log = LiveLog::default();
        for i in 0..DEFAULT_RETENTION * 3 {
            log.note(decode(i));
        }
        assert!(
            log.errors().len() <= DEFAULT_RETENTION * 2,
            "error log grew past the trim threshold: {}",
            log.errors().len()
        );
        let newest = log.errors().last().expect("a retained error");
        assert_eq!(
            newest.to_string(),
            decode(DEFAULT_RETENTION * 3 - 1).to_string(),
            "trimming must drop the oldest, so the newest error always survives",
        );
    }

    /// The refusal buffer is bounded too, for the case that never drains it:
    /// a caller driving the wallet loop by hand who never calls
    /// `take_rejections`.
    #[test]
    fn the_rejection_buffer_is_bounded() {
        let mut log = LiveLog::default();
        for i in 0..DEFAULT_RETENTION * 3 {
            log.reject(
                "BTC-USD",
                OrderId(i as u64),
                WalletError::Venue,
                OrderKind::Market,
            );
        }
        assert!(
            log.take_rejections().len() <= DEFAULT_RETENTION * 2,
            "rejection buffer grew past the trim threshold",
        );
    }

    /// A refusal writes both logs at once, and the drain empties only the
    /// rejection side — the error detail stays readable through `errors()`
    /// after the driver has taken the rejection.
    #[test]
    fn a_refusal_records_the_detail_and_the_rejection_separately() {
        let mut log = LiveLog::default();
        let err = log.refuse("BTC-USD", OrderId(7), OrderKind::Stop, decode(1));
        assert_eq!(err, WalletError::Venue);

        let drained = log.take_rejections();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id, OrderId(7));
        assert_eq!(drained[0].kind, OrderKind::Stop);

        assert_eq!(log.errors().len(), 1, "the detail outlives the drain");
        assert!(
            log.take_rejections().is_empty(),
            "a second drain must yield nothing"
        );
    }
}
