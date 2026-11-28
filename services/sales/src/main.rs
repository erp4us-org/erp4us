mod catalog;
mod delivery;
mod domain;
mod store;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use domain::{CreateDraft, Draft};
use erp4us_identity::{Auth, Denial, Scope};
use mongodb::Client;
use serde::{Deserialize, Serialize};
use std::{env, io};
use store::{Confirm, ConfirmOrder, Insert, SavedOrder, Store};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

#[derive(Clone)]
struct Sales {
    auth: Auth,
    orders: Store,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    after: Option<Uuid>,
    limit: Option<u8>,
}

#[derive(Serialize)]
struct Page {
    items: Vec<Draft>,
    next_after: Option<Uuid>,
}

const READ: &str = "sales.order.read";
const CREATE: &str = "sales.order.draft.create";
const CONFIRM: &str = "sales.order.confirm";

fn scope(auth: &Auth, headers: &HeaderMap, permission: &str) -> Result<Scope, StatusCode> {
    auth.authorize(headers, permission)
        .map_err(|denied| match denied {
            Denial::Unauthenticated => StatusCode::UNAUTHORIZED,
            Denial::InvalidScope => StatusCode::BAD_REQUEST,
            Denial::Forbidden => StatusCode::FORBIDDEN,
        })
}

async fn create(
    State(app): State<Sales>,
    headers: HeaderMap,
    input: Result<Json<CreateDraft>, JsonRejection>,
) -> Result<(StatusCode, Json<Draft>), StatusCode> {
    let scope = scope(&app.auth, &headers, CREATE)?;
    let Json(input) = input.map_err(command_rejection)?;
    let quantity = input.quantity().ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    // An identical retry returns the original snapshots even if an upstream owner is down.
    if let Some(old) = app.orders.get(scope, input.id).await.map_err(storage)? {
        return retry(old, scope, &input);
    }
    let (customer, product) = catalog::resolve(&headers, scope, &input).await?;
    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let draft = Draft::new(&input, quantity, customer, product, created_at)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let order = SavedOrder::new(scope, input, draft);
    match app.orders.insert(&order).await.map_err(storage)? {
        Insert::Created => Ok((StatusCode::CREATED, Json(order.draft))),
        Insert::AlreadyExists => {
            let saved = app
                .orders
                .get(scope, order.draft.id)
                .await
                .map_err(storage)?
                .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
            if order.is_same_command_as(&saved) {
                Ok((StatusCode::OK, Json(saved.draft)))
            } else {
                Err(StatusCode::CONFLICT)
            }
        }
    }
}

fn command_rejection(err: JsonRejection) -> StatusCode {
    match err.status() {
        StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNSUPPORTED_MEDIA_TYPE => err.status(),
        _ => StatusCode::BAD_REQUEST,
    }
}

async fn confirm(
    State(app): State<Sales>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    input: Result<Json<ConfirmOrder>, JsonRejection>,
) -> Result<Json<Draft>, StatusCode> {
    let scope = scope(&app.auth, &headers, CONFIRM)?;
    let Json(input) = input.map_err(command_rejection)?;
    if id.is_nil() || input.command_id.is_nil() || input.expected_revision == 0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut values = headers.get_all("traceparent").iter();
    let trace = values
        .next()
        .and_then(|value| value.to_str().ok())
        .filter(|value| values.next().is_none() && erp4us_messaging::valid_traceparent(value));
    match app
        .orders
        .confirm(scope, id, input, trace.map(str::to_owned))
        .await
        .map_err(storage)?
    {
        Confirm::Saved(order) => Ok(Json(*order)),
        Confirm::Missing => Err(StatusCode::NOT_FOUND),
        Confirm::Conflict => Err(StatusCode::CONFLICT),
    }
}

fn retry(
    old: SavedOrder,
    scope: Scope,
    input: &CreateDraft,
) -> Result<(StatusCode, Json<Draft>), StatusCode> {
    if old.is_retry(scope, input) {
        Ok((StatusCode::OK, Json(old.draft)))
    } else {
        Err(StatusCode::CONFLICT)
    }
}

async fn detail(
    State(app): State<Sales>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Draft>, StatusCode> {
    let scope = scope(&app.auth, &headers, READ)?;
    if id.is_nil() {
        return Err(StatusCode::BAD_REQUEST);
    }
    app.orders
        .get(scope, id)
        .await
        .map_err(storage)?
        .map(|order| Json(order.view()))
        .ok_or(StatusCode::NOT_FOUND)
}

async fn list(
    State(app): State<Sales>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Result<Json<Page>, StatusCode> {
    let scope = scope(&app.auth, &headers, READ)?;
    let limit = query.limit.unwrap_or(25);
    if !(1..=50).contains(&limit) || query.after.is_some_and(|id| id.is_nil()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (items, next_after) = app
        .orders
        .page(scope, query.after, usize::from(limit))
        .await
        .map_err(storage)?;
    Ok(Json(Page { items, next_after }))
}

fn storage(_: mongodb::error::Error) -> StatusCode {
    StatusCode::SERVICE_UNAVAILABLE
}

fn required(key: &str) -> io::Result<String> {
    env::var(key).map_err(|_| io::Error::other(format!("{key} is required")))
}

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt().json().init();
    let relay = match env::args().nth(1).as_deref() {
        None => false,
        Some("relay") => true,
        Some(_) => return Err(io::Error::other("Unknown Sales role")),
    };
    let uri = required("SALES_MONGODB_URI")?;
    let db = Client::with_uri_str(uri)
        .await
        .map_err(|_| io::Error::other("Invalid Sales storage configuration"))?
        .database("sales");
    let orders = Store::open(db)
        .await
        .map_err(|_| io::Error::other("Could not initialize Sales storage"))?;
    if relay {
        return delivery::relay(orders).await;
    }
    let auth = Auth::load(
        &required("ERP4US_IDENTITY_POLICY")?,
        &required("ERP4US_ISSUER_JWKS")?,
    )?;
    let ready = orders.clone();
    let listener = tokio::net::TcpListener::bind(
        env::var("ERP4US_BIND").unwrap_or_else(|_| "127.0.0.1:8092".into()),
    )
    .await?;
    tracing::info!("Sales listening");
    axum::serve(
        listener,
        Router::new()
            .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/health/ready",
                get(move || async move {
                    if ready.ready().await {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }),
            )
            .route("/v1/sales/orders", get(list).post(create))
            .route("/v1/sales/orders/{id}", get(detail))
            .route("/v1/sales/orders/{id}/confirmation", post(confirm))
            .layer(DefaultBodyLimit::max(4096))
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
                    response.into_response()
                },
            ))
            .with_state(Sales { auth, orders }),
    )
    .await
}
