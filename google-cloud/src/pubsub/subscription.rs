use std::collections::{HashMap, VecDeque};

use chrono::Duration;

use crate::pubsub::api;
use crate::pubsub::{Client, Error, Message};
use futures::channel::mpsc::SendError;
use futures::future::ready;
use futures::sink::{Sink, SinkExt};
use futures::stream::Stream;
use futures::stream::TryStreamExt;

/// Represents the subscription's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionConfig {
    pub(crate) ack_deadline_duration: Duration,
    pub(crate) message_retention_duration: Option<Duration>,
    pub(crate) labels: HashMap<String, String>,
}

impl SubscriptionConfig {
    /// Set the message acknowledgement duration.
    pub fn ack_deadline(mut self, duration: Duration) -> SubscriptionConfig {
        self.ack_deadline_duration = duration;
        self
    }

    /// Enable message retention and set its duration.
    pub fn retain_messages(mut self, duration: Duration) -> SubscriptionConfig {
        self.message_retention_duration = Some(duration);
        self
    }

    /// Attach a label to the subscription.
    pub fn label(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> SubscriptionConfig {
        self.labels.insert(name.into(), value.into());
        self
    }
}

impl Default for SubscriptionConfig {
    fn default() -> SubscriptionConfig {
        SubscriptionConfig {
            ack_deadline_duration: Duration::seconds(10),
            message_retention_duration: None,
            labels: HashMap::new(),
        }
    }
}

/// Optional parameters for pull.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiveOptions {
    /// return immediately if there are no messages in the subscription
    pub return_immediately: bool,
    /// Number of messages to retrieve at once
    pub max_messages: i32,
}

/// Options to send on a streaming pull.
#[derive(Debug, Clone, PartialEq)]
pub struct ReceiveStreamOptions {
    /// The IDs from previous pulls that should be acked.
    pub ack_ids: Vec<String>,
    /// A list of message IDs that should have their deadline modified.
    pub modify_deadline_ack_ids: Vec<String>,
    /// The new deadline (starting from now) for `modify_deadline_ack_ids`.
    pub modify_deadline_seconds: Vec<i32>,
    /// The ack deadline for the stream.
    pub stream_ack_deadline_seconds: i32,
}

impl Default for ReceiveOptions {
    fn default() -> Self {
        Self {
            return_immediately: false,
            max_messages: 1,
        }
    }
}

/// Represents a subscription, tied to a topic.
#[derive(Clone)]
pub struct Subscription {
    pub(crate) client: Client,
    pub(crate) name: String,
    pub(crate) buffer: VecDeque<api::ReceivedMessage>,
}

impl Subscription {
    pub(crate) fn new(client: Client, name: impl Into<String>) -> Subscription {
        Subscription {
            client,
            name: name.into(),
            buffer: VecDeque::new(),
        }
    }

    /// Returns the unique identifier within its project
    pub fn id(&self) -> &str {
        self.name.rsplit('/').next().unwrap()
    }

    /// Receive the next message from the subscription.
    pub async fn receive(&mut self) -> Option<Message> {
        self.receive_with_options(Default::default()).await
    }

    /// Receive the next message from the subscription with options.
    pub async fn receive_with_options(&mut self, opts: ReceiveOptions) -> Option<Message> {
        loop {
            if let Some(handle) = self.buffer.pop_front() {
                let message = handle.message.unwrap();
                let timestamp = message.publish_time.unwrap();
                let message = Message {
                    client: self.client.clone(),
                    subscription_name: self.name.clone(),
                    data: message.data,
                    message_id: message.message_id,
                    ack_id: handle.ack_id,
                    attributes: message.attributes,
                    publish_time: chrono::NaiveDateTime::from_timestamp(
                        timestamp.seconds,
                        timestamp.nanos as u32,
                    ),
                };
                break Some(message);
            } else if let Ok(messages) = self.pull(&opts).await {
                if messages.is_empty() && opts.return_immediately {
                    break None;
                }
                self.buffer.extend(messages);
            }
        }
    }

    /// Delete the subscription.
    pub async fn delete(mut self) -> Result<(), Error> {
        let request = api::DeleteSubscriptionRequest {
            subscription: self.name.clone(),
        };
        let request = self.client.construct_request(request).await?;
        self.client.subscriber.delete_subscription(request).await?;

        Ok(())
    }

    pub(crate) async fn pull(
        &mut self,
        opts: &ReceiveOptions,
    ) -> Result<Vec<api::ReceivedMessage>, Error> {
        let request = api::PullRequest {
            subscription: self.name.clone(),
            return_immediately: opts.return_immediately,
            max_messages: opts.max_messages,
        };
        let request = self.client.construct_request(request).await?;
        let response = self.client.subscriber.pull(request).await?;
        let response = response.into_inner();

        Ok(response.received_messages)
    }

    /// Create a stream of messages from the server.
    pub async fn pull_streaming(
        &mut self,
        opts: ReceiveStreamOptions,
    ) -> Result<
        (
            impl Stream<Item = Result<Vec<Message>, Error>>,
            impl Sink<ReceiveStreamOptions, Error = SendError> + Clone + Send + Sync + 'static,
        ),
        Error,
    > {
        let spr = api::StreamingPullRequest {
            subscription: self.name.clone(),
            ack_ids: opts.ack_ids,
            modify_deadline_seconds: opts.modify_deadline_seconds,
            modify_deadline_ack_ids: opts.modify_deadline_ack_ids,
            stream_ack_deadline_seconds: opts.stream_ack_deadline_seconds,
        };

        let (request, sender) = self.client.construct_streaming_request(spr).await?;

        let client = self.client.clone();
        let sub_name = self.name.clone();

        let sender = sender.with(move |opts: ReceiveStreamOptions| {
            ready(Ok::<_, SendError>(api::StreamingPullRequest {
                subscription: "".into(), // subscription can only be sent on the initial request.
                ack_ids: opts.ack_ids,
                modify_deadline_seconds: opts.modify_deadline_seconds,
                modify_deadline_ack_ids: opts.modify_deadline_ack_ids,
                stream_ack_deadline_seconds: opts.stream_ack_deadline_seconds,
            }))
        });

        let response = self.client.subscriber.streaming_pull(request).await?;
        let response = response.into_inner();

        let response = response
            .map_ok(move |v| {
                v.received_messages
                    .into_iter()
                    .map(|handle| {
                        let msg = handle.message.unwrap();
                        let timestamp = msg.publish_time.unwrap();
                        Message {
                            client: client.clone(),
                            subscription_name: sub_name.clone(),
                            data: msg.data,
                            message_id: msg.message_id,
                            ack_id: handle.ack_id,
                            attributes: msg.attributes,
                            publish_time: chrono::NaiveDateTime::from_timestamp(
                                timestamp.seconds,
                                timestamp.nanos as u32,
                            ),
                        }
                    })
                    .collect()
            })
            .map_err(Error::from);

        Ok((response, sender))
    }
}

// impl<'a> Stream for Subscription<'a> {
//     type Item = Message<'a>;
//     fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
//         let fut = match self.fut {
//             Some(fut) => fut.as_mut(),
//             None => {
//                 self.fut.replace(Box::pin(self.next_message()));
//                 self.fut.as_mut().unwrap().as_mut()
//             }
//         };

//         fut.poll(cx)
//     }
// }
