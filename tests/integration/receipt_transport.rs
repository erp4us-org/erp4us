//! Shared setup for the two ignored receipt transport tests, not application configuration.

use erp4us_messaging::{ConnectOptions, Connection, JetStream};
use mongodb::{Client, Database};
use std::{num::NonZeroU16, time::Duration};
use uuid::Uuid;

pub(super) const STREAM_MARKER: &str = "Isolated ERP4US receipt transport proof";
pub(super) const WAIT: Duration = Duration::from_secs(5);

pub(super) async fn connect(owner: &str) -> (Database, JetStream) {
    fn port(name: &str) -> NonZeroU16 {
        std::env::var(name)
            .ok()
            .and_then(|value| {
                let port: NonZeroU16 = value.parse().ok()?;
                (port.to_string() == value).then_some(port)
            })
            .unwrap_or_else(|| panic!("{name} must name a disposable loopback port"))
    }

    let mongo = port("RECEIPT_TRANSPORT_TEST_MONGO_PORT");
    let nats = port("RECEIPT_TRANSPORT_TEST_NATS_PORT");
    let db = Client::with_uri_str(format!(
        "mongodb://127.0.0.1:{mongo}/?directConnection=true&serverSelectionTimeoutMS=3000"
    ))
    .await
    .expect("local receipt test Mongo settings")
    .database(&format!("{owner}_receipt_wire_{}", Uuid::new_v4().simple()));
    let broker = Connection::connect(
        &format!("nats://127.0.0.1:{nats}"),
        ConnectOptions::new()
            .connection_timeout(WAIT)
            .max_reconnects(1)
            .ignore_discovered_servers(),
    )
    .await
    .expect("disposable local receipt test NATS");
    (db, broker.jetstream(WAIT))
}
