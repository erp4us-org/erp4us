use crate::{
    configured_scope,
    drafts::{Failure, Store},
    material_plan::{AuthorizedPlan, STREAM, decode, subject},
    required,
};
use erp4us_messaging::{AckKind, ConnectOptions, Connection, consumer, stream};
use futures_util::StreamExt;
use std::{future::Future, io, time::Duration};
use tokio::time::{sleep, timeout};

const DURABLE: &str = "procurement-material-plans-v1";
const ACK_WAIT: Duration = Duration::from_secs(30);
const LEASE_INTERVAL: Duration = Duration::from_secs(10);
const LEASE_PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);
const LEASE_RENEWALS: usize = 3;
const QUARANTINE_RETRY: Duration = Duration::from_secs(10);
const RETRY: [u64; 4] = [1, 2, 4, 8];

async fn connect(user: &str) -> io::Result<Connection> {
    Connection::connect(
        &required("NATS_URL")?,
        ConnectOptions::with_user_and_password(user.into(), required("NATS_PASSWORD")?)
            .custom_inbox_prefix("_INBOX.procurement-inventory")
            .connection_timeout(Duration::from_secs(3)),
    )
    .await
    .map_err(broker_unavailable)
}

fn broker_unavailable<E>(_: E) -> io::Error {
    io::Error::other("Procurement broker operation unavailable")
}

fn durable_config(filter_subject: String) -> consumer::pull::Config {
    consumer::pull::Config {
        durable_name: Some(DURABLE.into()),
        filter_subject,
        deliver_policy: consumer::DeliverPolicy::All,
        ack_policy: consumer::AckPolicy::Explicit,
        ack_wait: ACK_WAIT,
        max_ack_pending: 1,
        // Apply retry limits in the worker: keep redelivering if quarantine is unavailable.
        max_deliver: -1,
        ..Default::default()
    }
}

fn matches_policy(policy: &consumer::Config, filter: &str) -> bool {
    policy.durable_name.as_deref() == Some(DURABLE)
        && policy.deliver_subject.is_none()
        && policy.filter_subject == filter
        && policy.filter_subjects.is_empty()
        && policy.deliver_policy == consumer::DeliverPolicy::All
        && policy.ack_policy == consumer::AckPolicy::Explicit
        && policy.ack_wait == ACK_WAIT
        && policy.max_ack_pending == 1
        && policy.max_deliver == -1
        && policy.backoff.is_empty()
        && !policy.headers_only
}

fn check_policy(info: &consumer::Info, subject: &str) -> io::Result<()> {
    if info.stream_name != STREAM || info.name != DURABLE || !matches_policy(&info.config, subject)
    {
        return Err(io::Error::other(
            "Procurement durable policy does not match configuration",
        ));
    }
    Ok(())
}

pub(super) async fn provision() -> io::Result<()> {
    let scope = configured_scope().map_err(io::Error::other)?;
    let filter = subject(scope).map_err(broker_unavailable)?;
    let broker = connect("provisioner").await?;
    let jetstream = broker.jetstream(Duration::from_secs(5));
    let stream = jetstream
        .operations()
        .get_stream_no_info(STREAM)
        .await
        .map_err(broker_unavailable)?;
    let durable = stream
        .get_or_create_consumer(DURABLE, durable_config(filter.clone()))
        .await
        .map_err(broker_unavailable)?;
    check_policy(durable.cached_info(), &filter)
}

fn retry_delay(delivered: i64) -> Option<Duration> {
    let attempt = usize::try_from(delivered.checked_sub(1)?).ok()?;
    RETRY.get(attempt).copied().map(Duration::from_secs)
}

async fn renew_lease(
    interval: Duration,
    publish_timeout: Duration,
    mut publish_progress: impl AsyncFnMut() -> bool,
) {
    let mut warned = false;
    for _ in 0..LEASE_RENEWALS {
        sleep(interval).await;
        if !matches!(timeout(publish_timeout, publish_progress()).await, Ok(true)) && !warned {
            tracing::warn!("Procurement delivery lease renewal unavailable");
            warned = true;
        }
    }
    tracing::warn!("Procurement store still pending after delivery lease renewal budget");
}

