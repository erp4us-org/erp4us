use crate::preview::{self, Rule, Warehouse};
use erp4us_messaging::{
    AckKind, ConnectOptions, Connection, Contract, Delivery, Envelope, Error, Kind, Layer, Route,
    Scope, consumer, stream,
};
use futures_util::StreamExt;
use mongodb::bson::{DateTime, doc};
use serde::{Deserialize, Serialize};
use std::{io, time::Duration};
use uuid::Uuid;

const STREAM: &str = "ERP4US_BUSINESS_EVENTS_V1";
const DURABLE: &str = "inventory-sales-v1";
const SALES: &str = "erp4us.sales";
const ACK_WAIT: Duration = Duration::from_secs(30);
const RETRY: [u64; 4] = [1, 2, 4, 8];

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConfirmedOrder {
    pub order_id: Uuid,
    pub line_id: Uuid,
    revision: u32,
    pub sku: String,
    pub product_revision: String,
    unit: Unit,
    pieces: String,
}

#[derive(Deserialize, Serialize)]
enum Unit {
    #[serde(rename = "EA")]
    Each,
}

impl ConfirmedOrder {
    pub(super) fn quantity(&self) -> Option<i32> {
        if self.pieces.starts_with('0')
            || !(1..=4).contains(&self.pieces.len())
            || !self.pieces.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        self.pieces.parse().ok().filter(|n| (1..=1000).contains(n))
    }
}

impl Contract for ConfirmedOrder {
    const ROUTE: Route<'static> = Route {
        layer: Layer::Business,
        domain: "sales",
        kind: Kind::Event,
        resource: "order",
        action: "confirmed",
    };
    const SCHEMA_VERSION: &'static str = "1.0.0";
    const PROTOCOL_SUPPORT: &'static str = "=1.0.0";
    const SCHEMA_SUPPORT: &'static str = "=1.0.0";
    const DELIVERY: Delivery = Delivery::JetStream(STREAM);

    fn validate(&self) -> Result<(), Error> {
        if self.order_id.is_nil()
            || self.line_id.is_nil()
            || self.revision != 2
            || !(1..=64).contains(&self.sku.len())
            || !self
                .sku
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
            || !(1..=32).contains(&self.product_revision.len())
            || !self
                .product_revision
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || self.quantity().is_none()
        {
            return Err(Error::InvalidEnvelope("Sales confirmation payload"));
        }
        Ok(())
    }
}

fn permitted(warehouse: Warehouse, envelope: &Envelope<ConfirmedOrder>) -> bool {
    envelope.source_service == SALES
        && envelope.tenant_id == Some(warehouse.tenant)
        && envelope.organization_id == Some(warehouse.organization)
        && envelope
            .actor
            .as_ref()
            .is_some_and(|actor| Uuid::parse_str(&actor.id).is_ok_and(|id| !id.is_nil()))
}

async fn connect(user: &str) -> io::Result<Connection> {
    Connection::connect(
        &preview::required("NATS_URL")?,
        ConnectOptions::with_user_and_password(user.into(), preview::required("NATS_PASSWORD")?)
            .custom_inbox_prefix("_INBOX.inventory-sales")
            .connection_timeout(Duration::from_secs(3)),
    )
    .await
    .map_err(unavailable)
}

fn unavailable<E>(_: E) -> io::Error {
    io::Error::other("Inventory broker operation unavailable")
}

pub(super) async fn provision() -> io::Result<()> {
    let warehouse = Warehouse::load()?;
    let filter = ConfirmedOrder::ROUTE
        .subject(1, Scope::Tenant(warehouse.tenant), 1)
        .map_err(unavailable)?;
    let broker = connect("provisioner").await?;
    let jetstream = broker.jetstream(Duration::from_secs(5));
    let stream = jetstream
        .operations()
        .get_stream_no_info(STREAM)
        .await
        .map_err(unavailable)?;
    let durable = stream
        .get_or_create_consumer(
            DURABLE,
            consumer::pull::Config {
                durable_name: Some(DURABLE.into()),
                filter_subject: filter.clone(),
                deliver_policy: consumer::DeliverPolicy::All,
                ack_policy: consumer::AckPolicy::Explicit,
                ack_wait: ACK_WAIT,
                max_ack_pending: 1,
                max_deliver: -1,
                ..Default::default()
            },
        )
        .await
        .map_err(unavailable)?;
    let actual = &durable.cached_info().config;
    if actual.filter_subject != filter
        || !actual.filter_subjects.is_empty()
        || actual.ack_wait != ACK_WAIT
        || actual.ack_policy != consumer::AckPolicy::Explicit
        || actual.deliver_policy != consumer::DeliverPolicy::All
        || actual.max_ack_pending != 1
        || actual.max_deliver != -1
    {
        return Err(io::Error::other(
            "Inventory durable policy does not match configuration",
        ));
    }
    Ok(())
}

