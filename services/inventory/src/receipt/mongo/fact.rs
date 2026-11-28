use super::{Reject, Saved};
use crate::netting::{Item, MAX_PIECES, positive};
use erp4us_messaging::{
    Actor, Contract, Delivery, Envelope, Error, Kind, Layer, Route, Scope, Version,
};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

/// Must come from verified actor context, not the receipt request.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ActorKind {
    User,
    Service,
    Agent,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Posted {
    receipt_id: Uuid,
    plan_id: Uuid,
    purchase_order_id: Uuid,
    purchase_line_id: Uuid,
    sku: String,
    #[serde(deserialize_with = "explicit_revision")]
    revision: Option<String>,
    unit: String,
    pieces: String,
    receipt_sequence: i32,
}

fn explicit_revision<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::deserialize(deserializer)
}

impl Contract for Posted {
    const ROUTE: Route<'static> = Route {
        layer: Layer::Business,
        domain: "inventory",
        kind: Kind::Event,
        resource: "purchase-receipt",
        action: "posted",
    };
    const SCHEMA_VERSION: &'static str = "1.0.0";
    const PROTOCOL_SUPPORT: &'static str = "=1.0.0";
    const SCHEMA_SUPPORT: &'static str = "=1.0.0";
    const DELIVERY: Delivery = Delivery::JetStream("ERP4US_BUSINESS_EVENTS_V1");

    fn validate(&self) -> Result<(), Error> {
        let item = Item {
            sku: self.sku.clone(),
            revision: self.revision.clone(),
        };
        if !item.valid()
            || self.unit != "EA"
            || positive(&self.pieces).is_none()
            || !(1..=MAX_PIECES).contains(&self.receipt_sequence)
            || [
                self.receipt_id,
                self.plan_id,
                self.purchase_order_id,
                self.purchase_line_id,
            ]
            .iter()
            .any(Uuid::is_nil)
        {
            return Err(Error::InvalidEnvelope("purchase receipt payload"));
        }
        Ok(())
    }
}

pub(super) fn encode(saved: &Saved, kind: ActorKind) -> Result<String, Reject> {
    let effect = &saved.effect;
    let command = effect.command;
    let row = &effect.line.binding.row.key;
    let message_id = saved.message_id;
    if message_id == command.receipt_id {
        return Err(Reject::InvalidState);
    }
    let timestamp = OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(saved.posted_at.timestamp_millis()) * 1_000_000,
    )
    .ok()
    .and_then(|time| time.format(&Rfc3339).ok())
    .ok_or(Reject::InvalidState)?;
    let kind = match kind {
        ActorKind::User => "user",
        ActorKind::Service => "service",
        ActorKind::Agent => "agent",
    };
    let event = Envelope {
        message_id,
        message_type: Posted::ROUTE
            .subject(1, Scope::Tenant(command.sales.scope.tenant), 1)
            .map_err(|_| Reject::InvalidState)?,
        protocol_version: Version::new(1, 0, 0),
        schema_version: Version::new(1, 0, 0),
        service_version: Version::parse(env!("CARGO_PKG_VERSION"))
            .map_err(|_| Reject::InvalidState)?,
        timestamp,
        source_service: "erp4us.inventory".into(),
        tenant_id: Some(command.sales.scope.tenant),
        organization_id: Some(command.sales.scope.organization),
        actor: Some(Actor {
            id: command.actor.to_string(),
            kind: kind.into(),
        }),
        correlation_id: message_id,
        causation_id: None,
        traceparent: None,
        tracestate: None,
        payload: Posted {
            receipt_id: command.receipt_id,
            plan_id: row.plan,
            purchase_order_id: command.purchase.order,
            purchase_line_id: command.purchase.line,
            sku: row.stock.item.sku.clone(),
            revision: row.stock.item.revision.clone(),
            unit: "EA".into(),
            pieces: command.pieces.to_string(),
            receipt_sequence: effect.line.sequence,
        },
    };
    event.validate().map_err(|_| Reject::InvalidState)?;
    serde_json::to_string(&event)
        .ok()
        .filter(|json| json.len() <= 7 * 1024)
        .ok_or(Reject::InvalidState)
}
