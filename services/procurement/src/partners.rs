//! Verify supplier choices through the owning API, never through another service's database.
use crate::{Scope, material_plan::canonical_id, terms::Supplier};
use axum::http::{HeaderValue, StatusCode, header};
use erp4us_identity::{ORGANIZATION_HEADER, TENANT_HEADER};
use futures_util::{StreamExt, TryStreamExt, stream};
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Request, body::Bytes};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::{net::TcpStream, task::JoinSet, time::timeout};
use uuid::Uuid;

// Fixed Compose owner, as in Sales. No client-selected endpoint or redirect.
const OWNER: (&str, u16) = ("master-data", 8080);
const OWNER_DEADLINE: Duration = Duration::from_secs(3);
const PARALLEL_LOOKUPS: usize = 8;
const MAX_RESPONSE_BYTES: usize = 65_536;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Failure {
    Invalid,
    Unauthenticated,
    Forbidden,
    Unavailable,
}

#[derive(Deserialize)]
struct Partner {
    #[serde(with = "canonical_id")]
    id: Uuid,
    #[serde(with = "canonical_id")]
    tenant_id: Uuid,
    #[serde(with = "canonical_id")]
    organization_id: Uuid,
    code: String,
    name: String,
    roles: Vec<Role>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Role {
    Customer,
    Supplier,
}

fn decode_supplier(body: &[u8], id: Uuid, scope: Scope) -> Result<Supplier, Failure> {
    let partner: Partner = serde_json::from_slice(body).map_err(|_| Failure::Unavailable)?;
    if (partner.id, partner.tenant_id, partner.organization_id)
        != (id, scope.tenant_id, scope.organization_id)
    {
        return Err(Failure::Invalid);
    }
    match partner.roles.as_slice() {
        [Role::Supplier] | [Role::Supplier, Role::Customer] | [Role::Customer, Role::Supplier] => {}
        [Role::Customer] => return Err(Failure::Invalid),
        _ => return Err(Failure::Unavailable),
    }
    if !(1..=32).contains(&partner.code.len())
        || !partner
            .code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
        || !(1..=120).contains(&partner.name.chars().count())
        || partner.name.trim().is_empty()
        || partner.name.chars().any(char::is_control)
    {
        return Err(Failure::Unavailable);
    }
    Ok(Supplier {
        id,
        code: partner.code,
        name: partner.name,
    })
}

pub(super) async fn resolve(
    bearer: &HeaderValue,
    scope: Scope,
    ids: impl Iterator<Item = Uuid>,
) -> Result<HashMap<Uuid, Supplier>, Failure> {
    let unique: HashSet<_> = ids.collect();
    timeout(
        OWNER_DEADLINE,
        stream::iter(unique)
            .map(|id| async move {
                let supplier = get(bearer, scope, id).await?;
                Ok((id, supplier))
            })
            .buffer_unordered(PARALLEL_LOOKUPS)
            .try_collect(),
    )
    .await
    .map_err(|_| Failure::Unavailable)?
}

async fn get(bearer: &HeaderValue, scope: Scope, id: Uuid) -> Result<Supplier, Failure> {
    let request = Request::get(format!("/v1/partners/{id}"))
        .header(header::HOST, format!("{}:{}", OWNER.0, OWNER.1))
        .header(header::CONNECTION, "close")
        .header(header::AUTHORIZATION, bearer)
        .header(TENANT_HEADER, scope.tenant_id.to_string())
        .header(ORGANIZATION_HEADER, scope.organization_id.to_string())
        .body(Empty::<Bytes>::new())
        .map_err(|_| Failure::Unavailable)?;
    let socket = TcpStream::connect(OWNER)
        .await
        .map_err(|_| Failure::Unavailable)?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
        .await
        .map_err(|_| Failure::Unavailable)?;
    // Abort the connection if this lookup is cancelled or reaches its deadline.
    let mut tasks = JoinSet::new();
    tasks.spawn(connection);
    let response = sender
        .send_request(request)
        .await
        .map_err(|_| Failure::Unavailable)?;
    match response.status() {
        StatusCode::OK => {
            let bytes = Limited::new(response.into_body(), MAX_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|_| Failure::Unavailable)?
                .to_bytes();
            decode_supplier(&bytes, id, scope)
        }
        StatusCode::NOT_FOUND => Err(Failure::Invalid),
        StatusCode::UNAUTHORIZED => Err(Failure::Unauthenticated),
        StatusCode::FORBIDDEN => Err(Failure::Forbidden),
        _ => Err(Failure::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshots_only_the_scoped_supplier_and_checks_published_roles() {
        let id = Uuid::new_v4();
        let scope = Scope {
            tenant_id: Uuid::new_v4(),
            organization_id: Uuid::new_v4(),
        };
        let mut partner = json!({
            "id": id,
            "tenant_id": scope.tenant_id,
            "organization_id": scope.organization_id,
            "code": "SUP-1",
            "name": "Industrial Components",
            "roles": ["supplier"],
            "profile": {"contact":"Not retained"},
        });
        let decode = |partner: &serde_json::Value| {
            decode_supplier(&serde_json::to_vec(partner).unwrap(), id, scope)
        };
        let supplier = json!({"id": id, "code": "SUP-1", "name": "Industrial Components"});
        assert_eq!(
            serde_json::to_value(decode(&partner).unwrap()).unwrap(),
            supplier
        );
        partner["roles"] = json!(["customer", "supplier"]);
        assert!(decode(&partner).is_ok());
        for key in ["id", "tenant_id", "organization_id"] {
            let mut foreign = partner.clone();
            foreign[key] = json!(Uuid::new_v4());
            assert_eq!(decode(&foreign), Err(Failure::Invalid));
        }
        partner["roles"] = json!(["customer"]);
        assert_eq!(decode(&partner), Err(Failure::Invalid));
        for roles in [
            json!(["Supplier"]),
            json!(["supplier", "supplier"]),
            json!([]),
            json!(null),
        ] {
            partner["roles"] = roles;
            assert_eq!(decode(&partner), Err(Failure::Unavailable));
        }
    }
}
