//! Ephemeral presence of the preview's single Article Relay, not runtime health.
use crate::PROBE_TIMEOUT;
use erp4us_messaging::{
    ConnectOptions, Connection, ConnectionEvent, Contract, Delivery, Envelope, Error, Kind, Layer,
    Message, Route, Scope, State as BrokerState, Version,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::{sync::watch, time::timeout};
use uuid::Uuid;

const SOURCE: &str = "erp4us.article-delivery";
const POLL: Duration = Duration::from_secs(5);
const MAX_PAYLOAD_BYTES: usize = 8192;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Heartbeat {
    instance_id: String,
    uptime_seconds: u64,
    nats: String,
}

impl Contract for Heartbeat {
    const ROUTE: Route<'static> = Route {
        layer: Layer::Diagnostics,
        domain: "article-delivery",
        kind: Kind::Event,
        resource: "service",
        action: "heartbeat",
    };
    const SCHEMA_VERSION: &'static str = "1.0.0";
    const PROTOCOL_SUPPORT: &'static str = "=1.0.0";
    const SCHEMA_SUPPORT: &'static str = "=1.0.0";
    const DELIVERY: Delivery = Delivery::Core;

    fn validate(&self) -> Result<(), Error> {
        if self.nats != "connected"
            || !self
                .instance_id
                .parse::<Uuid>()
                .is_ok_and(|id| !id.is_nil() && id.hyphenated().to_string() == self.instance_id)
        {
            return Err(Error::InvalidEnvelope("relay heartbeat payload"));
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
struct GetSettings {}

impl Contract for GetSettings {
    const ROUTE: Route<'static> = Route {
        layer: Layer::Config,
        domain: "article-delivery",
        kind: Kind::Query,
        resource: "heartbeat",
        action: "get",
    };
    const SCHEMA_VERSION: &'static str = "1.0.0";
    const PROTOCOL_SUPPORT: &'static str = "=1.0.0";
    const SCHEMA_SUPPORT: &'static str = "=1.0.0";
    const DELIVERY: Delivery = Delivery::Core;
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct Settings {
    revision: u64,
    heartbeat_seconds: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsReply {
    result: Settings,
}

fn decode_settings(message: &Message) -> Option<Settings> {
    let mut media_types = message
        .headers
        .as_ref()?
        .iter()
        .filter(|(name, _)| name.to_string().eq_ignore_ascii_case("Content-Type"))
        .flat_map(|(_, values)| values);
    if message.status.is_some()
        || message.payload.len() > MAX_PAYLOAD_BYTES
        || media_types.next()?.as_str() != "application/json"
        || media_types.next().is_some()
    {
        return None;
    }
    let reply: SettingsReply = serde_json::from_slice(&message.payload).ok()?;
    (reply.result.heartbeat_seconds <= 3600).then_some(reply.result)
}

async fn query_settings(broker: &Connection) -> Option<Settings> {
    let query = Envelope::system(
        GetSettings {},
        Version::new(1, 0, 0),
        "erp4us.platform-diagnostics",
        Version::parse(env!("CARGO_PKG_VERSION")).ok()?,
    )
    .ok()?;
    let response = timeout(
        PROBE_TIMEOUT,
        broker.request_with_temporary_inbox(&query, PROBE_TIMEOUT),
    )
    .await
    .ok()?
    .ok()?;
    decode_settings(&response)
}

#[derive(Clone, Copy)]
struct Receipt {
    received: Instant,
    instance: Uuid,
    message_id: Uuid,
    uptime_seconds: u64,
}

impl Receipt {
    fn decode(message: &Message, received: Instant) -> Option<Self> {
        if message.status.is_some()
            || message.reply.is_some()
            || message.payload.len() > MAX_PAYLOAD_BYTES
        {
            return None;
        }
        let envelope = Envelope::<Heartbeat>::decode(
            message.subject.as_str(),
            message.headers.as_ref()?,
            &message.payload,
        )
        .ok()?;
        if envelope.source_service != SOURCE || envelope.tenant_id.is_some() {
            return None;
        }
        Some(Self {
            received,
            instance: envelope.payload.instance_id.parse().ok()?,
            message_id: envelope.message_id,
            uptime_seconds: envelope.payload.uptime_seconds,
        })
    }
}

#[derive(Default)]
struct Window {
    connection_number: u64,
    settings: Option<Settings>,
    settings_seen: Option<Instant>,
    started: Option<Instant>,
    last: Option<Receipt>,
}

impl Window {
    fn configure(&mut self, settings: Option<Settings>, now: Instant) {
        if settings != self.settings {
            // An observed settings change starts a new grace window. Do not revive a receipt
            // from a previous revision (a change could have disabled and reenabled publishing).
            if self.settings.is_some() {
                self.last = None;
            }
            self.started = settings.map(|_| now);
        }
        self.settings_seen = settings.map(|_| now);
        self.settings = settings;
    }

    fn receive(&mut self, receipt: Receipt) {
        if self.settings.is_some_and(|s| s.heartbeat_seconds == 0)
            || self.last.is_some_and(|last| {
                receipt.message_id == last.message_id
                    || (receipt.instance == last.instance
                        && receipt.uptime_seconds < last.uptime_seconds)
            })
        {
            return;
        }
        self.last = Some(receipt);
    }

    fn report(&self, now: Instant) -> Report {
        let (Some(settings), Some(seen), Some(started)) =
            (self.settings, self.settings_seen, self.started)
        else {
            return Report::default();
        };
        if now.saturating_duration_since(seen) >= POLL + PROBE_TIMEOUT {
            return Report::default();
        }
        let interval = settings.heartbeat_seconds;
        if interval == 0 {
            return Report {
                state: Status::Disabled,
                interval_seconds: Some(0),
                ..Report::default()
            };
        }
        let age = self
            .last
            .map(|receipt| now.saturating_duration_since(receipt.received));
        let elapsed = age.unwrap_or_else(|| now.saturating_duration_since(started));
        let state = if elapsed >= Duration::from_secs(u64::from(interval) * 3) {
            Status::Stale
        } else if age.is_some() {
            Status::Fresh
        } else {
            Status::Waiting
        };
        Report {
            state,
            interval_seconds: Some(interval),
            instance_id: self.last.map(|receipt| receipt.instance),
            age_seconds: age.map(|age| age.as_secs()),
        }
    }
}

#[derive(Default, Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Waiting,
    Fresh,
    Stale,
    Disabled,
    #[default]
    Unavailable,
}

#[derive(Default, Serialize)]
pub(super) struct Report {
    state: Status,
    interval_seconds: Option<u16>,
    instance_id: Option<Uuid>,
    age_seconds: Option<u64>,
}

#[derive(Default)]
struct Transport {
    generation: u64,
    connected: bool,
    window: Option<Window>,
}

impl Transport {
    fn new_generation(&mut self, connected: bool) {
        self.generation = self.generation.wrapping_add(1);
        self.connected = connected;
        self.window = None;
    }

    fn report(&self, connection_number: u64, now: Instant) -> Report {
        self.window
            .as_ref()
            // The underlying connection may have changed before the callback runs.
            .filter(|window| window.connection_number == connection_number)
            .map_or_else(Report::default, |window| window.report(now))
    }
}

#[derive(Clone)]
pub(super) struct Observer(watch::Receiver<Transport>);

impl Observer {
    pub(super) async fn connect(
        url: &str,
        options: ConnectOptions,
    ) -> Result<(Connection, Self), Error> {
        let (tx, rx) = watch::channel(Transport::default());
        let events = tx.clone();
        let subject = Heartbeat::ROUTE.subject(1, Scope::System, 1)?;
        let denied = format!("Permissions Violation for Subscription to \"{subject}\"");
        let broker = Connection::connect(
            url,
            options
                .subscription_capacity(16)
                .event_callback(move |event| {
                    let events = events.clone();
                    let denied = denied.clone();
                    async move {
                        match event {
                            ConnectionEvent::Connected => {
                                events.send_modify(|state| state.new_generation(true));
                            }
                            ConnectionEvent::Disconnected
                            | ConnectionEvent::Closed
                            | ConnectionEvent::Draining => {
                                events.send_modify(|state| state.new_generation(false));
                            }
                            ConnectionEvent::ServerError(error) if error.to_string() == denied => {
                                events.send_modify(|state| state.new_generation(false));
                            }
                            _ => {}
                        }
                    }
                }),
        )
        .await?;
        tokio::spawn(observe(broker.clone(), tx, rx.clone()));
        Ok((broker, Self(rx)))
    }

    pub(super) fn report(&self, broker: &Connection) -> Report {
        if broker.state() != BrokerState::Connected {
            return Report::default();
        }
        self.0
            .borrow()
            .report(broker.successful_connections(), Instant::now())
    }
}

async fn observe(
    broker: Connection,
    tx: watch::Sender<Transport>,
    mut rx: watch::Receiver<Transport>,
) {
    while let Ok(generation) = rx
        .wait_for(|state| state.connected)
        .await
        .map(|s| s.generation)
    {
        let ended = tokio::select! {
            biased;
            _ = rx.wait_for(|state| state.generation != generation) => false,
            _ = observe_generation(&broker, &tx, generation) => true,
        };
        if ended {
            tx.send_if_modified(|state| {
                state.generation == generation && state.window.take().is_some()
            });
            // Retry a failed or ended subscription without busy-looping.
            let _ = timeout(POLL, rx.wait_for(|state| state.generation != generation)).await;
        }
    }
}

async fn observe_generation(broker: &Connection, tx: &watch::Sender<Transport>, generation: u64) {
    let connection_number = broker.successful_connections();
    let Ok(Ok(mut messages)) = timeout(
        PROBE_TIMEOUT,
        broker.subscribe::<Heartbeat>(1, Scope::System, 1),
    )
    .await
    else {
        return;
    };
    if !tx.send_if_modified(|state| {
        if state.generation != generation {
            return false;
        }
        state.window = Some(Window {
            connection_number,
            ..Window::default()
        });
        true
    }) {
        return;
    }
    let settings = async {
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if broker.successful_connections() != connection_number {
                return;
            }
            let settings = query_settings(broker).await;
            update(tx, generation, |window| {
                window.configure(settings, Instant::now());
            });
        }
    };
    let receipts = async {
        while let Some(message) = messages.next().await {
            if let Some(receipt) = Receipt::decode(&message, Instant::now()) {
                update(tx, generation, |window| window.receive(receipt));
            }
        }
    };
    // Settings timeouts never delay draining the bounded subscription, and ending the window
    // drops the subscriber so buffered receipts cannot cross a reconnect.
    tokio::select! {
        _ = settings => {}
        _ = receipts => {}
    }
}

fn update(tx: &watch::Sender<Transport>, generation: u64, apply: impl FnOnce(&mut Window)) {
    tx.send_if_modified(|state| {
        if state.generation == generation
            && let Some(window) = state.window.as_mut()
        {
            apply(window);
            return true;
        }
        false
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use erp4us_messaging::HeaderMap;
    use serde_json::{Value, json};

    const HEARTBEAT: &str = "v1.diagnostics.system.article-delivery.event.service.heartbeat.v1";
    const INSTANCE: &str = "11111111-1111-4111-8111-111111111111";

    fn settings(revision: u64, heartbeat_seconds: u16) -> Option<Settings> {
        Some(Settings {
            revision,
            heartbeat_seconds,
        })
    }

    fn message(subject: &str, headers: HeaderMap, body: Value) -> Message {
        let body = serde_json::to_vec(&body).unwrap();
        Message {
            subject: subject.to_owned().into(),
            reply: None,
            headers: Some(headers),
            length: body.len(),
            payload: body.into(),
            status: None,
            description: None,
        }
    }

    fn wire(source: &str, instance: &str, nats: &str) -> Message {
        let root_id = "22222222-2222-4222-8222-222222222222";
        let mut headers = HeaderMap::new();
        for (key, value) in [
            ("Content-Type", "application/json"),
            ("Erp4us-Protocol-Version", "1.0.0"),
            ("Erp4us-Schema-Version", "1.0.0"),
            ("Erp4us-Service-Version", "0.1.0"),
            ("Erp4us-Message-Id", root_id),
            ("Erp4us-Correlation-Id", root_id),
            ("Erp4us-Source", source),
        ] {
            headers.insert(key, value.to_owned());
        }
        message(
            HEARTBEAT,
            headers,
            json!({
                "messageId": root_id, "messageType": HEARTBEAT,
                "protocolVersion": "1.0.0", "schemaVersion": "1.0.0", "serviceVersion": "0.1.0",
                "timestamp": "2000-01-01T00:00:00Z", "sourceService": source, "correlationId": root_id,
                "payload": {"instanceId": instance, "uptimeSeconds": 42, "nats": nats}
            }),
        )
    }

    #[test]
    fn staleness_uses_local_receipt_and_gives_an_initial_grace() {
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let mut window = Window::default();
        window.configure(settings(0, 2), start);
        assert_eq!(window.report(at(5)).state, Status::Waiting);
        let stale = window.report(at(6));
        assert_eq!(stale.state, Status::Stale);
        assert_eq!(stale.age_seconds, None);
        assert_eq!(stale.instance_id, None);

        // The producer timestamp in this fixture is decades old. It does not affect presence.
        let receipt = Receipt::decode(&wire(SOURCE, INSTANCE, "connected"), at(6)).unwrap();
        window.receive(receipt);
        window.configure(settings(0, 2), at(10));
        let fresh = window.report(at(11));
        assert_eq!(fresh.state, Status::Fresh);
        assert_eq!(fresh.age_seconds, Some(5));
        window.receive(Receipt {
            received: at(11),
            ..receipt
        });
        let stale = window.report(at(12));
        assert_eq!(stale.state, Status::Stale);
        assert_eq!(stale.age_seconds, Some(6)); // Duplicate did not reset receipt age.
    }

    #[test]
    fn disabled_and_unknown_settings_do_not_claim_presence() {
        let now = Instant::now();
        let receipt = Receipt::decode(&wire(SOURCE, INSTANCE, "connected"), now).unwrap();
        let mut window = Window::default();
        window.configure(settings(0, 1), now);
        window.receive(receipt);
        assert_eq!(window.report(now).state, Status::Fresh);
        window.configure(settings(1, 0), now);
        window.receive(receipt);
        assert_eq!(
            serde_json::to_value(window.report(now)).unwrap(),
            json!({"state": "disabled", "interval_seconds": 0, "instance_id": null, "age_seconds": null})
        );
        window.configure(settings(2, 1), now);
        assert_eq!(window.report(now).state, Status::Waiting);
        window.receive(receipt);
        assert_eq!(
            window.report(now + POLL + PROBE_TIMEOUT).state,
            Status::Unavailable
        );
        window.configure(None, now);
        let unavailable = window.report(now);
        assert_eq!(unavailable.state, Status::Unavailable);
        assert_eq!(unavailable.interval_seconds, None);
        assert_eq!(unavailable.age_seconds, None);
    }

    #[test]
    fn reconnect_clears_receipt_and_requires_new_presence() {
        let now = Instant::now();
        let receipt = Receipt::decode(&wire(SOURCE, INSTANCE, "connected"), now).unwrap();
        let mut transport = Transport::default();
        transport.new_generation(true);
        let mut window = Window {
            connection_number: 1,
            ..Window::default()
        };
        window.configure(settings(0, 1), now);
        window.receive(receipt);
        transport.window = Some(window);
        assert_eq!(transport.report(1, now).state, Status::Fresh);
        assert_eq!(transport.report(2, now).state, Status::Unavailable);
        let original = transport.generation;
        transport.new_generation(false);
        assert_eq!(transport.report(1, now).state, Status::Unavailable);
        transport.new_generation(true);
        assert_ne!(transport.generation, original);
        let next = now + Duration::from_secs(1);
        let mut resumed = Window {
            connection_number: 2,
            ..Window::default()
        };
        resumed.configure(settings(0, 1), next);
        transport.window = Some(resumed);
        assert_eq!(transport.report(2, next).state, Status::Waiting);
        let next_receipt = Receipt {
            received: next,
            message_id: Uuid::new_v4(),
            ..receipt
        };
        transport.window.as_mut().unwrap().receive(next_receipt);
        assert_eq!(
            serde_json::to_value(transport.report(2, next)).unwrap(),
            json!({"state": "fresh", "interval_seconds": 1, "instance_id": INSTANCE, "age_seconds": 0})
        );
    }

    #[test]
    fn requires_exact_wire_subject_versions_source_and_payload() {
        let now = Instant::now();
        assert!(Receipt::decode(&wire(SOURCE, INSTANCE, "connected"), now).is_some());
        assert!(
            Receipt::decode(&wire("erp4us.not-the-relay", INSTANCE, "connected"), now).is_none()
        );
        assert!(
            Receipt::decode(
                &wire(SOURCE, Uuid::nil().to_string().as_str(), "connected"),
                now
            )
            .is_none()
        );
        assert!(Receipt::decode(&wire(SOURCE, INSTANCE, "degraded"), now).is_none());

        let mut foreign = wire(SOURCE, INSTANCE, "connected");
        foreign.subject = "v1.diagnostics.11111111-1111-4111-8111-111111111111.article-delivery.event.service.heartbeat.v1".into();
        assert!(Receipt::decode(&foreign, now).is_none());
        let mut mismatch = wire(SOURCE, INSTANCE, "connected");
        mismatch
            .headers
            .as_mut()
            .unwrap()
            .insert("Erp4us-Protocol-Version", "1.0.1");
        assert!(Receipt::decode(&mismatch, now).is_none());
        let mut duplicate = wire(SOURCE, INSTANCE, "connected");
        duplicate
            .headers
            .as_mut()
            .unwrap()
            .append("Erp4us-Source", SOURCE);
        assert!(Receipt::decode(&duplicate, now).is_none());
        let mut unsupported = wire(SOURCE, INSTANCE, "connected");
        let mut body: Value = serde_json::from_slice(&unsupported.payload).unwrap();
        body["schemaVersion"] = json!("1.0.1");
        unsupported.payload = serde_json::to_vec(&body).unwrap().into();
        unsupported
            .headers
            .as_mut()
            .unwrap()
            .insert("Erp4us-Schema-Version", "1.0.1");
        assert!(Receipt::decode(&unsupported, now).is_none());
    }

    #[test]
    fn only_accepts_a_current_interval_from_the_settings_reply_contract() {
        assert_eq!(
            GetSettings::ROUTE.subject(1, Scope::System, 1).unwrap(),
            "v1.config.system.article-delivery.query.heartbeat.get.v1"
        );
        let reply = |body| {
            let mut headers = HeaderMap::new();
            headers.insert("Content-Type", "application/json");
            message("_INBOX.platform-diagnostics.fixture", headers, body)
        };
        let valid = reply(json!({"result": {"revision": 1, "heartbeatSeconds": 0}}));
        assert_eq!(decode_settings(&valid).unwrap().heartbeat_seconds, 0);
        for body in [
            json!({"error": {"code": "unavailable"}}),
            json!({"result": {"revision": 1, "heartbeatSeconds": 0}, "error": {}}),
            json!({"result": {"revision": -1, "heartbeatSeconds": 1}}),
            json!({"result": {"revision": 1, "heartbeatSeconds": 3601}}),
        ] {
            assert!(decode_settings(&reply(body)).is_none());
        }
    }
}
