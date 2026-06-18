use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct Envelope {
    pub topic: String,
    pub source_peer_id: String,
    pub data: Vec<u8>,
    pub received_at_millis: u64,
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-topic fanout of pubsub messages to local consumers (gRPC Subscribe
/// streams).
pub struct Subscriptions {
    capacity: usize,
    senders: RwLock<HashMap<String, broadcast::Sender<Arc<Envelope>>>>,
}

impl Subscriptions {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            senders: RwLock::new(HashMap::new()),
        }
    }

    pub fn subscribe(&self, topic: &str) -> broadcast::Receiver<Arc<Envelope>> {
        let mut senders = self.senders.write().expect("subscriptions lock poisoned");
        senders
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(self.capacity).0)
            .subscribe()
    }

    /// Best-effort publish; returns the number of receivers that got the message.
    pub fn publish(&self, envelope: Envelope) -> usize {
        let senders = self.senders.read().expect("subscriptions lock poisoned");
        match senders.get(&envelope.topic) {
            Some(sender) => sender.send(Arc::new(envelope)).unwrap_or(0),
            None => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(topic: &str, data: &[u8]) -> Envelope {
        Envelope {
            topic: topic.to_string(),
            source_peer_id: "QmPeer".to_string(),
            data: data.to_vec(),
            received_at_millis: 0,
        }
    }

    #[tokio::test]
    async fn subscriber_receives_published_message() {
        let subs = Subscriptions::new(16);
        let mut rx = subs.subscribe("topic-a");
        assert_eq!(subs.publish(envelope("topic-a", b"hello")), 1);
        let received = rx.recv().await.unwrap();
        assert_eq!(received.data, b"hello");
    }

    #[tokio::test]
    async fn publish_without_subscriber_is_dropped() {
        let subs = Subscriptions::new(16);
        assert_eq!(subs.publish(envelope("topic-a", b"hello")), 0);
    }

    #[tokio::test]
    async fn topics_are_isolated() {
        let subs = Subscriptions::new(16);
        let mut rx_a = subs.subscribe("topic-a");
        let _rx_b = subs.subscribe("topic-b");
        subs.publish(envelope("topic-b", b"for-b"));
        subs.publish(envelope("topic-a", b"for-a"));
        assert_eq!(rx_a.recv().await.unwrap().data, b"for-a");
    }

    #[tokio::test]
    async fn slow_subscriber_drops_oldest() {
        let subs = Subscriptions::new(2);
        let mut rx = subs.subscribe("topic-a");
        for i in 0..4u8 {
            subs.publish(envelope("topic-a", &[i]));
        }
        // The first recv reports the lag, subsequent recvs return the newest items.
        match rx.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => assert_eq!(n, 2),
            other => panic!("expected Lagged, got {:?}", other),
        }
        assert_eq!(rx.recv().await.unwrap().data, vec![2]);
        assert_eq!(rx.recv().await.unwrap().data, vec![3]);
    }
}
