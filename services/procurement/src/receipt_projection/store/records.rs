use super::*;
use crate::material_plan::{canonical_id, canonical_uuid, required_revision};
use erp4us_messaging::Version;
use serde::{Deserialize, Serialize};

pub(super) const SCHEMA: i32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LineId {
    #[serde(with = "canonical_id")]
    tenant_id: Uuid,
    #[serde(with = "canonical_id")]
    organization_id: Uuid,
    #[serde(with = "canonical_id")]
    plan_id: Uuid,
    #[serde(with = "canonical_id")]
    purchase_order_id: Uuid,
    #[serde(with = "canonical_id")]
    purchase_line_id: Uuid,
}

impl LineId {
    pub(super) fn key(self) -> Document {
        doc! {
            "tenantId": self.tenant_id.to_string(), "organizationId": self.organization_id.to_string(),
            "planId": self.plan_id.to_string(), "purchaseOrderId": self.purchase_order_id.to_string(),
            "purchaseLineId": self.purchase_line_id.to_string(),
        }
    }

    fn scope(self) -> Scope {
        Scope {
            tenant_id: self.tenant_id,
            organization_id: self.organization_id,
        }
    }
}

impl From<LineKey<'_>> for LineId {
    fn from(key: LineKey<'_>) -> Self {
        Self {
            tenant_id: key.tenant_id,
            organization_id: key.organization_id,
            plan_id: key.plan_id,
            purchase_order_id: key.purchase_order_id,
            purchase_line_id: key.purchase_line_id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Pin {
    sku: String,
    #[serde(deserialize_with = "required_revision")]
    revision: Option<String>,
    unit: String,
}

impl Pin {
    fn line<'a>(&'a self, id: LineId) -> LineKey<'a> {
        LineKey {
            tenant_id: id.tenant_id,
            organization_id: id.organization_id,
            plan_id: id.plan_id,
            purchase_order_id: id.purchase_order_id,
            purchase_line_id: id.purchase_line_id,
            sku: &self.sku,
            revision: self.revision.as_deref(),
            unit: &self.unit,
        }
    }
}

