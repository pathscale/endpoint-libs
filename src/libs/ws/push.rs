#![allow(dead_code)]

use alloc::collections::BTreeMap as HashMap;
use alloc::vec::Vec;
use core::hash::Hash;
use parking_lot::RwLock;
use serde::Serialize;
use tracing::error;

use crate::libs::toolbox::{ArcToolbox, RequestContext};

use super::{ConnectionId, WsResponseGeneric, WsStreamResponseGeneric};

pub struct SubscriberContext {
    pub ctx: RequestContext,
    pub stream_seq: u32,
}

#[derive(Default)]
pub struct Subscribers {
    pub subscribers: HashMap<ConnectionId, SubscriberContext>,
}

pub struct SubscribeManager<Key> {
    pub topics: RwLock<HashMap<Key, Subscribers>>,
}

impl<Key: Hash + Eq + Ord + Into<u32>> Default for SubscribeManager<Key> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Key: Hash + Eq + Ord + Into<u32>> SubscribeManager<Key> {
    pub fn new() -> Self {
        Self {
            topics: RwLock::new(HashMap::new()),
        }
    }

    pub fn add_topics(&self, topics: Vec<Key>) {
        let mut map = self.topics.write();
        for topic in topics {
            map.entry(topic).or_default();
        }
    }
    pub fn add_topic(&self, topic: Key) {
        self.topics.write().entry(topic).or_default();
    }
    pub fn subscribe_multi(&self, topics: Vec<Key>, ctx: RequestContext) {
        for topic in topics {
            self.subscribe(topic, ctx.clone());
        }
    }
    pub fn subscribe(&self, topic: Key, ctx: RequestContext) {
        self.topics
            .write()
            .entry(topic)
            .or_default()
            .subscribers
            .insert(ctx.connection_id, SubscriberContext { ctx, stream_seq: 0 });
    }
    pub fn unsubscribe_multi(&self, topics: Vec<Key>, connection_id: ConnectionId) {
        for topic in topics {
            self.unsubscribe(topic, connection_id);
        }
    }
    pub fn unsubscribe(&self, topic: Key, connection_id: ConnectionId) {
        if let Some(subscribers) = self.topics.write().get_mut(&topic) {
            subscribers.subscribers.remove(&connection_id);
        }
    }
    pub fn publish_with_filter(
        &self,
        toolbox: &ArcToolbox,
        topic: Key,
        msg: &impl Serialize,
        filter: impl Fn(&RequestContext) -> bool,
    ) {
        let data = match serde_json::value::to_raw_value(msg) {
            Ok(d) => d,
            Err(e) => {
                error!(ws_server=true, err=%e, "Failed to serialize stream data - skipping publish");
                return;
            }
        };
        let mut deliveries = Vec::new();
        {
            let mut map = self.topics.write();
            let Some(topic_2) = map.get_mut(&topic) else {
                return;
            };
            let stream_code = topic.into();
            for sub in topic_2.subscribers.values_mut() {
                if !filter(&sub.ctx) {
                    continue;
                }
                let outbound = WsResponseGeneric::Stream(WsStreamResponseGeneric {
                    original_seq: sub.ctx.seq,
                    method: sub.ctx.method,
                    stream_seq: sub.stream_seq,
                    stream_code,
                    data: data.clone(),
                });
                sub.stream_seq += 1;
                deliveries.push((sub.ctx.connection_id, outbound));
            }
            let mut dead_connections = Vec::new();
            for (conn_id, outbound) in deliveries {
                if !toolbox.send_to_subscriber(conn_id, outbound) {
                    dead_connections.push(conn_id);
                }
            }
            for conn_id in dead_connections {
                topic_2.subscribers.remove(&conn_id);
            }
        }
    }
    pub fn publish_to_all(&self, toolbox: &ArcToolbox, topic: Key, msg: &impl Serialize) {
        self.publish_with_filter(toolbox, topic, msg, |_| true)
    }
}