async fn finish_store<T>(operation: impl Future<Output = T>, lease: impl Future<Output = ()>) -> T {
    // MongoDB 3.10 does not support cancelling in-flight operations by dropping their futures.
    // Even after the best-effort lease expires, keep this worker on this delivery until the
    // operation resolves. No detached task, next pull, or timeout-driven quarantine.
    tokio::pin!(operation);
    tokio::select! {
        biased;
        result = &mut operation => result,
        () = lease => operation.await,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Ack,
    Retry(Duration),
    Quarantine(&'static str),
}

fn record_action(delivered: i64, result: Result<(), Failure>) -> Action {
    match result {
        Ok(()) => Action::Ack,
        Err(Failure::ConflictingMessage) => Action::Quarantine("conflicting-message"),
        Err(Failure::ConflictingPlan) => Action::Quarantine("conflicting-plan"),
        Err(Failure::Unavailable) => {
            retry_delay(delivered).map_or(Action::Quarantine("retry-exhausted"), Action::Retry)
        }
    }
}

async fn persist_delivery(
    store: &Store,
    event: Option<AuthorizedPlan>,
    stream_sequence: u64,
    delivered: i64,
) -> AckKind {
    let action = match event {
        None => Action::Quarantine("invalid-or-forbidden-envelope"),
        // Only quarantine writes are retried beyond the final business attempt.
        Some(_) if delivered > RETRY.len() as i64 + 1 => Action::Quarantine("retry-exhausted"),
        Some(event) => record_action(delivered, store.record(&event).await),
    };
    let reason = match action {
        Action::Ack => return AckKind::Ack,
        Action::Retry(delay) => return AckKind::Nak(Some(delay)),
        Action::Quarantine(reason) => reason,
    };
    match store.quarantine(stream_sequence, delivered, reason).await {
        Ok(()) => {
            tracing::warn!(
                reason,
                stream_sequence,
                "Procurement quarantined an Inventory material plan"
            );
            AckKind::Term
        }
        Err(_) => {
            tracing::warn!("Procurement quarantine unavailable; delivery retained for retry");
            AckKind::Nak(Some(QUARANTINE_RETRY))
        }
    }
}

pub(super) async fn consume() -> io::Result<()> {
    let scope = configured_scope().map_err(io::Error::other)?;
    let filter = subject(scope).map_err(broker_unavailable)?;
    let store = Store::open().await?;
    let broker = connect("procurement_inventory").await?;
    let jetstream = broker.jetstream(Duration::from_secs(5));
    let stream: stream::Stream<()> = jetstream
        .operations()
        .get_stream_no_info(STREAM)
        .await
        .map_err(broker_unavailable)?;
    let durable: consumer::PullConsumer = stream
        .get_consumer(DURABLE)
        .await
        .map_err(broker_unavailable)?;
    check_policy(durable.cached_info(), &filter)?;
    let mut messages = durable
        .stream()
        .max_messages_per_batch(1)
        .messages()
        .await
        .map_err(broker_unavailable)?;
    while let Some(message) = messages.next().await {
        let message = message.map_err(broker_unavailable)?;
        let info = message.info().map_err(broker_unavailable)?;
        if info.stream != STREAM
            || info.consumer != DURABLE
            || info.stream_sequence == 0
            || info.delivered < 1
        {
            return Err(io::Error::other("Unexpected Procurement delivery metadata"));
        }
        let event = decode(
            scope,
            &filter,
            message.subject.as_str(),
            message.headers.as_ref(),
            &message.payload,
        );
        let ack = finish_store(
            persist_delivery(&store, event, info.stream_sequence, info.delivered),
            // Progress uses the same scoped reply subject as the final ACK, NAK or TERM.
            renew_lease(LEASE_INTERVAL, LEASE_PUBLISH_TIMEOUT, async || {
                message.ack_with(AckKind::Progress).await.is_ok()
            }),
        )
        .await;
        match ack {
            AckKind::Ack => message.double_ack().await,
            other => message.ack_with(other).await,
        }
        .map_err(broker_unavailable)?;
    }
    Err(io::Error::other("Procurement consumer stream ended"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp4us_messaging::consumer::IntoConsumerConfig;
    use futures_util::poll;
    use std::{cell::Cell, future::pending};
    use tokio::sync::oneshot;

    #[test]
    fn refuses_durable_with_nonmatching_delivery_policy() {
        let filter = "v1.business.ddcae139-c1e8-47d2-8b48-349cb6f30ce5.inventory.event.material-plan.netted.v1";
        let policy = durable_config(filter.into()).into_consumer_config();
        assert!(matches_policy(&policy, filter));
        for invalid in [
            consumer::Config {
                filter_subject: "v1.business.*.inventory.event.material-plan.netted.v1".into(),
                ..policy.clone()
            },
            consumer::Config {
                filter_subjects: vec![filter.into()],
                ..policy.clone()
            },
            consumer::Config {
                ack_policy: consumer::AckPolicy::None,
                ..policy.clone()
            },
            consumer::Config {
                max_deliver: 3,
                ..policy.clone()
            },
            consumer::Config {
                deliver_subject: Some("_INBOX.other".into()),
                ..policy.clone()
            },
        ] {
            assert!(!matches_policy(&invalid, filter));
        }
    }

    #[test]
    fn business_retry_backoff_is_bounded() {
        assert_eq!(retry_delay(0), None);
        assert_eq!(retry_delay(1), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(4), Some(Duration::from_secs(8)));
        assert_eq!(retry_delay(5), None);
        assert_eq!(retry_delay(6), None);
    }

    #[test]
    fn only_completed_success_can_ack_and_only_definite_failures_can_quarantine() {
        assert_eq!(record_action(5, Ok(())), Action::Ack);
        assert_eq!(
            record_action(4, Err(Failure::Unavailable)),
            Action::Retry(Duration::from_secs(8))
        );
        assert_eq!(
            record_action(5, Err(Failure::Unavailable)),
            Action::Quarantine("retry-exhausted")
        );
        assert_eq!(
            record_action(1, Err(Failure::ConflictingPlan)),
            Action::Quarantine("conflicting-plan")
        );
        assert_eq!(
            record_action(1, Err(Failure::ConflictingMessage)),
            Action::Quarantine("conflicting-message")
        );
    }

    #[tokio::test]
    async fn ending_the_lease_does_not_end_the_store_operation() {
        let (finish, operation) = oneshot::channel();
        let (expire, lease) = oneshot::channel();
        let delivery = finish_store(operation, async { lease.await.unwrap() });
        tokio::pin!(delivery);
        assert!(poll!(&mut delivery).is_pending());
        expire.send(()).unwrap();
        assert!(poll!(&mut delivery).is_pending());
        // The operation still owns the receiver, and its late result is authoritative.
        finish.send(Ok(())).unwrap();
        assert_eq!(record_action(5, delivery.await.unwrap()), Action::Ack);
    }

    #[tokio::test]
    async fn store_completion_does_not_wait_for_a_stalled_lease_publish() {
        let (finish, operation) = oneshot::channel();
        let renewing = Cell::new(false);
        let delivery = finish_store(operation, async {
            renewing.set(true);
            pending().await
        });
        tokio::pin!(delivery);
        assert!(poll!(&mut delivery).is_pending());
        assert!(renewing.get());
        finish.send(()).unwrap();
        delivery.await.unwrap();
    }

    #[tokio::test]
    async fn lease_publish_timeout_does_not_start_unbounded_renewals() {
        let publishes = Cell::new(0);
        renew_lease(Duration::ZERO, Duration::ZERO, async || {
            publishes.set(publishes.get() + 1);
            pending().await
        })
        .await;
        assert_eq!(publishes.get(), LEASE_RENEWALS);
    }
}
