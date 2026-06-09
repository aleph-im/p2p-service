use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use futures::Stream;
use libp2p::{Multiaddr, PeerId};
use log::{info, warn};
use tonic::{Request, Response, Status};

use crate::metrics::Metrics;
use crate::p2p::network::{classify_dial_error, DialErrorKind, P2PClient};
use crate::subscriptions::{now_millis, Envelope, Subscriptions};

pub mod proto {
    tonic::include_proto!("aleph.p2p.v1");
}

use proto::aleph_p2p_server::{AlephP2p, AlephP2pServer};

pub struct GrpcService {
    pub client: P2PClient,
    pub subscriptions: Arc<Subscriptions>,
    pub local_peer_id: PeerId,
    pub metrics: Metrics,
}

impl GrpcService {
    pub fn into_server(self) -> AlephP2pServer<GrpcService> {
        AlephP2pServer::new(self)
    }
}

// tonic::Status is large by design; boxing it here would only complicate the
// `?` plumbing in the handlers.
#[allow(clippy::result_large_err)]
fn parse_peer_id(value: &str) -> Result<PeerId, Status> {
    PeerId::from_str(value)
        .map_err(|e| Status::invalid_argument(format!("invalid peer id {value}: {e}")))
}

#[allow(clippy::result_large_err)]
fn parse_multiaddr(value: &str) -> Result<Multiaddr, Status> {
    Multiaddr::from_str(value)
        .map_err(|e| Status::invalid_argument(format!("invalid multiaddr {value}: {e}")))
}

/// Map a dial failure to a gRPC status, mirroring the HTTP endpoint mapping
/// (wrong peer id -> FAILED_PRECONDITION, unreachable -> NOT_FOUND).
fn map_dial_error_to_status(
    error: Box<dyn std::error::Error + Send>,
    peer_id: &PeerId,
    multiaddr: &Multiaddr,
) -> Status {
    match classify_dial_error(error.as_ref()) {
        DialErrorKind::WrongPeerId => {
            warn!(
                "Wrong peer ID dialing {} with multiaddr {}: {}",
                peer_id, multiaddr, error
            );
            Status::failed_precondition(format!("wrong peer id: {error}"))
        }
        DialErrorKind::Unreachable => {
            warn!(
                "Failed to dial {} with multiaddr {}: {}",
                peer_id, multiaddr, error
            );
            Status::not_found(format!("peer unreachable: {error}"))
        }
        DialErrorKind::Internal => Status::internal(error.to_string()),
    }
}

#[tonic::async_trait]
impl AlephP2p for GrpcService {
    async fn identify(
        &self,
        _request: Request<proto::IdentifyRequest>,
    ) -> Result<Response<proto::IdentifyResponse>, Status> {
        let mut client = self.client.clone();
        let node_info = client
            .identify()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(proto::IdentifyResponse {
            peer_id: node_info.peer_id.to_string(),
            listen_multiaddrs: node_info
                .listen_multiaddrs
                .iter()
                .map(|a| a.to_string())
                .collect(),
            external_multiaddrs: node_info
                .external_multiaddrs
                .iter()
                .map(|a| a.to_string())
                .collect(),
        }))
    }

    async fn dial(
        &self,
        request: Request<proto::DialRequest>,
    ) -> Result<Response<proto::DialResponse>, Status> {
        let request = request.into_inner();
        let peer_id = parse_peer_id(&request.peer_id)?;
        let multiaddr = parse_multiaddr(&request.multiaddr)?;

        let mut client = self.client.clone();
        client
            .dial_and_wait(peer_id, multiaddr.clone())
            .await
            .map_err(|e| map_dial_error_to_status(e, &peer_id, &multiaddr))?;
        Ok(Response::new(proto::DialResponse {}))
    }

    async fn publish(
        &self,
        request: Request<proto::PublishRequest>,
    ) -> Result<Response<proto::PublishResponse>, Status> {
        let request = request.into_inner();
        let topic = libp2p::gossipsub::IdentTopic::new(&request.topic);

        let mut client = self.client.clone();
        match client.publish(&topic, &request.payload).await {
            Ok(()) => {}
            Err(e) => {
                // Publishing on a mesh with no peers must not fail the caller:
                // with echo requested the local delivery still matters, and a
                // node briefly without mesh peers is not an error condition.
                let no_peers = matches!(
                    e.downcast_ref::<libp2p::gossipsub::PublishError>(),
                    Some(libp2p::gossipsub::PublishError::NoPeersSubscribedToTopic)
                );
                if !no_peers {
                    return Err(Status::internal(e.to_string()));
                }
                warn!("Publish on {} with no mesh peers", request.topic);
            }
        }
        self.metrics.total_messages_sent.inc();
        self.metrics.increment_event("message_published");

        if request.echo {
            self.subscriptions.publish(Envelope {
                topic: request.topic,
                source_peer_id: self.local_peer_id.to_string(),
                data: request.payload,
                received_at_millis: now_millis(),
            });
        }
        Ok(Response::new(proto::PublishResponse {}))
    }

    type SubscribeStream =
        Pin<Box<dyn Stream<Item = Result<proto::PubsubEnvelope, Status>> + Send>>;

    async fn subscribe(
        &self,
        request: Request<proto::SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let topic_name = request.into_inner().topic;

        // Create the broadcast receiver BEFORE joining the gossipsub topic so
        // that no message arriving in between is dropped, and before returning
        // the response so a publish issued right after Subscribe cannot be
        // missed (echo included).
        let mut receiver = self.subscriptions.subscribe(&topic_name);

        // Ensure the swarm is subscribed to the topic (idempotent).
        let mut client = self.client.clone();
        client
            .subscribe(&libp2p::gossipsub::IdentTopic::new(&topic_name))
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let metrics = self.metrics.clone();
        info!("New gRPC subscriber on topic {}", topic_name);

        let stream = async_stream::stream! {
            loop {
                match receiver.recv().await {
                    Ok(envelope) => {
                        yield Ok(proto::PubsubEnvelope {
                            topic: envelope.topic.clone(),
                            source_peer_id: envelope.source_peer_id.clone(),
                            payload: envelope.data.clone(),
                            received_at_millis: envelope.received_at_millis,
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("gRPC subscriber lagged, dropped {} messages", n);
                        metrics.increment_event("subscriber_lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        };
        Ok(Response::new(Box::pin(stream)))
    }

    async fn set_preferred_peers(
        &self,
        _request: Request<proto::SetPreferredPeersRequest>,
    ) -> Result<Response<proto::SetPreferredPeersResponse>, Status> {
        // Implemented in a later task.
        Err(Status::unimplemented("set_preferred_peers"))
    }

    async fn get_peers(
        &self,
        _request: Request<proto::GetPeersRequest>,
    ) -> Result<Response<proto::GetPeersResponse>, Status> {
        // Implemented in a later task.
        Err(Status::unimplemented("get_peers"))
    }
}
