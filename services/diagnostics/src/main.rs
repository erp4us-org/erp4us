//! Read-only application health, deliberately independent of the host runtime.
mod heartbeat;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use erp4us_identity::{Auth, Denial};
use erp4us_messaging::{
    AccountErrorKind, ConnectOptions, Connection, JetStream, State as BrokerState,
};
use http_body_util::Empty;
use hyper::{Request, body::Bytes};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use std::{
    env, io,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Semaphore,
    time::timeout,
};

const PERMISSION: &str = "platform.health.read";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct Diagnostics {
    auth: Auth,
    broker: Connection,
    heartbeat: heartbeat::Observer,
    probes: Arc<Semaphore>,
}

#[derive(Clone, Copy)]
struct Target {
    id: &'static str,
    host: &'static str,
    port: u16,
    path: &'static str,
    healthy_status: StatusCode,
}

// This small adapter probes only services from the trusted Compose deployment.
// Neither a browser-supplied URL nor an upstream response body leaves it.
const MASTER_DATA: Target = Target {
    id: "master-data",
    host: "master-data",
    port: 8080,
    path: "/health/ready",
    healthy_status: StatusCode::NO_CONTENT,
};
const MANUFACTURING: Target = Target {
    id: "manufacturing",
    host: "manufacturing",
    port: 8090,
    path: "/health/ready",
    healthy_status: StatusCode::NO_CONTENT,
};
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Readiness {
    Ready,
    Unready,
    Unreachable,
}

#[derive(Serialize)]
struct Observation {
    service: &'static str,
    readiness: Readiness,
}

#[derive(Serialize)]
struct Report {
    observed_at_unix_ms: u64,
    application_checks: [Observation; 3],
    jetstream: jetstream::Counters,
    article_relay_heartbeat: heartbeat::Report,
}

async fn status(target: Target) -> Option<StatusCode> {
    let stream = TcpStream::connect((target.host, target.port)).await.ok()?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    let request = Request::get(target.path)
        .header(header::HOST, target.host)
        .header(header::CONNECTION, "close")
        .body(Empty::<Bytes>::new())
        .ok()?;
    let mut task = tokio::task::JoinSet::new();
    task.spawn(connection);
    sender
        .send_request(request)
        .await
        .ok()
        .map(|response| response.status())
}

async fn observe(target: Target) -> Observation {
    let readiness = match timeout(PROBE_TIMEOUT, status(target)).await.ok().flatten() {
        Some(code) if code == target.healthy_status => Readiness::Ready,
        Some(_) => Readiness::Unready,
        None => Readiness::Unreachable,
    };
    Observation {
        service: target.id,
        readiness,
    }
}

