use crate::{
    Scope, approval_store,
    budget_approvals::{self, Approval},
    budget_policy::{JsonObject, Policy},
    drafts::{Draft, Status, Store},
    material_plan::{Bom, BuyRow, canonical_uuid},
    partners,
    purchase_batch::{Intent as IssueIntent, IssueFailure, IssuedBatch},
    purchase_store, required,
    terms::{Command, Decision, Failure, Terms},
    terms_store,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    routing::get,
};
use erp4us_identity::{Auth, Denial, ORGANIZATION_HEADER, TENANT_HEADER};
use serde::Serialize;
use std::{env, future::Future, io, sync::Arc, time::Duration};
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout_at},
};
use uuid::Uuid;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const SAVE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_OPERATIONS: usize = 16;
const TERMS_BODY_LIMIT: usize = 16 * 1024;
const APPROVAL_BODY_LIMIT: usize = 2 * 1024;
const ISSUE_BODY_LIMIT: usize = 2 * 1024;
const READ_PERMISSION: &str = "procurement.proposal.read";
const WRITE_TERMS_PERMISSION: &str = "procurement.proposal.terms.write";
const READ_PARTNERS_PERMISSION: &str = "master-data.partner.read";
const READ_PURCHASE_PERMISSION: &str = "procurement.purchase-order.read";
const ISSUE_PURCHASE_PERMISSION: &str = "procurement.purchase-order.issue";

#[derive(Clone)]
struct Api {
    store: Store,
    terms: terms_store::Store,
    approvals: approval_store::Store,
    budget_policy: Option<Arc<Policy>>,
    auth: Auth,
    operations: Arc<Semaphore>,
}

#[derive(Serialize)]
struct Proposal {
    proposal_id: Uuid,
    plan_id: Uuid,
    order_id: Uuid,
    line_id: Uuid,
    status: Status,
    bom: Bom,
    lines: Vec<BuyRow>,
}

impl From<Draft> for Proposal {
    fn from(draft: Draft) -> Self {
        Self {
            proposal_id: draft.proposal_id,
            plan_id: draft.plan.plan_id,
            order_id: draft.plan.order_id,
            line_id: draft.plan.line_id,
            status: draft.status,
            bom: draft.plan.bom,
            lines: draft.plan.buy_rows,
        }
    }
}

fn authorize(
    auth: &Auth,
    headers: &HeaderMap,
    permission: &str,
) -> Result<erp4us_identity::Scope, StatusCode> {
    let scope = auth
        .authorize(headers, permission)
        .map_err(|error| match error {
            Denial::Unauthenticated => StatusCode::UNAUTHORIZED,
            Denial::InvalidScope => StatusCode::BAD_REQUEST,
            Denial::Forbidden => StatusCode::FORBIDDEN,
        })?;
    if scope.tenant.is_nil() || scope.organization.is_nil() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(scope)
}

impl From<erp4us_identity::Scope> for Scope {
    fn from(scope: erp4us_identity::Scope) -> Self {
        Self {
            tenant_id: scope.tenant,
            organization_id: scope.organization,
        }
    }
}

fn plan_id(value: &str) -> Result<Uuid, StatusCode> {
    Uuid::try_parse(value)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(StatusCode::BAD_REQUEST)
}

impl From<Failure> for StatusCode {
    fn from(error: Failure) -> Self {
        match error {
            Failure::Invalid => Self::BAD_REQUEST,
            Failure::Conflict => Self::CONFLICT,
            Failure::Unavailable => Self::SERVICE_UNAVAILABLE,
        }
    }
}

impl From<partners::Failure> for StatusCode {
    fn from(error: partners::Failure) -> Self {
        match error {
            partners::Failure::Invalid => Self::BAD_REQUEST,
            partners::Failure::Unauthenticated => Self::UNAUTHORIZED,
            partners::Failure::Forbidden => Self::FORBIDDEN,
            partners::Failure::Unavailable => Self::SERVICE_UNAVAILABLE,
        }
    }
}

impl From<budget_approvals::Failure> for StatusCode {
    fn from(error: budget_approvals::Failure) -> Self {
        match error {
            budget_approvals::Failure::Invalid => Self::BAD_REQUEST,
            budget_approvals::Failure::Forbidden => Self::FORBIDDEN,
            budget_approvals::Failure::NotFound => Self::NOT_FOUND,
            budget_approvals::Failure::Conflict => Self::CONFLICT,
            budget_approvals::Failure::Unavailable => Self::SERVICE_UNAVAILABLE,
        }
    }
}

