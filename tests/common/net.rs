//! Hosting a `wiremock` server for a **blocking** client.
//!
//! The live wallets own a private `tokio` runtime and block on each REST call,
//! so they must be driven from a synchronous context — calling one from inside
//! a `#[tokio::test]` nests runtimes and panics. [`serve`] is the way round it:
//! it stands the mock server up on a runtime it then keeps alive (the worker
//! threads go on serving after `block_on` returns) and hands the wallet calls
//! back to the main thread, outside any runtime context.
//!
//! Was copy-pasted byte-for-byte into `live_okx.rs` and `live_coinbase.rs`.

use wiremock::MockServer;

/// A mock HTTP server plus the runtime hosting it. **Both fields must stay
/// alive** for the duration of the test — dropping either tears the server
/// down mid-call, which surfaces as a connection error rather than as anything
/// pointing at the lifetime.
pub struct Server {
    /// Kept for its `Drop`; the worker threads are what serve the mocks.
    _runtime: tokio::runtime::Runtime,
    /// Kept for its `Drop`; also how a test asserts on received requests.
    pub mock: MockServer,
    /// The base URL to point the client at.
    pub uri: String,
}

/// Stand up a mock server on a kept-alive multi-threaded runtime, running
/// `setup` against it first to mount the stubs.
///
/// `setup` is spelled as a boxed future rather than an `async` closure because
/// it borrows the `&MockServer` it mounts onto.
pub fn serve<F>(setup: F) -> Server
where
    F: for<'a> FnOnce(
        &'a MockServer,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
{
    let runtime = tokio::runtime::Runtime::new().expect("multi-thread runtime");
    let mock = runtime.block_on(async {
        let server = MockServer::start().await;
        setup(&server).await;
        server
    });
    let uri = mock.uri();
    Server {
        _runtime: runtime,
        mock,
        uri,
    }
}
