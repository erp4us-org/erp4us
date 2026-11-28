mod domain;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use domain::{Catalog, Demand, PlanError};
use erp4us_identity::{Auth, Denial};
use mongodb::{
    Client, Collection,
    bson::{self, Document, doc},
};
use serde::Deserialize;
use std::{env, io};
use uuid::Uuid;

const PLAN_READ: &str = "manufacturing.plan.read";

#[derive(Deserialize)]
struct Record {
    catalog: Catalog,
}

#[derive(Clone)]
struct App {
    auth: Auth,
    catalogs: Collection<Record>,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt().json().init();
    let uri = required("MANUFACTURING_MONGODB_URI")?;
    let database = Client::with_uri_str(uri)
        .await
        .map_err(storage_failure)?
        .database("manufacturing");
    let catalogs = database.collection("catalogs");
    match env::args().nth(1).as_deref() {
        Some("seed-preview") => seed(catalogs).await,
        None => {
            let auth = Auth::load(
                &required("ERP4US_IDENTITY_POLICY")?,
                &required("ERP4US_ISSUER_JWKS")?,
            )?;
            let bind = env::var("ERP4US_BIND").unwrap_or_else(|_| "127.0.0.1:8090".into());
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!("Manufacturing planning listening");
            axum::serve(
                listener,
                Router::new()
                    .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
                    .route(
                        "/health/ready",
                        get(move || async move {
                            match database.run_command(doc! {"ping": 1}).await {
                                Ok(_) => StatusCode::NO_CONTENT,
                                Err(_) => StatusCode::SERVICE_UNAVAILABLE,
                            }
                        }),
                    )
                    .route("/v1/manufacturing/products", get(products))
                    .route("/v1/manufacturing/requirements:preview", post(plan))
                    .layer(middleware::from_fn(
                        |request: Request, next: Next| async move {
                            let mut response: Response = next.run(request).await;
                            response.headers_mut().insert(
                                header::CACHE_CONTROL,
                                HeaderValue::from_static("no-store"),
                            );
                            if response.status() == StatusCode::UNAUTHORIZED {
                                response.headers_mut().insert(
                                    header::WWW_AUTHENTICATE,
                                    HeaderValue::from_static("Bearer"),
                                );
                            }
                            response
                        },
                    ))
                    .layer(DefaultBodyLimit::max(4096))
                    .with_state(App { auth, catalogs }),
            )
            .await
        }
        Some(_) => Err(io::Error::other("Unknown manufacturing command")),
    }
}

async fn seed(catalogs: Collection<Record>) -> io::Result<()> {
    let tenant = seed_scope("ERP4US_SEED_TENANT")?;
    let organization = seed_scope("ERP4US_SEED_ORGANIZATION")?;
    let catalog: Catalog =
        serde_json::from_str(include_str!("novamotion.json")).map_err(io::Error::other)?;
    catalog
        .validate()
        .map_err(|_| io::Error::other("Invalid manufacturing seed"))?;
    let payload = bson::to_document(&catalog).map_err(io::Error::other)?;
    catalogs
        .update_one(
            scope_filter(tenant, organization),
            doc! { "$setOnInsert": {
                "catalog": payload, "schemaVersion": 1,
            }},
        )
        .upsert(true)
        .await
        .map_err(storage_failure)?;
    tracing::info!("NovaMotion planning seed applied");
    Ok(())
}

fn seed_scope(key: &str) -> io::Result<Uuid> {
    Uuid::parse_str(&required(key)?)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(|| io::Error::other(format!("Invalid {key}")))
}

fn required(key: &str) -> io::Result<String> {
    env::var(key).map_err(|_| io::Error::other(format!("{key} is required")))
}

fn storage_failure(_: mongodb::error::Error) -> io::Error {
    io::Error::other("Manufacturing storage is unavailable")
}

fn scope_filter(tenant: Uuid, organization: Uuid) -> Document {
    doc! { "_id": { "tenant": tenant.to_string(), "organization": organization.to_string() } }
}

async fn load(app: App, headers: HeaderMap) -> Result<Catalog, StatusCode> {
    let scope = app
        .auth
        .authorize(&headers, PLAN_READ)
        .map_err(|denial| match denial {
            Denial::Unauthenticated => StatusCode::UNAUTHORIZED,
            Denial::InvalidScope => StatusCode::BAD_REQUEST,
            Denial::Forbidden => StatusCode::FORBIDDEN,
        })?;
    app.catalogs
        .find_one(scope_filter(scope.tenant, scope.organization))
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .map(|record| record.catalog)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn products(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Json<Vec<domain::Product>>, StatusCode> {
    Ok(Json(load(app, headers).await?.products))
}

async fn plan(
    State(app): State<App>,
    headers: HeaderMap,
    Json(request): Json<Demand>,
) -> Result<Json<domain::Plan>, StatusCode> {
    load(app, headers)
        .await?
        .plan(request)
        .map(Json)
        .map_err(|error| match error {
            PlanError::UnknownProduct => StatusCode::NOT_FOUND,
            PlanError::InvalidQuantity | PlanError::Overflow => StatusCode::UNPROCESSABLE_ENTITY,
            PlanError::Cycle | PlanError::InvalidStructure => StatusCode::CONFLICT,
        })
}