impl From<IssueFailure> for StatusCode {
    fn from(error: IssueFailure) -> Self {
        match error {
            IssueFailure::Invalid => Self::BAD_REQUEST,
            IssueFailure::Forbidden => Self::FORBIDDEN,
            IssueFailure::NotFound => Self::NOT_FOUND,
            IssueFailure::Conflict => Self::CONFLICT,
            IssueFailure::Unavailable => Self::SERVICE_UNAVAILABLE,
        }
    }
}

async fn bounded<T: Send + 'static>(
    operations: &Arc<Semaphore>,
    deadline: Instant,
    task: impl Future<Output = Result<T, StatusCode>> + Send + 'static,
) -> Result<T, StatusCode> {
    let permit = operations
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Timing out a request must not drop MongoDB driver futures. The detached
    // operation retains its slot until it ends, even when the client disconnects.
    let handle = tokio::spawn(async move {
        let _permit = permit;
        before(deadline)?;
        task.await
    });
    timeout_at(deadline, handle)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
}

fn before(deadline: Instant) -> Result<(), StatusCode> {
    if Instant::now() >= deadline {
        Err(StatusCode::SERVICE_UNAVAILABLE)
    } else {
        Ok(())
    }
}

async fn lookup(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(plan): Path<String>,
) -> Result<Json<Proposal>, StatusCode> {
    let scope = authorize(&api.auth, &headers, READ_PERMISSION)?;
    let id = plan_id(&plan)?;
    let draft = bounded(
        &api.operations,
        Instant::now() + LOOKUP_TIMEOUT,
        async move {
            api.store
                .get(scope.into(), id)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                .ok_or(StatusCode::NOT_FOUND)
        },
    )
    .await?;
    Ok(Json(draft.into()))
}

async fn latest_terms(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(plan): Path<String>,
) -> Result<Json<Terms>, StatusCode> {
    let scope = authorize(&api.auth, &headers, READ_PERMISSION)?;
    let id = plan_id(&plan)?;
    let terms = bounded(
        &api.operations,
        Instant::now() + LOOKUP_TIMEOUT,
        async move {
            api.terms
                .latest(scope.into(), id)
                .await?
                .ok_or(StatusCode::NOT_FOUND)
        },
    )
    .await?;
    Ok(Json(terms))
}

