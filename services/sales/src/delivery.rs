//! Publish the order-owned outbox through the shared JetStream wire library.
use crate::{
    domain::{Unit, pieces, valid_product_revision, valid_sku},
    required,
    store::Store,
};
use erp4us_identity::Scope as IdentityScope;
use erp4us_messaging::{
    Actor, ConnectOptions, Connection, Contract, Delivery, Envelope, Error, Kind, Layer, Route,
    Scope, Version,
};
use serde::{Deserialize, Serialize};
use std::{io, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const STREAM: &str = "ERP4US_BUSINESS_EVENTS_V1";
const SOURCE: &str = "erp4us.sales";
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfirmedOrder {
    pub order_id: Uuid,
    pub revision: u32,
    pub line_id: Uuid,
    pub sku: String,
    pub product_revision: String,
    pub unit: Unit,
    pub pieces: String,
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
            || !valid_sku(&self.sku)
            || !valid_product_revision(&self.product_revision)
            || pieces(&self.pieces).is_none()
        {
            return Err(Error::InvalidEnvelope("confirmed sales order"));
        }
        Ok(())
    }
}

pub(crate) fn event(
    scope: IdentityScope,
    payload: ConfirmedOrder,
    traceparent: Option<String>,
) -> Result<Envelope<ConfirmedOrder>, Error> {
    let message_id = Uuid::new_v4();
    let event = Envelope {
        message_id,
        message_type: ConfirmedOrder::ROUTE.subject(1, Scope::Tenant(scope.tenant), 1)?,
        protocol_version: Version::new(1, 0, 0),
        schema_version: Version::new(1, 0, 0),
        service_version: Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| Error::InvalidContract)?,
        timestamp: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|_| Error::InvalidEnvelope("timestamp"))?,
        source_service: SOURCE.into(),
        tenant_id: Some(scope.tenant),
        organization_id: Some(scope.organization),
        actor: Some(Actor {
            id: scope.actor.to_string(),
            kind: "user".into(),
        }),
        correlation_id: message_id,
        causation_id: None,
        traceparent,
        tracestate: None,
        payload,
    };
    event.validate()?;
    Ok(event)
}

pub(crate) async fn relay(orders: Store) -> io::Result<()> {
    #[derive(Debug)]
    enum Stage {
        Read,
        Invalid,
        Timeout,
        Publish,
        Mark,
    }

    let tenant = Uuid::parse_str(&required("SALES_RELAY_TENANT")?)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(|| io::Error::other("Invalid Sales relay tenant"))?;
    let broker = Connection::connect(
        &required("NATS_URL")?,
        ConnectOptions::with_user_and_password("sales_relay".into(), required("NATS_PASSWORD")?)
            .custom_inbox_prefix("_INBOX.sales-relay")
            .connection_timeout(Duration::from_secs(2))
            .retry_on_initial_connect(),
    )
    .await
    .map_err(|_| io::Error::other("Could not initialize Sales broker connection"))?;
    let jetstream = broker.jetstream(PUBLISH_TIMEOUT);
    let idle = Duration::from_millis(300);
    let mut backoff = idle;
    loop {
        let result: Result<bool, Stage> = async {
            if let Some(pending) = orders.next_event(tenant).await.map_err(|_| Stage::Read)? {
                let envelope = pending.envelope().map_err(|_| Stage::Invalid)?;
                timeout(PUBLISH_TIMEOUT, jetstream.publish(&envelope))
                    .await
                    .map_err(|_| Stage::Timeout)?
                    .map_err(|_| Stage::Publish)?;
                orders.mark_sent(&pending).await.map_err(|_| Stage::Mark)?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        .await;
        match result {
            Ok(true) => backoff = idle,
            Ok(false) => {
                backoff = idle;
                sleep(idle).await;
            }
            Err(stage) => {
                tracing::warn!(?stage, "Sales outbox delivery pending; backing off");
                sleep(backoff).await;
                backoff = (backoff * 2).min(PUBLISH_TIMEOUT);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_the_published_order_event_fixture() {
        let encoded =
            include_str!("../../../contracts/examples/nats/sales-order-confirmed-v1.json");
        let mut message: Envelope<ConfirmedOrder> = serde_json::from_str(encoded).unwrap();
        assert!(message.validate().is_ok());
        assert_eq!(message.payload.pieces, "10");
        message.payload.pieces = "10.0".into();
        assert!(message.validate().is_err());
    }
}
