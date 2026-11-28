use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateDraft {
    #[serde(with = "uuid::serde::hyphenated")]
    pub id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    pub customer_id: Uuid,
    pub sku: String,
    pub product_revision: String,
    pub pieces: String,
}

impl CreateDraft {
    pub(crate) fn quantity(&self) -> Option<u16> {
        if self.id.is_nil()
            || self.customer_id.is_nil()
            || !valid_sku(&self.sku)
            || !valid_product_revision(&self.product_revision)
        {
            return None;
        }
        pieces(&self.pieces)
    }
}

pub(crate) fn valid_sku(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

pub(crate) fn valid_product_revision(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub(crate) fn pieces(value: &str) -> Option<u16> {
    if !(1..=4).contains(&value.len())
        || value.starts_with('0')
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    value.parse().ok().filter(|n| (1..=1000).contains(n))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Address {
    street: String,
    postal_code: String,
    city: String,
    country: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Profile {
    contact: String,
    address: Address,
    delivery_address: Address,
    payment_days: u16,
    currency: String,
    language: String,
}

#[derive(Deserialize)]
pub(crate) struct Partner {
    id: Uuid,
    revision: u32,
    code: String,
    name: String,
    roles: Vec<String>,
    profile: Option<Profile>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Customer {
    #[serde(with = "uuid::serde::hyphenated")]
    id: Uuid,
    revision: u32,
    code: String,
    name: String,
    profile: Profile,
}

impl Partner {
    pub(crate) fn customer(self, expected: Uuid) -> Option<Customer> {
        let profile = self.profile.filter(|profile| profile.currency == "EUR")?;
        (self.id == expected
            && self.revision > 0
            && self.roles.iter().any(|role| role == "customer"))
        .then_some(Customer {
            id: self.id,
            revision: self.revision,
            code: self.code,
            name: self.name,
            profile,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct Product {
    pub sku: String,
    pub revision: String,
    name: String,
    price_eur: String,
}

#[derive(Deserialize, Serialize)]
struct Line {
    #[serde(with = "uuid::serde::hyphenated")]
    id: Uuid,
    sku: String,
    product_revision: String,
    name: String,
    unit: Unit,
    pieces: String,
    unit_price: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub(crate) enum Unit {
    #[serde(rename = "EA")]
    Each,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Draft,
    Confirmed,
}

#[derive(Deserialize, Serialize)]
enum Currency {
    #[serde(rename = "EUR")]
    Eur,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Draft {
    #[serde(with = "uuid::serde::hyphenated")]
    pub id: Uuid,
    status: Status,
    revision: u32,
    created_at: String,
    customer: Customer,
    line: Line,
    currency: Currency,
    total: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirmed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirmation_event_id: Option<Uuid>,
}

// Unit prices are capped at 9,999,999.99 EUR and at most 1000 EA, so totals
// fit the public ten-digit euro limit. No binary floating point or rounding.
fn unit_cents(decimal: &str) -> Option<u64> {
    let (euros, cents) = decimal.split_once('.')?;
    if !(1..=7).contains(&euros.len())
        || (euros.len() > 1 && euros.starts_with('0'))
        || cents.len() != 2
        || !euros
            .bytes()
            .chain(cents.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some(euros.parse::<u64>().ok()? * 100 + cents.parse::<u64>().ok()?)
}

impl Draft {
    pub(crate) fn new(
        input: &CreateDraft,
        quantity: u16,
        customer: Customer,
        product: Product,
        created_at: String,
    ) -> Option<Self> {
        let total = unit_cents(&product.price_eur)?.checked_mul(u64::from(quantity))?;
        Some(Self {
            id: input.id,
            status: Status::Draft,
            revision: 1,
            created_at,
            customer,
            line: Line {
                id: Uuid::new_v4(),
                sku: product.sku,
                product_revision: product.revision,
                name: product.name,
                unit: Unit::Each,
                pieces: input.pieces.clone(),
                unit_price: product.price_eur,
            },
            currency: Currency::Eur,
            total: format!("{}.{:02}", total / 100, total % 100),
            confirmed_at: None,
            confirmation_event_id: None,
        })
    }

    pub(crate) fn confirmation_payload(&self) -> crate::delivery::ConfirmedOrder {
        crate::delivery::ConfirmedOrder {
            order_id: self.id,
            revision: 2,
            line_id: self.line.id,
            sku: self.line.sku.clone(),
            product_revision: self.line.product_revision.clone(),
            unit: self.line.unit,
            pieces: self.line.pieces.clone(),
        }
    }

    pub(crate) fn confirmed(mut self, timestamp: String, message_id: Uuid) -> Self {
        self.status = Status::Confirmed;
        self.revision = 2;
        self.confirmed_at = Some(timestamp);
        self.confirmation_event_id = Some(message_id);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_price_uses_exact_bounded_cents() {
        assert_eq!(unit_cents("2490.00"), Some(249_000));
        assert_eq!(unit_cents("0.07"), Some(7));
        for invalid in ["2490", "02.00", "2.999", "-1.00", "+1.00", "10000000.00"] {
            assert_eq!(unit_cents(invalid), None);
        }
    }

    #[test]
    fn quantity_requires_canonical_whole_pieces() {
        let mut draft = CreateDraft {
            id: Uuid::new_v4(),
            customer_id: Uuid::new_v4(),
            sku: "NM-1000".into(),
            product_revision: "A".into(),
            pieces: "10".into(),
        };
        assert_eq!(draft.quantity(), Some(10));
        for invalid in ["0", "010", "1001", "+10", "10.0", "1e1", " 10"] {
            draft.pieces = invalid.into();
            assert_eq!(draft.quantity(), None);
        }
    }
}
