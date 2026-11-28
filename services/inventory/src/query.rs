use crate::{line_key, material_plan, preview};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use erp4us_identity::{Auth, Denial, Scope};
use mongodb::{
    Database,
    bson::{Document, doc},
};
use serde::Serialize;
use std::{env, io};
use uuid::Uuid;

#[derive(Clone)]
pub(super) struct Inventory {
    pub db: Database,
    pub auth: Auth,
}

pub(super) fn authorize(
    auth: &Auth,
    headers: &HeaderMap,
    permission: &str,
) -> Result<Scope, StatusCode> {
    auth.authorize(headers, permission)
        .map_err(|error| match error {
            Denial::Unauthenticated => StatusCode::UNAUTHORIZED,
            Denial::InvalidScope => StatusCode::BAD_REQUEST,
            Denial::Forbidden => StatusCode::FORBIDDEN,
        })
}

#[derive(Serialize)]
struct Reservation {
    order_id: Uuid,
    line_id: Uuid,
    warehouse_id: Uuid,
    sku: String,
    product_revision: String,
    unit: &'static str,
    requested: String,
    reserved: String,
    outstanding: String,
    source_event_id: Uuid,
}

fn view(order_id: Uuid, line_id: Uuid, row: &Document) -> Option<Reservation> {
    let stock = row.get_document("stock").ok()?;
    if stock.get_str("unit") != Ok("EA") {
        return None;
    }
    let quantity = |name| {
        row.get_i32(name)
            .ok()
            .filter(|n| (0..=1000).contains(n))
            .map(|n| n.to_string())
    };
    Some(Reservation {
        order_id,
        line_id,
        warehouse_id: Uuid::parse_str(stock.get_str("warehouseId").ok()?).ok()?,
        sku: stock.get_str("catalogSku").ok()?.into(),
        product_revision: stock.get_str("productRevision").ok()?.into(),
        unit: "EA",
        requested: quantity("requested")?,
        reserved: quantity("reserved")?,
        outstanding: quantity("outstanding")?,
        source_event_id: Uuid::parse_str(row.get_str("sourceEventId").ok()?).ok()?,
    })
}

async fn lookup(
    State(state): State<Inventory>,
    headers: HeaderMap,
    Path((order, line)): Path<(Uuid, Uuid)>,
) -> Result<Json<Reservation>, StatusCode> {
    let scope = authorize(&state.auth, &headers, "inventory.reservation.read")?;
    if order.is_nil() || line.is_nil() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let key = line_key(
        &preview::scoped(scope.tenant, scope.organization),
        order,
        line,
    );
    let row = preview::reservations(&state.db)
        .find_one(doc! {"_id": key, "sourceEventId": {"$exists": true}})
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .ok_or(StatusCode::NOT_FOUND)?;
    view(order, line, &row)
        .map(Json)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

pub(super) async fn serve() -> io::Result<()> {
    let auth = Auth::load(
        &preview::required("ERP4US_IDENTITY_POLICY")?,
        &preview::required("ERP4US_ISSUER_JWKS")?,
    )?;
    let db = preview::open().await?;
    let ready = db.clone();
    let bind = env::var("ERP4US_BIND").unwrap_or_else(|_| "127.0.0.1:8093".into());
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(
        listener,
        Router::new()
            .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/health/ready",
                get(move || async move {
                    if ready.run_command(doc! {"ping": 1}).await.is_ok() {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }),
            )
            .route(
                "/v1/inventory/reservations/{order_id}/{line_id}",
                get(lookup),
            )
            .route(
                "/v1/inventory/reservations/{order_id}/{line_id}/material-plan",
                get(material_plan::get).post(material_plan::create),
            )
            .route(
                "/v1/inventory/reservations/{order_id}/{line_id}/material-plan/procurement-handoff",
                get(material_plan::handoff::get).post(material_plan::handoff::send),
            )
            .layer(DefaultBodyLimit::max(1024))
            .layer(middleware::from_fn(
                |request: Request, next: Next| async move {
                    let mut response: Response = next.run(request).await;
                    response
                        .headers_mut()
                        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                    if response.status() == StatusCode::UNAUTHORIZED {
                        response
                            .headers_mut()
                            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                    }
                    response
                },
            ))
            .with_state(Inventory { db, auth }),
    )
    .await
}
