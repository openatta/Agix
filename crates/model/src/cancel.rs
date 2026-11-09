//! A model call's cancellation token, honoured while the provider is still
//! answering.
//!
//! Dropping a stream ends its request, but only when its consumer gets round
//! to dropping it — and a consumer parked on `next()` behind a provider that
//! has gone quiet is not going to. Both adapters therefore race the token
//! against the response: before `stream()` returns, and on every read after.

use base::interface::model::{ModelError, ModelEvent};
use futures::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

/// `stream`'s items until `cancel` fires, then one `Err(Cancelled)` and the
/// end.
pub(crate) fn until_cancelled<S>(
    stream: S,
    cancel: CancellationToken,
) -> impl Stream<Item = Result<ModelEvent, ModelError>> + Send + 'static
where
    S: Stream<Item = Result<ModelEvent, ModelError>> + Send + 'static,
{
    async_stream::stream! {
        let mut stream = Box::pin(stream);
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                next = stream.next() => Some(next),
            };
            match next {
                None => {
                    yield Err(ModelError::Cancelled);
                    break;
                }
                Some(Some(item)) => yield item,
                Some(None) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_stream_that_never_answers_ends_when_its_call_is_cancelled() {
        let cancel = CancellationToken::new();
        let mut stream = Box::pin(until_cancelled(
            futures::stream::pending::<Result<ModelEvent, ModelError>>(),
            cancel.clone(),
        ));
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("the cancel was not honoured");
        assert!(
            matches!(first, Some(Err(ModelError::Cancelled))),
            "{first:?}"
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn an_uncancelled_stream_passes_through_unchanged() {
        let events = vec![Ok(ModelEvent::TextDelta { text: "a".into() })];
        let out: Vec<_> = until_cancelled(futures::stream::iter(events), CancellationToken::new())
            .collect()
            .await;
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Ok(ModelEvent::TextDelta { text }) if text == "a"));
    }
}
