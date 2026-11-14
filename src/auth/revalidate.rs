//! Keeps a streaming `/mcp` response honest after its request was let in. The
//! bearer is checked once, when the request starts; an SSE stream then lives for
//! hours, past `sessions --revoke`, `set-auth` or the token's own expiry. So the
//! stream's body asks the store again on an interval and ends once the token is
//! gone — the client's reconnect then meets the normal guard (401, or a refresh).
//! Holds the token's hash, never the bearer.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header::CONTENT_TYPE;
use axum::response::Response;
use http_body::{Frame, SizeHint};

use crate::oauth::Store;

/// How often an open stream's token is checked again: the longest a stream
/// outlives its token.
pub const STREAM_REVALIDATE: Duration = Duration::from_secs(30);

/// Wrap `response`'s body, if it is an event stream, so it ends within `every` of
/// the token behind `token_hash` being revoked or expiring. Anything else (a
/// one-shot body) passes through untouched.
pub fn end_when_revoked(
    response: Response,
    store: Arc<Store>,
    token_hash: String,
    every: Duration,
) -> Response {
    let streaming = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    if !streaming {
        return response;
    }
    response.map(|inner| {
        Body::new(Revalidated {
            inner,
            revoked: Some(Box::pin(revoked(store, token_hash, every))),
        })
    })
}

/// Resolves once the store says the token is no longer valid. A store that can't
/// answer is not a revocation: the stream stays and the next tick asks again —
/// the same rule as the guard's "outage is a 503, not a 401".
async fn revoked(store: Arc<Store>, token_hash: String, every: Duration) {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if matches!(store.validate_hash(&token_hash).await, Ok(false)) {
            return;
        }
    }
}

/// The response body, cut short when `revoked` resolves. No task: the check is
/// polled with the body, so it stops the moment the client goes away.
struct Revalidated {
    inner: Body,
    /// `None` once the stream has been ended.
    revoked: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl http_body::Body for Revalidated {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let Some(revoked) = self.revoked.as_mut() else {
            return Poll::Ready(None);
        };
        if revoked.as_mut().poll(cx).is_ready() {
            self.revoked = None;
            tracing::info!("stream ended: its token is no longer valid");
            return Poll::Ready(None);
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.revoked.is_none() || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
