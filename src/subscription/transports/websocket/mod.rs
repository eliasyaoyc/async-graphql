use crate::{http, Data, FieldResult, ObjectType, Response, Schema, SubscriptionType};
use futures::channel::mpsc;
use futures::task::{AtomicWaker, Context, Poll};
use futures::{Future, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
struct OperationMessage {
    #[serde(rename = "type")]
    ty: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<serde_json::Value>,
}

type SubscriptionStreams = HashMap<String, Pin<Box<dyn Stream<Item = Response> + Send>>>;

type HandleRequestBoxFut<'a> =
    Pin<Box<dyn Future<Output = FieldResult<WSContext<'a>>> + Send + 'a>>;

type InitializerFn = Arc<dyn Fn(serde_json::Value) -> FieldResult<Data> + Send + Sync>;

pub fn create<Query, Mutation, Subscription>(
    schema: &Schema<Query, Mutation, Subscription>,
) -> (mpsc::UnboundedSender<Vec<u8>>, impl Stream<Item = Vec<u8>>)
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    create_with_initializer(schema, |_| Ok(Default::default()))
}

pub fn create_with_initializer<Query, Mutation, Subscription>(
    schema: &Schema<Query, Mutation, Subscription>,
    initializer: impl Fn(serde_json::Value) -> FieldResult<Data> + Send + Sync + 'static,
) -> (mpsc::UnboundedSender<Vec<u8>>, impl Stream<Item = Vec<u8>>)
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    let schema = schema.clone();
    let (tx_bytes, rx_bytes) = mpsc::unbounded();
    let stream = async_stream::stream! {
        let mut streams = Default::default();
        let mut send_buf = Default::default();
        let mut data = Arc::new(Data::default());
        let mut inner_stream = SubscriptionStream {
            schema: &schema,
            initializer: Arc::new(initializer),
            rx_bytes,
            handle_request_fut: None,
            waker: AtomicWaker::new(),
            ctx: Some(WSContext {
                streams: &mut streams,
                send_buf: &mut send_buf,
                ctx_data: &mut data,
            }),
        };
        while let Some(data) = inner_stream.next().await {
            yield data;
        }
    };
    (tx_bytes, stream)
}

struct WSContext<'a> {
    streams: &'a mut SubscriptionStreams,
    send_buf: &'a mut VecDeque<Vec<u8>>,
    ctx_data: &'a mut Arc<Data>,
}

fn send_message<T: Serialize>(send_buf: &mut VecDeque<Vec<u8>>, msg: &T) {
    if let Ok(data) = serde_json::to_vec(msg) {
        send_buf.push_back(data);
    }
}

#[allow(missing_docs)]
#[allow(clippy::type_complexity)]
struct SubscriptionStream<'a, Query, Mutation, Subscription> {
    schema: &'a Schema<Query, Mutation, Subscription>,
    initializer: InitializerFn,
    rx_bytes: mpsc::UnboundedReceiver<Vec<u8>>,
    handle_request_fut: Option<HandleRequestBoxFut<'a>>,
    waker: AtomicWaker,
    ctx: Option<WSContext<'a>>,
}

impl<'a, Query, Mutation, Subscription> Stream
    for SubscriptionStream<'a, Query, Mutation, Subscription>
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        loop {
            // receive bytes
            if let Some(ctx) = &mut this.ctx {
                if let Some(bytes) = ctx.send_buf.pop_front() {
                    return Poll::Ready(Some(bytes));
                }
            }

            if let Some(handle_request_fut) = &mut this.handle_request_fut {
                match handle_request_fut.as_mut().poll(cx) {
                    Poll::Ready(Ok(ctx)) => {
                        this.ctx = Some(ctx);
                        this.handle_request_fut = None;
                        continue;
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(None),
                    Poll::Pending => {}
                }
            } else {
                match Pin::new(&mut this.rx_bytes).poll_next(cx) {
                    Poll::Ready(Some(data)) => {
                        let ctx = this.ctx.take().unwrap();
                        this.handle_request_fut = Some(Box::pin(handle_request(
                            this.schema.clone(),
                            this.initializer.clone(),
                            ctx,
                            data,
                        )));
                        this.waker.wake();
                        continue;
                    }
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => {}
                }
            }

            // receive msg
            if let Some(ctx) = &mut this.ctx {
                if !ctx.streams.is_empty() {
                    let mut closed = Vec::new();

                    for (id, incoming_stream) in ctx.streams.iter_mut() {
                        loop {
                            match incoming_stream.as_mut().poll_next(cx) {
                                Poll::Ready(Some(res)) => {
                                    if let Some(err) = &res.error {
                                        closed.push(id.to_string());
                                        send_message(
                                            ctx.send_buf,
                                            &OperationMessage {
                                                ty: "error".to_string(),
                                                id: Some(id.to_string()),
                                                payload: Some(serde_json::to_value(err).unwrap()),
                                            },
                                        );
                                    } else {
                                        send_message(
                                            ctx.send_buf,
                                            &OperationMessage {
                                                ty: "data".to_string(),
                                                id: Some(id.to_string()),
                                                payload: Some(serde_json::to_value(&res).unwrap()),
                                            },
                                        );
                                    }
                                }
                                Poll::Ready(None) => {
                                    closed.push(id.to_string());
                                    send_message(
                                        ctx.send_buf,
                                        &OperationMessage {
                                            ty: "complete".to_string(),
                                            id: Some(id.to_string()),
                                            payload: None,
                                        },
                                    );
                                }
                                Poll::Pending => break,
                            }
                        }
                    }

                    for id in closed {
                        ctx.streams.remove(&id);
                    }
                    this.waker.register(cx.waker());
                    return Poll::Pending;
                } else {
                    this.waker.register(cx.waker());
                    return Poll::Pending;
                }
            } else {
                return Poll::Pending;
            }
        }
    }
}

async fn handle_request<'a, Query, Mutation, Subscription>(
    schema: Schema<Query, Mutation, Subscription>,
    initializer: InitializerFn,
    ctx: WSContext<'a>,
    data: Vec<u8>,
) -> FieldResult<WSContext<'a>>
where
    Query: ObjectType + Send + Sync + 'static,
    Mutation: ObjectType + Send + Sync + 'static,
    Subscription: SubscriptionType + Send + Sync + 'static,
{
    match serde_json::from_slice::<OperationMessage>(&data) {
        Ok(msg) => match msg.ty.as_str() {
            "connection_init" => {
                if let Some(payload) = msg.payload {
                    *ctx.ctx_data = Arc::new(initializer(payload)?);
                }
                send_message(
                    ctx.send_buf,
                    &OperationMessage {
                        ty: "connection_ack".to_string(),
                        id: None,
                        payload: None,
                    },
                );
            }
            "start" => {
                if let (Some(id), Some(payload)) = (msg.id, msg.payload) {
                    if let Ok(request) = serde_json::from_value::<http::GQLRequest>(payload) {
                        let stream = schema.execute_stream(request).boxed();
                        ctx.streams.insert(id, stream);
                    }
                }
            }
            "stop" => {
                if let Some(id) = msg.id {
                    if ctx.streams.remove(&id).is_some() {
                        send_message(
                            ctx.send_buf,
                            &OperationMessage {
                                ty: "complete".to_string(),
                                id: Some(id),
                                payload: None,
                            },
                        );
                    }
                }
            }
            "connection_terminate" => return Err("connection_terminate".into()),
            _ => return Err("Unknown op".into()),
        },
        Err(err) => return Err(err.into()),
    }

    Ok(ctx)
}