async fn health(State(state): State<Diagnostics>, headers: HeaderMap) -> Response {
    let response = match state.auth.authorize_platform(&headers, PERMISSION) {
        Ok(()) => match state.probes.try_acquire() {
            Ok(_permit) => report(&state).await,
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        Err(Denial::Unauthenticated) => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response(),
        Err(_) => StatusCode::FORBIDDEN.into_response(),
    };
    ([(header::CACHE_CONTROL, "no-store")], response).into_response()
}

async fn report(state: &Diagnostics) -> Response {
    let (master_data, manufacturing, (jetstream_health, jetstream)) = tokio::join!(
        observe(MASTER_DATA),
        observe(MANUFACTURING),
        jetstream::observe(&state.broker)
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok());
    match now {
        Some(observed_at_unix_ms) => Json(Report {
            observed_at_unix_ms,
            application_checks: [master_data, manufacturing, jetstream_health],
            jetstream,
            article_relay_heartbeat: state.heartbeat.report(&state.broker),
        })
        .into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

mod jetstream {
    use super::*;

    const STREAM: &str = "ERP4US_BUSINESS_EVENTS_V1";
    const ARTICLE_RECEIPTS: &str = "article-receipts";
    const INVENTORY_SALES: &str = "inventory-sales-v1";
    const PROCUREMENT_MATERIAL_PLANS: &str = "procurement-material-plans-v1";

    #[derive(Default, Serialize)]
    pub(super) struct Counters {
        stream: Option<StreamCounters>,
        consumer: Option<ConsumerCounters>,
        pipeline_consumers: PipelineConsumers,
    }

    #[derive(Default, Serialize)]
    struct PipelineConsumers {
        inventory_sales: Option<ConsumerCounters>,
        procurement_material_plans: Option<ConsumerCounters>,
    }

    #[derive(Serialize)]
    struct StreamCounters {
        messages: u64,
        bytes: u64,
    }

    #[derive(Serialize)]
    struct ConsumerCounters {
        pending: u64,
        ack_pending: usize,
        redelivered: usize,
    }

    pub(super) async fn observe(broker: &Connection) -> (Observation, Counters) {
        let (readiness, counters) = if broker.state() == BrokerState::Connected {
            let context = broker.jetstream(PROBE_TIMEOUT);
            let (readiness, stream, receipts, inventory_sales, procurement_material_plans) = tokio::join!(
                account(&context),
                timeout(PROBE_TIMEOUT, stream(&context)),
                timeout(PROBE_TIMEOUT, consumer(&context, ARTICLE_RECEIPTS)),
                timeout(PROBE_TIMEOUT, consumer(&context, INVENTORY_SALES)),
                timeout(
                    PROBE_TIMEOUT,
                    consumer(&context, PROCUREMENT_MATERIAL_PLANS)
                )
            );
            (
                readiness,
                Counters {
                    stream: stream.ok().flatten(),
                    consumer: receipts.ok().flatten(),
                    pipeline_consumers: PipelineConsumers {
                        inventory_sales: inventory_sales.ok().flatten(),
                        procurement_material_plans: procurement_material_plans.ok().flatten(),
                    },
                },
            )
        } else {
            (Readiness::Unreachable, Counters::default())
        };
        (
            Observation {
                service: "jetstream",
                readiness,
            },
            counters,
        )
    }

    async fn account(context: &JetStream) -> Readiness {
        match timeout(PROBE_TIMEOUT, context.operations().query_account()).await {
            Ok(Ok(_)) => Readiness::Ready,
            Ok(Err(error))
                if matches!(
                    error.kind(),
                    AccountErrorKind::JetStreamUnavailable | AccountErrorKind::JetStream(_)
                ) =>
            {
                Readiness::Unready
            }
            _ => Readiness::Unreachable,
        }
    }

    async fn stream(context: &JetStream) -> Option<StreamCounters> {
        let stream = context.operations().get_stream(STREAM).await.ok()?;
        let state = &stream.cached_info().state;
        Some(StreamCounters {
            messages: state.messages,
            bytes: state.bytes,
        })
    }

    async fn consumer(context: &JetStream, name: &str) -> Option<ConsumerCounters> {
        let stream = context.operations().get_stream_no_info(STREAM).await.ok()?;
        let info = stream.consumer_info(name).await.ok()?;
        Some(ConsumerCounters {
            pending: info.num_pending,
            ack_pending: info.num_ack_pending,
            redelivered: info.num_redelivered,
        })
    }

    #[cfg(test)]
    #[test]
    fn pipeline_counts_leave_legacy_values_intact_and_distinguish_zero_from_absence() {
        let counters = Counters {
            stream: Some(StreamCounters {
                messages: 12,
                bytes: 1024,
            }),
            consumer: Some(ConsumerCounters {
                pending: 1,
                ack_pending: 2,
                redelivered: 3,
            }),
            pipeline_consumers: PipelineConsumers {
                inventory_sales: Some(ConsumerCounters {
                    pending: 0,
                    ack_pending: 4,
                    redelivered: 5,
                }),
                procurement_material_plans: None,
            },
        };
        assert_eq!(
            serde_json::to_value(counters).unwrap(),
            serde_json::json!({
                "stream": {"messages": 12, "bytes": 1024},
                "consumer": {"pending": 1, "ack_pending": 2, "redelivered": 3},
                "pipeline_consumers": {
                    "inventory_sales": {"pending": 0, "ack_pending": 4, "redelivered": 5},
                    "procurement_material_plans": null
                }
            })
        );
    }
}

fn required(name: &str) -> io::Result<String> {
    env::var(name).map_err(|_| io::Error::other(format!("{name} is required")))
}

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt().json().init();
    let auth = Auth::load(
        &required("ERP4US_IDENTITY_POLICY")?,
        &required("ERP4US_ISSUER_JWKS")?,
    )?;
    let (broker, heartbeat) = heartbeat::Observer::connect(
        &required("NATS_URL")?,
        ConnectOptions::with_user_and_password(
            "platform_diagnostics".into(),
            required("NATS_PASSWORD")?,
        )
        .custom_inbox_prefix("_INBOX.platform-diagnostics")
        .connection_timeout(PROBE_TIMEOUT)
        .retry_on_initial_connect(),
    )
    .await
    .map_err(io::Error::other)?;
    let listener =
        TcpListener::bind(env::var("ERP4US_BIND").unwrap_or_else(|_| "127.0.0.1:8091".into()))
            .await?;
    tracing::info!("Application diagnostics listening");
    axum::serve(
        listener,
        Router::new()
            .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
            .route("/v1/admin/application-health", get(health))
            .with_state(Diagnostics {
                auth,
                broker,
                heartbeat,
                probes: Arc::new(Semaphore::new(4)),
            }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn health_response_serializes_unavailable_measurements_as_null() {
        let report = Report {
            observed_at_unix_ms: 42,
            application_checks: [
                Observation {
                    service: "master-data",
                    readiness: Readiness::Ready,
                },
                Observation {
                    service: "manufacturing",
                    readiness: Readiness::Unready,
                },
                Observation {
                    service: "jetstream",
                    readiness: Readiness::Unreachable,
                },
            ],
            jetstream: jetstream::Counters::default(),
            article_relay_heartbeat: heartbeat::Report::default(),
        };
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({
                "observed_at_unix_ms": 42,
                "application_checks": [
                    {"service": "master-data", "readiness": "ready"},
                    {"service": "manufacturing", "readiness": "unready"},
                    {"service": "jetstream", "readiness": "unreachable"}
                ],
                "jetstream": {
                    "stream": null, "consumer": null,
                    "pipeline_consumers": {"inventory_sales": null, "procurement_material_plans": null}
                },
                "article_relay_heartbeat": {
                    "state": "unavailable", "interval_seconds": null, "instance_id": null, "age_seconds": null
                }
            })
        );
    }

    #[tokio::test]
    async fn distinguishes_error_responses_from_disconnected_services() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target {
            host: "127.0.0.1",
            port: listener.local_addr().unwrap().port(),
            ..MASTER_DATA
        };
        let job = tokio::spawn(async move {
            for wire in [
                b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".as_slice(),
                b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .as_slice(),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut input = [0u8; 512];
                let _ = socket.read(&mut input).await.unwrap();
                socket.write_all(wire).await.unwrap();
            }
        });
        assert_eq!(observe(target).await.readiness, Readiness::Ready);
        assert_eq!(observe(target).await.readiness, Readiness::Unready);
        job.await.unwrap();
        assert_eq!(observe(target).await.readiness, Readiness::Unreachable);
    }
}
