mod client;
mod subject;
mod wire;

pub use client::{Connection, JetStream};
pub use semver::{Version, VersionReq};
pub use subject::{Kind, Layer, Route, Scope, Subject};
pub use wire::{Actor, Contract, Delivery, Envelope, valid_traceparent};
// Native types needed for provisioning, pull-consumer lifecycle and explicit ACKs.
pub use async_nats::jetstream::{
    AckKind, ErrorCode, consumer,
    context::{AccountErrorKind, GetStreamErrorKind},
    stream,
};
pub use async_nats::{
    ConnectOptions, Event as ConnectionEvent, HeaderMap, Message, Subscriber, connection::State,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid erp4us subject")]
    InvalidSubject,
    #[error("missing, ambiguous or inconsistent header: {0}")]
    InvalidHeader(&'static str),
    #[error("invalid envelope: {0}")]
    InvalidEnvelope(&'static str),
    #[error("unsupported protocol version")]
    UnsupportedProtocol,
    #[error("unsupported schema version")]
    UnsupportedSchema,
    #[error("invalid contract version declaration")]
    InvalidContract,
    #[error("transport does not match contract")]
    WrongTransport,
    #[error("message serialization failed")]
    Serialize(#[source] serde_json::Error),
    #[error("message deserialization failed")]
    Deserialize(#[source] serde_json::Error),
    #[error("NATS connection failed")]
    Connection(#[from] async_nats::ConnectError),
    #[error("Core NATS publish failed")]
    CorePublish(#[from] async_nats::PublishError),
    #[error("Core NATS subscribe failed")]
    Subscribe(#[from] async_nats::SubscribeError),
    #[error("NATS request failed")]
    Request(#[from] async_nats::RequestError),
    #[error("NATS shutdown failed")]
    Drain(#[from] async_nats::client::DrainError),
    #[error("JetStream publish or acknowledgement failed")]
    JetStreamPublish(#[from] async_nats::jetstream::context::PublishError),
    #[error("JetStream acknowledged an unexpected stream")]
    UnexpectedStream,
}