impl From<LineKey<'_>> for Pin {
    fn from(line: LineKey<'_>) -> Self {
        Self {
            sku: line.sku.into(),
            revision: line.revision.map(str::to_owned),
            unit: line.unit.into(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MessageKey {
    #[serde(with = "canonical_id")]
    tenant_id: Uuid,
    #[serde(with = "canonical_id")]
    organization_id: Uuid,
    #[serde(with = "canonical_id")]
    message_id: Uuid,
}

pub(super) fn message_filter(scope: Scope, id: Uuid) -> Document {
    doc! {"_id": {
        "tenantId": scope.tenant_id.to_string(), "organizationId": scope.organization_id.to_string(),
        "messageId": id.to_string(),
    }}
}

pub(super) fn receipt_filter(scope: Scope, id: Uuid) -> Document {
    doc! {
        "_id.tenantId": scope.tenant_id.to_string(),
        "_id.organizationId": scope.organization_id.to_string(),
        "receiptId": id.to_string(),
    }
}

pub(super) fn sequence_filter(line: LineId, sequence: u32) -> Document {
    doc! {"line": line.key(), "sequence": sequence}
}

/// Only immutable owner attributes that matter for equality. The decoded
/// root correlation equals the message ID; the contract pins source and
/// wire versions. JSON formatting, trace context and transport metadata are
/// not business identity. Compare the validated posting timestamp losslessly.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Attestation {
    #[serde(with = "canonical_id")]
    actor_id: Uuid,
    actor_kind: String,
    posted_at: String,
    service_version: String,
}

impl Attestation {
    fn encode(&self) -> Option<Vec<u8>> {
        let version = Version::parse(&self.service_version).ok()?;
        if !version.pre.is_empty()
            || !version.build.is_empty()
            || version.to_string() != self.service_version
            || self.actor_id.is_nil()
            || !matches!(self.actor_kind.as_str(), "user" | "service" | "agent")
            || DateTime::parse_rfc3339_str(&self.posted_at).is_err()
        {
            return None;
        }
        bson::to_vec(self).ok()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Inbox {
    #[serde(rename = "_id")]
    key: MessageKey,
    schema_version: i32,
    pub(super) line: LineId,
    pub(super) pin: Pin,
    #[serde(with = "canonical_id")]
    receipt_id: Uuid,
    sequence: u32,
    pieces: String,
    owner: Attestation,
}

pub(super) struct Prepared {
    pub(super) inbox: Inbox,
    owner_content: Vec<u8>,
}

impl Prepared {
    /// The caller has already run the scoped wire decoder and separately
    /// authorized the Inventory publication principal for this scope.
    pub(super) fn decoded(scope: Scope, event: &Envelope<ReceiptPosted>) -> Result<Self, Failure> {
        let actor = event.actor.as_ref().ok_or(Failure::InvalidFact)?;
        let owner = Attestation {
            actor_id: canonical_uuid(&actor.id).ok_or(Failure::InvalidFact)?,
            actor_kind: actor.kind.clone(),
            posted_at: event.timestamp.clone(),
            service_version: event.service_version.to_string(),
        };
        let owner_content = owner.encode().ok_or(Failure::InvalidFact)?;
        let fact = VerifiedReceipt::from_decoded(scope, event, &owner_content)
            .map_err(|_| Failure::InvalidFact)?;
        Ok(Self {
            inbox: Inbox {
                key: MessageKey {
                    tenant_id: scope.tenant_id,
                    organization_id: scope.organization_id,
                    message_id: fact.id.message_id,
                },
                schema_version: SCHEMA,
                line: fact.line.into(),
                pin: fact.line.into(),
                receipt_id: fact.id.receipt_id,
                sequence: fact.id.sequence,
                pieces: event.payload.pieces.clone(),
                owner,
            },
            owner_content,
        })
    }

    pub(super) fn stored(inbox: Inbox) -> Result<Self, Failure> {
        let owner_content = inbox.owner.encode().ok_or(Failure::Unavailable)?;
        let prepared = Self {
            inbox,
            owner_content,
        };
        prepared.fact()?;
        Ok(prepared)
    }

    pub(super) fn fact(&self) -> Result<VerifiedReceipt<'_>, Failure> {
        let inbox = &self.inbox;
        if inbox.schema_version != SCHEMA {
            return Err(Failure::Unavailable);
        }
        VerifiedReceipt::from_verified(
            Scope {
                tenant_id: inbox.key.tenant_id,
                organization_id: inbox.key.organization_id,
            },
            inbox.pin.line(inbox.line),
            Identity {
                message_id: inbox.key.message_id,
                receipt_id: inbox.receipt_id,
                sequence: inbox.sequence,
            },
            &inbox.pieces,
            &self.owner_content,
        )
        .map_err(|_| Failure::Unavailable)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Reason {
    Message,
    Receipt,
    Sequence,
    OverOrdered,
    InconsistentStage,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Quarantine {
    reason: Reason,
    #[serde(with = "canonical_id")]
    message_id: Uuid,
    #[serde(with = "canonical_id")]
    receipt_id: Uuid,
}

impl Quarantine {
    pub(super) fn new(reason: Reason, id: Identity) -> Self {
        Self {
            reason,
            message_id: id.message_id,
            receipt_id: id.receipt_id,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Summary {
    #[serde(rename = "_id")]
    pub(super) line: LineId,
    schema_version: i32,
    pin: Pin,
    ordered: u32,
    highest_known: u32,
    admitted_count: u32,
    admitted_pieces: u32,
    through: u32,
    received: u32,
    pub(super) projected_at: Option<DateTime>,
    pub(super) pending: bool,
    pub(super) quarantine: Option<Quarantine>,
}

impl Summary {
    pub(super) fn initial(issued: IssuedLine<'_>) -> Self {
        Self {
            schema_version: SCHEMA,
            line: issued.key.into(),
            pin: issued.key.into(),
            ordered: issued.ordered,
            highest_known: 0,
            admitted_count: 0,
            admitted_pieces: 0,
            through: 0,
            received: 0,
            projected_at: None,
            pending: false,
            quarantine: None,
        }
    }

    pub(super) fn projection<'a>(&self, issued: IssuedLine<'a>) -> Result<Projection<'a>, Failure> {
        // Check persisted scalar invariants before restoring arithmetic state.
        let valid = self.schema_version == SCHEMA
            && self.pin.line(self.line) == issued.key
            && self.ordered == issued.ordered
            && self.highest_known <= MAX_SEQUENCE
            && self.through <= self.admitted_count
            && self.admitted_count <= self.highest_known
            && self.received <= self.admitted_pieces
            && self.admitted_pieces <= self.ordered
            && self.admitted_pieces + (self.highest_known - self.admitted_count) <= self.ordered
            && self.through <= self.received
            && self.admitted_count - self.through <= self.admitted_pieces - self.received
            && ((self.through == 0) == (self.received == 0))
            && ((self.through > 0) == self.projected_at.is_some())
            && (self.through < self.highest_known || self.received == self.admitted_pieces)
            && (!self.pending || (self.through < self.highest_known && self.quarantine.is_none()));
        if !valid {
            return Err(Failure::Unavailable);
        }
        Ok(Projection {
            line: issued.key,
            ordered: self.ordered,
            highest_known: self.highest_known,
            admitted_count: self.admitted_count,
            admitted_pieces: self.admitted_pieces,
            through: self.through,
            received: self.received,
            blocked: self.quarantine.is_some(),
        })
    }

    pub(super) fn save_prefix(&mut self, projection: &Projection<'_>, pending: bool) {
        if self.through < projection.through {
            self.projected_at = Some(DateTime::now());
        }
        self.highest_known = projection.highest_known;
        self.admitted_count = projection.admitted_count;
        self.admitted_pieces = projection.admitted_pieces;
        self.through = projection.through;
        self.received = projection.received;
        self.pending = pending;
    }

    /// An inbox fact is already bound to a positively verified historical
    /// issued tuple. A collision may only block that exact saved pin.
    pub(super) fn check_link(&self, inbox: &Inbox) -> Result<(), Failure> {
        let key = inbox.pin.line(inbox.line);
        let issued = IssuedLine::new(inbox.line.scope(), key, &self.ordered.to_string())
            .map_err(|_| Failure::Unavailable)?;
        self.projection(issued)?;
        Ok(())
    }

    pub(super) fn block(&mut self, marker: Quarantine) {
        self.quarantine.get_or_insert(marker);
        self.pending = false;
    }
}
