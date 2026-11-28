mod domain;
mod http;
mod partners;
mod store;
mod units;

use std::{env, io, net::SocketAddr};
use tokio::{net::TcpListener, signal};

#[tokio::main]
async fn main() -> io::Result<()> {
    tracing_subscriber::fmt().json().init();
    let uri = env::var("MASTER_DATA_MONGODB_URI")
        .map_err(|_| io::Error::other("MASTER_DATA_MONGODB_URI is required"))?;
    if let Some(command) = env::args().nth(1) {
        if command != "seed-preview-partners" {
            return Err(io::Error::other("Unknown Master Data command"));
        }
        let scope = erp4us_identity::Scope {
            tenant: seed_scope("ERP4US_SEED_TENANT")?,
            organization: seed_scope("ERP4US_SEED_ORGANIZATION")?,
            actor: seed_scope("ERP4US_SEED_ACTOR")?,
        };
        let store = store::Store::open(&uri)
            .await
            .map_err(|_| io::Error::other("Could not initialize the master-data database"))?;
        return partners::seed_preview(scope, &store).await;
    }
    let identity_policy = env::var("ERP4US_IDENTITY_POLICY")
        .map_err(|_| io::Error::other("ERP4US_IDENTITY_POLICY is required"))?;
    let issuer_keys = env::var("ERP4US_ISSUER_JWKS")
        .map_err(|_| io::Error::other("ERP4US_ISSUER_JWKS is required"))?;
    let auth = erp4us_identity::Auth::load(&identity_policy, &issuer_keys)?;
    let store = store::Store::open(&uri)
        .await
        .map_err(|_| io::Error::other("Could not initialize the master-data database"))?;
    let address = env::var("ERP4US_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let address: SocketAddr = address
        .parse()
        .map_err(|_| io::Error::other("ERP4US_BIND must be a socket address"))?;
    let listener = TcpListener::bind(address).await?;
    tracing::info!(%address, "Master Data listening");
    axum::serve(listener, http::router(store, auth))
        .with_graceful_shutdown(shutdown())
        .await
}

fn seed_scope(key: &str) -> io::Result<uuid::Uuid> {
    env::var(key)
        .ok()
        .and_then(|value| uuid::Uuid::parse_str(&value).ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(|| io::Error::other(format!("Missing or invalid {key}")))
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    signal::ctrl_c().await.expect("install interrupt handler");
}
