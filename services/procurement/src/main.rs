//! Procurement draft runtime and the trusted local stdin fixture adapter.
mod approval_store;
mod budget_approvals;
mod budget_policy;
mod consumer;
mod drafts;
mod material_plan;
mod partners;
mod purchase_batch;
mod purchase_store;
mod query;
mod receipt_projection;
#[cfg(test)]
mod receipt_wire;
mod store;
mod terms;
mod terms_store;

use mongodb::Client;
use serde::{Deserialize, Serialize};
use std::{
    env,
    io::{self, Read},
    process::ExitCode,
};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(tag = "contract", deny_unknown_fields, rename_all_fields = "camelCase")]
enum Input {
    #[serde(rename = "stock-shortage.v1")]
    Shortage {
        shortage_id: Uuid,
        order_id: Uuid,
        line_id: Uuid,
        article_id: Uuid,
        unit: Unit,
        missing_quantity: u32,
    },
    #[serde(rename = "get-draft-proposal.v1")]
    Get { shortage_id: Uuid },
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Clone, Copy)]
enum Unit {
    #[serde(rename = "EA")]
    Each,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Shortage {
    #[serde(with = "uuid::serde::hyphenated")]
    shortage_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    order_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    line_id: Uuid,
    #[serde(with = "uuid::serde::hyphenated")]
    article_id: Uuid,
    unit: Unit,
    quantity: i32,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Status {
    Draft,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Proposal {
    #[serde(with = "uuid::serde::hyphenated")]
    proposal_id: Uuid,
    status: Status,
    #[serde(flatten)]
    source: Shortage,
}

#[derive(Clone, Copy)]
struct Scope {
    tenant_id: Uuid,
    organization_id: Uuid,
}

fn configured_scope() -> Result<Scope, &'static str> {
    let id = |key| {
        env::var(key)
            .ok()
            .and_then(|value| Uuid::parse_str(&value).ok())
            .filter(|id| !id.is_nil())
            .ok_or("missing or invalid Procurement scope")
    };
    Ok(Scope {
        tenant_id: id("PROCUREMENT_TENANT_ID")?,
        organization_id: id("PROCUREMENT_ORGANIZATION_ID")?,
    })
}

async fn run() -> Result<(), &'static str> {
    let scope = configured_scope()?;
    let uri = env::var("PROCUREMENT_MONGO_URI").map_err(|_| "missing Procurement connection")?;
    let mut bytes = Vec::new();
    io::stdin()
        .take(8193)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read Procurement input")?;
    if bytes.len() > 8192 {
        return Err("Procurement input too large");
    }
    let input: Input = serde_json::from_slice(&bytes).map_err(|_| "invalid Procurement input")?;
    let database = Client::with_uri_str(uri)
        .await
        .map_err(|_| "invalid Procurement connection")?
        .database("procurement");
    let store = store::Store::new(&database, scope);
    let proposal = match input {
        Input::Shortage {
            shortage_id,
            order_id,
            line_id,
            article_id,
            unit,
            missing_quantity,
        } => {
            let quantity = i32::try_from(missing_quantity).map_err(|_| "quantity out of range")?;
            if quantity == 0 {
                None
            } else {
                Some(
                    store
                        .record(Shortage {
                            shortage_id,
                            order_id,
                            line_id,
                            article_id,
                            unit,
                            quantity,
                        })
                        .await
                        .map_err(store::Failure::message)?,
                )
            }
        }
        Input::Get { shortage_id } => store
            .get(shortage_id)
            .await
            .map_err(store::Failure::message)?,
    };
    println!(
        "{}",
        serde_json::to_string(&proposal).map_err(|_| "cannot encode Procurement result")?
    );
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let mode = env::args().nth(1);
    if mode.is_some() {
        tracing_subscriber::fmt().json().init();
    }
    let outcome = match mode.as_deref() {
        None => run().await.map_err(io::Error::other),
        Some("serve") => query::serve().await,
        Some("provision-consumer") => consumer::provision().await,
        Some("consume") => consumer::consume().await,
        Some(_) => Err(io::Error::other("Unknown Procurement role")),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn required(key: &str) -> io::Result<String> {
    env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| io::Error::other(format!("Missing {key}")))
}
