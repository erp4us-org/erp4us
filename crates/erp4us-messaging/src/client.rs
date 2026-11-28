use crate::{
    ConnectOptions, Contract, Delivery, Envelope, Error, Kind, Message, Scope, State, Subscriber,
};
use async_nats::jetstream::{self, context::PublishAckFuture, publish::PublishAck};
use std::{sync::atomic::Ordering, time::Duration};

#[derive(Clone)]
pub struct Connection(async_nats::Client);

impl Connection {
    pub async fn connect(url: &str, options: ConnectOptions) -> Result<Self, Error> {
        Ok(Self(options.connect(url).await?))
    }

    pub fn state(&self) -> State {
        self.0.connection_state()
    }

    /// Count successful connections, including reconnects, for connection-local observations.
    pub fn successful_connections(&self) -> u64 {
        self.0.statistics().connects.load(Ordering::Relaxed)
    }

    pub async fn drain(self) -> Result<(), Error> {
        Ok(self.0.drain().await?)
    }

    pub fn jetstream(&self, timeout: Duration) -> JetStream {
        let mut context = jetstream::new(self.0.clone());
        context.set_timeout(timeout);
        JetStream(context)
    }

    pub async fn publish<T: Contract>(&self, envelope: &Envelope<T>) -> Result<(), Error> {
        if T::DELIVERY != Delivery::Core || T::ROUTE.kind == Kind::Query {
            return Err(Error::WrongTransport);
        }
        let payload = encode(envelope)?;
        self.0
            .publish_with_headers(envelope.subject()?, envelope.headers(), payload.into())
            .await?;
        Ok(())
    }

    pub async fn subscribe<T: Contract>(
        &self,
        protocol: u64,
        scope: Scope,
        schema: u64,
    ) -> Result<Subscriber, Error> {
        if T::DELIVERY != Delivery::Core {
            return Err(Error::WrongTransport);
        }
        Ok(self
            .0
            .subscribe(T::ROUTE.subject(protocol, scope, schema)?)
            .await?)
    }

    /// Returns the native inbox reply. The query contract defines its reply body and validation.
    pub async fn request<T: Contract>(
        &self,
        envelope: &Envelope<T>,
        timeout: Duration,
    ) -> Result<Message, Error> {
        self.request_inner(envelope, timeout, false).await
    }

    /// Use a dedicated reply subscription that is dropped on timeout or cancellation.
    /// Suitable for periodic probes that must not retain unanswered requests in the multiplexer.
    pub async fn request_with_temporary_inbox<T: Contract>(
        &self,
        envelope: &Envelope<T>,
        timeout: Duration,
    ) -> Result<Message, Error> {
        self.request_inner(envelope, timeout, true).await
    }

    async fn request_inner<T: Contract>(
        &self,
        envelope: &Envelope<T>,
        timeout: Duration,
        temporary_inbox: bool,
    ) -> Result<Message, Error> {
        if T::DELIVERY != Delivery::Core || T::ROUTE.kind != Kind::Query {
            return Err(Error::WrongTransport);
        }
        let payload = encode(envelope)?;
        let request = async_nats::Request::new()
            .headers(envelope.headers())
            .payload(payload.into())
            .timeout(Some(timeout));
        let request = if temporary_inbox {
            request.inbox(self.0.new_inbox())
        } else {
            request
        };
        Ok(self.0.send_request(envelope.subject()?, request).await?)
    }

    pub async fn reply<T: SerializeReply>(
        &self,
        request: &Message,
        reply: &T,
    ) -> Result<(), Error> {
        let inbox = request.reply.as_ref().ok_or(Error::InvalidSubject)?;
        if !inbox.as_str().starts_with("_INBOX.")
            || inbox.as_str().split('.').skip(1).any(|token| {
                token.is_empty()
                    || !token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            })
        {
            return Err(Error::InvalidSubject);
        }
        let payload = serde_json::to_vec(reply).map_err(Error::Serialize)?;
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Content-Type", "application/json");
        self.0
            .publish_with_headers(inbox.clone(), headers, payload.into())
            .await?;
        Ok(())
    }
}

use serde::Serialize as SerializeReply;

#[derive(Clone)]
pub struct JetStream(jetstream::Context);

impl JetStream {
    /// Native JetStream administration/pull-consumer lifecycle. Access still requires broker ACLs.
    pub fn operations(&self) -> &jetstream::Context {
        &self.0
    }

    pub async fn publish<T: Contract>(&self, envelope: &Envelope<T>) -> Result<PublishAck, Error> {
        let payload = encode(envelope)?;
        self.send(envelope, envelope.message_id.to_string(), payload)
            .await
    }

    /// Republish only after the caller checked authorization and its durable replay record.
    pub async fn replay<T: Contract>(
        &self,
        original: &jetstream::message::StreamMessage,
    ) -> Result<PublishAck, Error> {
        if original.sequence == 0 {
            return Err(Error::InvalidEnvelope("replay sequence"));
        }
        let envelope = Envelope::<T>::decode(
            original.subject.as_str(),
            &original.headers,
            &original.payload,
        )?;
        self.send(
            &envelope,
            format!("replay-{}-{}", original.sequence, envelope.message_id),
            original.payload.to_vec(),
        )
        .await
    }

    async fn send<T: Contract>(
        &self,
        envelope: &Envelope<T>,
        id: String,
        body: Vec<u8>,
    ) -> Result<PublishAck, Error> {
        let Delivery::JetStream(stream) = T::DELIVERY else {
            return Err(Error::WrongTransport);
        };
        let mut headers = envelope.headers();
        headers.insert("Nats-Msg-Id", id);
        headers.insert("Nats-Expected-Stream", stream);
        let pending: PublishAckFuture = self
            .0
            .publish_with_headers(envelope.subject()?, headers, body.into())
            .await?;
        let ack = pending.await?;
        if ack.stream != stream {
            return Err(Error::UnexpectedStream);
        }
        Ok(ack)
    }
}

fn encode<T: Contract>(envelope: &Envelope<T>) -> Result<Vec<u8>, Error> {
    envelope.validate()?;
    if envelope.schema_version.to_string() != T::SCHEMA_VERSION {
        return Err(Error::UnsupportedSchema);
    }
    serde_json::to_vec(envelope).map_err(Error::Serialize)
}
