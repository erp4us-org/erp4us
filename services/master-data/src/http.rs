use crate::{
    domain::{Article, RegisterArticle, Scope},
    partners,
    store::{InsertError, Store},
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State, rejection::JsonRejection},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use erp4us_identity::{Auth, Denial};
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

pub(crate) fn router(store: Store, auth: Auth) -> Router {
    Router::new()
        .route(
            "/v1/articles:register",
            post(register).layer(middleware::from_fn_with_state(
                (auth.clone(), "master-data.article.register"),
                authorize,
            )),
        )
        .route(
            "/v1/articles/{id}",
            get(find).layer(middleware::from_fn_with_state(
                (auth.clone(), "master-data.article.read"),
                authorize,
            )),
        )
        .route(
            "/v1/units:register",
            post(crate::units::register).layer(middleware::from_fn_with_state(
                (auth.clone(), "master-data.unit.register"),
                authorize,
            )),
        )
        .route(
            "/v1/units/{id}",
            get(crate::units::find).layer(middleware::from_fn_with_state(
                (auth.clone(), "master-data.unit.read"),
                authorize,
            )),
        )
        .route(
            "/v1/partners",
            get(partners::list).layer(middleware::from_fn_with_state(
                (auth.clone(), "master-data.partner.read"),
                authorize,
            )),
        )
        .route(
            "/v1/partners:register",
            post(partners::register).layer(middleware::from_fn_with_state(
                (auth.clone(), "master-data.partner.register"),
                authorize,
            )),
        )
        .route(
            "/v1/partners/{id}",
            get(partners::find).layer(middleware::from_fn_with_state(
                (auth.clone(), "master-data.partner.read"),
                authorize,
            )),
        )
        .route(
            "/v1/session",
            get(move |headers: axum::http::HeaderMap| async move {
                let mut response = match auth.authenticate(&headers) {
                    Ok(principal) => Json(principal).into_response(),
                    Err(Denial::Unauthenticated) => (
                        [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
                        StatusCode::UNAUTHORIZED,
                    )
                        .into_response(),
                    Err(_) => StatusCode::FORBIDDEN.into_response(),
                };
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
            }),
        )
        .layer(DefaultBodyLimit::max(4096))
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(ready))
        .with_state(store)
}

async fn authorize(
    State((auth, permission)): State<(Auth, &'static str)>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut response = match auth.authorize(request.headers(), permission) {
        Ok(scope) => {
            request.extensions_mut().insert(scope);
            next.run(request).await
        }
        Err(Denial::Unauthenticated) => (
            [(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))],
            problem(
                StatusCode::UNAUTHORIZED,
                "A valid bearer access token is required",
            ),
        )
            .into_response(),
        Err(Denial::InvalidScope) => problem(
            StatusCode::BAD_REQUEST,
            "Valid tenant and organization headers are required",
        ),
        Err(Denial::Forbidden) => problem(
            StatusCode::FORBIDDEN,
            "Operation not permitted in the requested scope",
        ),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn ready(State(store): State<Store>) -> StatusCode {
    match store.ping().await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[derive(Serialize)]
struct ArticleView {
    id: Uuid,
    tenant_id: Uuid,
    organization_id: Uuid,
    created_by: Uuid,
    schema_version: u32,
    revision: u32,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    sku: String,
    name: String,
}

impl From<Article> for ArticleView {
    fn from(article: Article) -> Self {
        Self {
            id: article.id,
            tenant_id: article.tenant_id,
            organization_id: article.organization_id,
            created_by: article.created_by,
            schema_version: article.schema_version,
            revision: article.revision,
            created_at: article.created_at,
            sku: article.sku,
            name: article.name,
        }
    }
}

async fn register(
    State(store): State<Store>,
    Extension(scope): Extension<Scope>,
    body: Result<Json<RegisterArticle>, JsonRejection>,
) -> Response {
    let Json(command) = match body {
        Ok(body) => body,
        Err(rejection) => return problem(rejection.status(), "Invalid JSON request"),
    };
    let article = match Article::register(scope, command) {
        Ok(article) => article,
        Err(detail) => return problem(StatusCode::UNPROCESSABLE_ENTITY, detail),
    };
    let span_id = Uuid::new_v4().simple().to_string();
    let traceparent = format!("00-{}-{}-00", Uuid::new_v4().simple(), &span_id[..16]);
    match store.insert(&article, &traceparent).await {
        Ok(()) => {
            tracing::info!(%traceparent, "Article registration committed");
            (
                StatusCode::CREATED,
                [(header::LOCATION, format!("/v1/articles/{}", article.id))],
                [("traceparent", traceparent)],
                Json(ArticleView::from(article)),
            )
                .into_response()
        }
        Err(InsertError::Conflict) => problem(StatusCode::CONFLICT, "SKU already registered"),
        Err(InsertError::Database(error)) => unavailable("register_article", error),
    }
}

async fn find(
    State(store): State<Store>,
    Extension(scope): Extension<Scope>,
    Path(id): Path<String>,
) -> Response {
    let Ok(id) = Uuid::parse_str(&id) else {
        return problem(StatusCode::BAD_REQUEST, "Invalid article ID");
    };
    match store.get(scope, id).await {
        Ok(Some(article)) => Json(ArticleView::from(article)).into_response(),
        Ok(None) => problem(StatusCode::NOT_FOUND, "Article not found"),
        Err(error) => unavailable("find_article", error),
    }
}

pub(crate) fn unavailable(operation: &str, _error: mongodb::error::Error) -> Response {
    tracing::error!(operation, "MongoDB operation failed");
    problem(StatusCode::SERVICE_UNAVAILABLE, "Persistence unavailable")
}

#[derive(Serialize)]
struct Problem {
    status: u16,
    detail: &'static str,
}

pub(crate) fn problem(status: StatusCode, detail: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        Json(Problem {
            status: status.as_u16(),
            detail,
        }),
    )
        .into_response()
}
