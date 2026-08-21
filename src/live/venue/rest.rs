//! The HTTP half of a live backend: the client, the runtime the blocking
//! [`Wallet`](crate::Wallet) surface bridges through, and the base URL.
//!
//! Signing is deliberately **not** here. The two venues differ in exactly that
//! (base64-HMAC over a prehash vs. a per-request ES256 JWT), and one of them
//! needs `&mut self` for a nonce counter while the other doesn't — so a shared
//! `signed` would have to widen both to the union of their needs forever.
//! Everything *around* the signature is identical, and that is what this holds.

use std::future::Future;

use super::super::LiveError;
use super::with_query;

/// A `reqwest` client, the private `tokio` runtime each live wallet owns, and
/// the venue's base URL.
///
/// The runtime incantation was written out three times before this existed
/// (twice in `coinbase.rs`, once in `okx.rs`), each with the same `expect`
/// message.
pub(in crate::live) struct HttpCore {
    client: reqwest::Client,
    rt: tokio::runtime::Runtime,
    base_url: String,
}

impl HttpCore {
    /// Panics only if a `tokio` current-thread runtime can't be built (out of
    /// OS resources) — the same failure every live wallet's constructor
    /// documents.
    pub(in crate::live) fn new(base_url: impl Into<String>) -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build a tokio runtime for the live wallet");
        Self {
            client: reqwest::Client::new(),
            rt,
            base_url: base_url.into(),
        }
    }

    /// The absolute URL for a request path, tolerating a base URL that was
    /// configured with a trailing slash.
    pub(in crate::live) fn url(&self, request_path: &str) -> String {
        format!("{}{request_path}", self.base_url.trim_end_matches('/'))
    }

    pub(in crate::live) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Block the calling thread on `fut`. Must not be called from inside an
    /// existing async runtime — see the module docs on `live`.
    pub(in crate::live) fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    /// An unsigned public GET — instrument / product specs on both venues.
    /// `params` is empty for a venue that puts the instrument in the path.
    pub(in crate::live) fn public_get(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<serde_json::Value, LiveError> {
        let url = self.url(&with_query(path, params));
        self.send(self.client.get(&url))
    }

    /// Send an already-built (and, for a private endpoint, already-signed)
    /// request and read its body.
    pub(in crate::live) fn send(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<serde_json::Value, LiveError> {
        self.block_on(async {
            let resp = req
                .send()
                .await
                .map_err(|e| LiveError::Network(e.to_string()))?;
            read_json(resp).await
        })
    }
}

/// Read a response body, mapping a non-2xx status into [`LiveError::Http`].
///
/// An empty body reads as `Null` rather than a decode error: a venue answers
/// some cancels with `204`-shaped emptiness, and the caller checks the envelope
/// it expects rather than the body's mere presence.
async fn read_json(resp: reqwest::Response) -> Result<serde_json::Value, LiveError> {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| LiveError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(LiveError::Http {
            status: status.as_u16(),
            body,
        });
    }
    if body.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&body).map_err(|e| LiveError::Decode(e.to_string()))
}
