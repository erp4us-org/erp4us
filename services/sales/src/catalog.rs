//! Resolve sales references through the owners' published, scoped HTTP APIs.
use crate::domain::{CreateDraft, Customer, Partner, Product};
use axum::http::{HeaderMap, StatusCode, header};
use erp4us_identity::Scope;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Request, body::Bytes};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use std::time::Duration;
use tokio::{net::TcpStream, task::JoinSet, time::timeout};

pub(crate) async fn resolve(
    headers: &HeaderMap,
    scope: Scope,
    draft: &CreateDraft,
) -> Result<(Customer, Product), StatusCode> {
    let partner_path = format!("/v1/partners/{}", draft.customer_id);
    let (partner, products) = tokio::try_join!(
        get::<Partner>("master-data", 8080, &partner_path, headers, scope),
        get::<Vec<Product>>(
            "manufacturing",
            8090,
            "/v1/manufacturing/products",
            headers,
            scope,
        )
    )?;
    let customer = partner
        .customer(draft.customer_id)
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    let product = products
        .into_iter()
        .find(|product| product.sku == draft.sku && product.revision == draft.product_revision)
        .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?;
    Ok((customer, product))
}

async fn get<T: DeserializeOwned>(
    host: &'static str,
    port: u16,
    path: &str,
    headers: &HeaderMap,
    scope: Scope,
) -> Result<T, StatusCode> {
    let unavailable = |_| StatusCode::SERVICE_UNAVAILABLE;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let request = Request::get(path)
        .header(header::HOST, host)
        .header(header::CONNECTION, "close")
        .header(header::AUTHORIZATION, bearer)
        .header("X-Erp4us-Tenant", scope.tenant.to_string())
        .header("X-Erp4us-Organization", scope.organization.to_string())
        .body(Empty::<Bytes>::new())
        .map_err(unavailable)?;
    timeout(Duration::from_secs(3), async move {
        let socket = TcpStream::connect((host, port))
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(socket))
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let mut task = JoinSet::new();
        task.spawn(connection);
        // No redirect following, arbitrary origin, or unbounded upstream body.
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        match response.status() {
            StatusCode::OK => {
                let bytes = Limited::new(response.into_body(), 65_536)
                    .collect()
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                    .to_bytes();
                serde_json::from_slice(&bytes).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
            }
            StatusCode::NOT_FOUND => Err(StatusCode::UNPROCESSABLE_ENTITY),
            status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) => Err(status),
            _ => Err(StatusCode::SERVICE_UNAVAILABLE),
        }
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
}