pub(super) async fn consume() -> io::Result<()> {
    let warehouse = Warehouse::load()?;
    let db = preview::open().await?;
    let broker = connect("inventory_sales").await?;
    let jetstream = broker.jetstream(Duration::from_secs(5));
    let stream: stream::Stream<()> = jetstream
        .operations()
        .get_stream_no_info(STREAM)
        .await
        .map_err(unavailable)?;
    let durable: consumer::PullConsumer =
        stream.get_consumer(DURABLE).await.map_err(unavailable)?;
    let mut messages = durable
        .stream()
        .max_messages_per_batch(1)
        .messages()
        .await
        .map_err(unavailable)?;
    while let Some(message) = messages.next().await {
        let message = message.map_err(unavailable)?;
        let info = message.info().map_err(unavailable)?;
        let event = message
            .headers
            .as_ref()
            .and_then(|headers| {
                Envelope::decode(message.subject.as_str(), headers, &message.payload).ok()
            })
            .filter(|event| permitted(warehouse, event));
        let reason = match event {
            Some(event) => match preview::reserve_confirmed(&db, warehouse, &event).await {
                Ok(()) => {
                    message.double_ack().await.map_err(unavailable)?;
                    continue;
                }
                Err(error) => match error.get_custom::<Rule>() {
                    Some(
                        Rule::ConflictingLine | Rule::ConflictingMessage | Rule::InvalidQuantity,
                    ) => "conflicting-confirmation",
                    _ => {
                        let delay = usize::try_from(info.delivered.saturating_sub(1))
                            .ok()
                            .and_then(|index| RETRY.get(index));
                        if let Some(delay) = delay {
                            message
                                .ack_with(AckKind::Nak(Some(Duration::from_secs(*delay))))
                                .await
                                .map_err(unavailable)?;
                            continue;
                        }
                        if matches!(error.get_custom::<Rule>(), Some(Rule::UnconfiguredStock)) {
                            "unconfigured-stock"
                        } else {
                            "inventory-retry-exhausted"
                        }
                    }
                },
            },
            None => "invalid-or-forbidden-envelope",
        };
        let key = format!("{STREAM}:{}", info.stream_sequence);
        let quarantine = db.collection::<mongodb::bson::Document>("sales_quarantine");
        if quarantine.update_one(doc! {"_id": key}, doc! {"$setOnInsert": {
            "reason": reason, "deliveryCount": info.delivered, "recordedAt": DateTime::now(),
            "schemaVersion": 1,
        }}).upsert(true).await.is_ok() {
            tracing::warn!(reason, "Inventory quarantined a Sales confirmation");
            message.ack_with(AckKind::Term).await.map_err(unavailable)?;
        } else {
            message.ack_with(AckKind::Nak(Some(Duration::from_secs(10))))
                .await.map_err(unavailable)?;
        }
    }
    Err(io::Error::other("Inventory consumer stream ended"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_wire_example_and_rejects_unconfigured_scope() {
        let mut envelope: Envelope<ConfirmedOrder> = serde_json::from_str(include_str!(
            "../../../contracts/examples/nats/sales-order-confirmed-v1.json"
        ))
        .unwrap();
        envelope.validate().unwrap();
        let warehouse = Warehouse {
            tenant: envelope.tenant_id.unwrap(),
            organization: envelope.organization_id.unwrap(),
            id: Uuid::nil(),
        };
        assert!(permitted(warehouse, &envelope));
        envelope.organization_id = Some(Uuid::nil());
        assert!(!permitted(warehouse, &envelope));
        envelope.payload.pieces = "1.0".into();
        assert!(envelope.validate().is_err());
    }
}