async fn save_terms(
    State(api): State<Api>,
    Path(plan): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<Terms>), StatusCode> {
    let headers = request.headers();
    let identity = authorize(&api.auth, headers, WRITE_TERMS_PERMISSION)?;
    authorize(&api.auth, headers, READ_PARTNERS_PERMISSION)?;
    if identity.actor.is_nil() {
        return Err(StatusCode::FORBIDDEN);
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .cloned()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let plan_id = plan_id(&plan)?;
    let Json(command) = Json::<Command>::from_request(request, &api)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let command = command.normalize()?;
    let scope = identity.into();
    let deadline = Instant::now() + SAVE_TIMEOUT;

    bounded(&api.operations, deadline, async move {
        let draft = api
            .store
            .get(scope, plan_id)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .ok_or(StatusCode::NOT_FOUND)?;
        let revision = match api.terms.decide(scope, plan_id, &command).await? {
            Decision::Replay(terms) => return Ok((StatusCode::OK, Json(terms))),
            Decision::Create(revision) => revision,
        };
        before(deadline)?;
        let bound = command.bind(&draft)?;
        let suppliers = match partners::resolve(&bearer, scope, bound.supplier_ids()).await {
            Ok(suppliers) => suppliers,
            Err(partners::Failure::Unavailable) => {
                // A concurrent attempt may already have saved this command.
                return resolve_race(&api.terms, scope, plan_id, &command).await;
            }
            Err(error) => return Err(error.into()),
        };
        let saved = bound.snapshot(revision, &suppliers, identity.actor)?;
        before(deadline)?;
        match api.terms.append(scope, &command, saved, deadline).await? {
            terms_store::Save::Created(terms) => Ok((StatusCode::CREATED, Json(terms))),
            terms_store::Save::Replay(terms) => Ok((StatusCode::OK, Json(terms))),
        }
    })
    .await
}

async fn resolve_race(
    store: &terms_store::Store,
    scope: Scope,
    plan_id: Uuid,
    command: &Command,
) -> Result<(StatusCode, Json<Terms>), StatusCode> {
    match store.decide(scope, plan_id, command).await? {
        Decision::Replay(terms) => Ok((StatusCode::OK, Json(terms))),
        Decision::Create(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn latest_approval(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(plan): Path<String>,
) -> Result<Json<Approval>, StatusCode> {
    let identity = authorize(&api.auth, &headers, READ_PERMISSION)?;
    let plan_id = plan_id(&plan)?;
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    bounded(&api.operations, deadline, async move {
        let result = api
            .approvals
            .get(
                &api.terms,
                api.budget_policy.as_deref(),
                identity.into(),
                plan_id,
                deadline,
            )
            .await?;
        Ok(Json(result))
    })
    .await
}

async fn record_approval(
    State(api): State<Api>,
    Path(plan): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<Approval>), StatusCode> {
    authorize(&api.auth, request.headers(), READ_PERMISSION)?;
    let plan_id = plan_id(&plan)?;
    let headers = request.headers().clone();
    let Json(JsonObject(intent)) =
        Json::<JsonObject<budget_approvals::Intent>>::from_request(request, &api)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST)?;
    let command = intent.authorizing_operation()?;
    let identity = authorize(&api.auth, &headers, command.permission())?;
    if identity.actor.is_nil() {
        return Err(StatusCode::FORBIDDEN);
    }
    let deadline = Instant::now() + SAVE_TIMEOUT;
    bounded(&api.operations, deadline, async move {
        match api
            .approvals
            .append(
                &api.terms,
                api.budget_policy.as_deref(),
                identity,
                plan_id,
                &command,
                deadline,
            )
            .await?
        {
            approval_store::Saved::Created(result) => Ok((StatusCode::CREATED, Json(result))),
            approval_store::Saved::Replay(result) => Ok((StatusCode::OK, Json(result))),
        }
    })
    .await
}

fn purchase_identity(
    auth: &Auth,
    headers: &HeaderMap,
    issue: bool,
) -> Result<erp4us_identity::Scope, StatusCode> {
    let identity = authorize(auth, headers, READ_PURCHASE_PERMISSION)?;
    if issue {
        authorize(auth, headers, ISSUE_PURCHASE_PERMISSION)?;
    }
    if identity.actor.is_nil() {
        return Err(StatusCode::FORBIDDEN);
    }
    for name in [TENANT_HEADER, ORGANIZATION_HEADER] {
        if headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(canonical_uuid)
            .is_none()
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    Ok(identity)
}

async fn get_purchase_batch(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(plan): Path<String>,
) -> Result<Json<IssuedBatch>, StatusCode> {
    let identity = purchase_identity(&api.auth, &headers, false)?;
    let plan_id = canonical_uuid(&plan).ok_or(StatusCode::BAD_REQUEST)?;
    bounded(
        &api.operations,
        Instant::now() + LOOKUP_TIMEOUT,
        async move {
            let batch = api.terms.purchases().get(identity.into(), plan_id).await?;
            Ok(Json(batch.try_into()?))
        },
    )
    .await
}

async fn issue_purchase_batch(
    State(api): State<Api>,
    Path(plan): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<IssuedBatch>), StatusCode> {
    let identity = purchase_identity(&api.auth, request.headers(), true)?;
    let plan_id = canonical_uuid(&plan).ok_or(StatusCode::BAD_REQUEST)?;
    let Json(JsonObject(intent)) = Json::<JsonObject<IssueIntent>>::from_request(request, &api)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let issue_intent = intent.validate()?;
    let deadline = Instant::now() + SAVE_TIMEOUT;
    bounded(&api.operations, deadline, async move {
        let saved = api
            .terms
            .purchases()
            .issue(
                purchase_store::Issue {
                    drafts: &api.store,
                    terms: &api.terms,
                    approvals: &api.approvals,
                    policy: api.budget_policy.as_deref(),
                    identity,
                    plan_id,
                    intent: issue_intent,
                },
                deadline,
            )
            .await?;
        let (status, batch) = match saved {
            purchase_store::Saved::Created(batch) => (StatusCode::CREATED, batch),
            purchase_store::Saved::Replay(batch) => (StatusCode::OK, batch),
        };
        Ok((status, Json(batch.try_into()?)))
    })
    .await
}

pub(super) async fn serve() -> io::Result<()> {
    let auth = Auth::load(
        &required("ERP4US_IDENTITY_POLICY")?,
        &required("ERP4US_ISSUER_JWKS")?,
    )?;
    let budget_policy = budget_approvals::load_policy()?.map(Arc::new);
    let store = Store::open().await?;
    let terms = store.open_terms().await?;
    let approvals = store.open_approvals().await?;
    let ready = store.clone();
    let operations = Arc::new(Semaphore::new(MAX_OPERATIONS));
    let ready_operations = operations.clone();
    let bind = env::var("ERP4US_BIND").unwrap_or_else(|_| {
        let port = env::var("ERP4US_PROCUREMENT_PORT").unwrap_or_else(|_| "8094".into());
        format!("127.0.0.1:{port}")
    });
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(
        listener,
        Router::new()
            .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
            .route(
                "/health/ready",
                get(move || async move {
                    bounded(
                        &ready_operations,
                        Instant::now() + LOOKUP_TIMEOUT,
                        async move {
                            if ready.ready().await {
                                Ok(StatusCode::NO_CONTENT)
                            } else {
                                Err(StatusCode::SERVICE_UNAVAILABLE)
                            }
                        },
                    )
                    .await
                }),
            )
            .route("/v1/procurement/proposals/by-plan/{plan_id}", get(lookup))
            .route(
                "/v1/procurement/proposals/by-plan/{plan_id}/terms",
                get(latest_terms)
                    .post(save_terms)
                    .layer(DefaultBodyLimit::max(TERMS_BODY_LIMIT)),
            )
            .route(
                "/v1/procurement/proposals/by-plan/{plan_id}/approval",
                get(latest_approval)
                    .post(record_approval)
                    .layer(DefaultBodyLimit::max(APPROVAL_BODY_LIMIT)),
            )
            .route(
                "/v1/procurement/proposals/by-plan/{plan_id}/purchase-orders",
                get(get_purchase_batch)
                    .post(issue_purchase_batch)
                    .layer(DefaultBodyLimit::max(ISSUE_BODY_LIMIT)),
            )
            .layer(middleware::from_fn(
                |request: Request, next: Next| async move {
                    let mut response = next.run(request).await;
                    let status = response.status();
                    let headers = response.headers_mut();
                    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                    if status == StatusCode::UNAUTHORIZED {
                        headers
                            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                    }
                    response
                },
            ))
            .with_state(Api {
                store,
                terms,
                approvals,
                budget_policy,
                auth,
                operations,
            }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material_plan::tests::{read, sample};
    use mongodb::bson::{DateTime, doc, from_document, to_document};
    use serde_json::json;
    use tokio::time::timeout;

    #[test]
    fn approval_failures_map_to_contract_status_without_outcome_leaks() {
        use budget_approvals::Failure::*;
        for (failure, code) in [
            (Invalid, StatusCode::BAD_REQUEST),
            (Forbidden, StatusCode::FORBIDDEN),
            (NotFound, StatusCode::NOT_FOUND),
            (Conflict, StatusCode::CONFLICT),
            (Unavailable, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            assert_eq!(StatusCode::from(failure), code);
        }
    }

    #[test]
    fn issuance_failures_map_to_contract_status_without_outcome_leaks() {
        use IssueFailure::*;
        for (failure, code) in [
            (Invalid, StatusCode::BAD_REQUEST),
            (Forbidden, StatusCode::FORBIDDEN),
            (NotFound, StatusCode::NOT_FOUND),
            (Conflict, StatusCode::CONFLICT),
            (Unavailable, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            assert_eq!(StatusCode::from(failure), code);
        }
    }

    #[tokio::test]
    async fn deadline_leaves_in_flight_work_running_and_bounded() {
        let slots = Arc::new(Semaphore::new(1));
        let (release, waiting) = tokio::sync::oneshot::channel();
        let (complete, completed) = tokio::sync::oneshot::channel();
        let result = bounded(
            &slots,
            Instant::now() + Duration::from_millis(50),
            async move {
                waiting.await.unwrap();
                complete.send(()).unwrap();
                Ok(())
            },
        )
        .await;
        assert_eq!(result, Err(StatusCode::SERVICE_UNAVAILABLE));
        assert_eq!(slots.available_permits(), 0);
        release.send(()).unwrap();
        timeout(Duration::from_secs(1), completed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before(Instant::now()), Err(StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn public_response_has_only_draft_snapshot_fields() {
        let sample = sample();
        let event = read(&sample).unwrap();
        let proposal_id = Uuid::new_v4();
        let draft: Draft = from_document(doc! {
            "proposalId": proposal_id.to_string(),
            "status": "draft",
            "plan": to_document(&event.plan).unwrap(),
            "sourceEventId": event.message_id.to_string(),
            "origin": to_document(&event.origin).unwrap(),
            "createdAt": DateTime::now(),
            "schemaVersion": 1,
        })
        .unwrap();
        let response = serde_json::to_value(Proposal::from(draft)).unwrap();
        assert_eq!(
            response,
            json!({
                "proposal_id": proposal_id,
                "plan_id": sample["payload"]["planId"],
                "order_id": sample["payload"]["orderId"],
                "line_id": sample["payload"]["lineId"],
                "status": "draft",
                "bom": {"sku":"NM-1000","revision":"A"},
                "lines": [
                    {"sku":"NM-MOTOR","revision":null,"unit":"EA","pieces":"6"},
                    {"sku":"NM-SPINDLE","revision":null,"unit":"EA","pieces":"10"},
                    {"sku":"NM-SPINDLE","revision":"B","unit":"EA","pieces":"2"}
                ]
            })
        );
    }
}
