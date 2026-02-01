use kameo::{
    actor::{Actor, ActorRef, WeakActorRef, ActorId},
    error::{ActorStopReason, Infallible},
    message::{Context as KameoContext, Message, StreamMessage},
};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{SinkExt, StreamExt, stream::SplitSink};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::ops::ControlFlow;
use std::future::Future;

use crate::actor::document::DocActor;
use crate::actor::root::Root;
use crate::actor::messages::{RequestDoc, ConnectClient, DisconnectClient, YjsData, WirePayload, PersistNow};
use crate::payload::{decode_doc_id, encode_with_doc_id};
use crate::hooks::{Hook, Context, RequestInfo, OnConnectPayload, OnAuthenticatePayload};

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ControlMessage {
    Leave { doc: String },
    Save { doc: String },
}

pub struct ClientActorArgs {
    pub socket: WebSocket,
    pub request_info: RequestInfo,
    pub root: ActorRef<Root>,
    pub hooks: Arc<Vec<Box<dyn Hook>>>,
}

pub struct ClientActor {
    sink: SplitSink<WebSocket, WsMessage>,
    docs: HashMap<Arc<str>, ActorRef<DocActor>>,
    root: ActorRef<Root>,
    request_info: RequestInfo,
    hooks: Arc<Vec<Box<dyn Hook>>>,
}

impl Actor for ClientActor {
    type Args = ClientActorArgs;
    type Error = Infallible;
    
    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let (sink, stream) = args.socket.split();
        actor_ref.attach_stream(stream, (), "ws");
        Ok(Self { sink, docs: HashMap::new(), root: args.root, request_info: args.request_info, hooks: args.hooks })
    }

    fn on_link_died(&mut self, _: WeakActorRef<Self>, id: ActorId, _: ActorStopReason) -> impl Future<Output = Result<ControlFlow<ActorStopReason>, Self::Error>> + Send {
        self.docs.retain(|_, doc| doc.id() != id);
        async { Ok(ControlFlow::Continue(())) }
    }
}

impl Message<StreamMessage<Result<WsMessage, axum::Error>, (), &'static str>> for ClientActor {
    type Reply = ();
    
    async fn handle(&mut self, msg: StreamMessage<Result<WsMessage, axum::Error>, (), &'static str>, ctx: &mut KameoContext<Self, Self::Reply>) {
        match msg {
            StreamMessage::Next(Ok(WsMessage::Binary(data))) => {
                let Some((doc_id, _)) = decode_doc_id(&data) else { return };
                let header_len = 1 + doc_id.len();
                let payload = data.slice(header_len..);
                let doc_id: Arc<str> = doc_id.into();

                let doc = match self.docs.get(&doc_id) {
                    Some(d) => d.clone(),
                    None => match self.connect_doc(Arc::clone(&doc_id), ctx.actor_ref()).await {
                        Some(d) => d,
                        None => return,
                    }
                };
                let _ = doc.tell(YjsData { client_id: ctx.actor_ref().id(), data: payload }).send().await;
            }
            StreamMessage::Next(Ok(WsMessage::Text(text))) => {
                if let Ok(ctrl) = serde_json::from_str::<ControlMessage>(&text) {
                    match ctrl {
                        ControlMessage::Leave { doc } => {
                            let doc_id: Arc<str> = doc.into();
                            if let Some(doc_actor) = self.docs.remove(&doc_id) {
                                let _ = doc_actor.tell(DisconnectClient(ctx.actor_ref().clone())).send().await;
                            }
                        }
                        ControlMessage::Save { doc } => {
                            let doc_id: Arc<str> = doc.into();
                            if let Some(doc_actor) = self.docs.get(&doc_id) {
                                let _ = doc_actor.ask(PersistNow).send().await;
                            }
                        }
                    }
                }
            }
            StreamMessage::Next(Ok(WsMessage::Ping(data))) => {
                if self.sink.send(WsMessage::Pong(data)).await.is_err() { ctx.actor_ref().kill(); }
            }
            StreamMessage::Next(Ok(WsMessage::Close(_))) | StreamMessage::Finished(_) => {
                for (_, doc) in self.docs.drain() { let _ = doc.tell(DisconnectClient(ctx.actor_ref().clone())).send().await; }
                ctx.actor_ref().kill();
            }
            StreamMessage::Next(Err(_)) => ctx.actor_ref().kill(),
            _ => {}
        }
    }
}

impl Message<WirePayload> for ClientActor {
    type Reply = ();
    async fn handle(&mut self, msg: WirePayload, ctx: &mut KameoContext<Self, Self::Reply>) {
        if self.sink.send(WsMessage::Binary(msg.0.to_vec().into())).await.is_err() { ctx.actor_ref().kill(); }
    }
}

impl ClientActor {
    async fn connect_doc(&mut self, doc_id: Arc<str>, me: &ActorRef<Self>) -> Option<ActorRef<DocActor>> {
        let client_id = me.id();
        
        for hook in self.hooks.iter() {
            if hook.on_connect(OnConnectPayload { doc_id: &doc_id, client_id, request: &self.request_info }).await.is_err() {
                return None;
            }
        }
        
        let mut context = Context::default();
        for hook in self.hooks.iter() {
            if hook.on_authenticate(OnAuthenticatePayload { doc_id: &doc_id, client_id, request: &self.request_info, context: &mut context }).await.is_err() {
                return None;
            }
        }
        
        let doc = self.root.ask(RequestDoc(Arc::clone(&doc_id))).send().await.ok()?;
        let init_msgs = doc.ask(ConnectClient { client: me.clone(), context }).send().await.ok()?;

        for payload in init_msgs {
            let wire = encode_with_doc_id(&doc_id, &payload);
            if self.sink.send(WsMessage::Binary(wire.to_vec().into())).await.is_err() { me.kill(); return None; }
        }

        self.docs.insert(doc_id, doc.clone());
        Some(doc)
    }
}
