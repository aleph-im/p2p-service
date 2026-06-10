use std::collections::hash_map::{DefaultHasher, Entry};
use std::collections::{HashMap, HashSet, LinkedList};
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, StreamExt};
use libp2p::gossipsub::{
    self, Behaviour as GossipsubBehaviour, Event as GossipsubEvent, Message as GossipsubMessage,
    MessageAuthenticity, MessageId, ValidationMode,
};
use libp2p::identify;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::dial_opts::DialOpts;
use libp2p::swarm::{ConnectionId, DialError, NetworkBehaviour, SwarmEvent};
use libp2p::{identity, noise, tcp, yamux, Multiaddr, PeerId, Swarm};
use log::{debug, info, warn};
use prometheus_client::metrics::gauge::Gauge;

use crate::p2p::peerstore::PeerStore;
use crate::p2p::subnet::{SubnetLimits, Verdict};

use crate::subscriptions::{now_millis, Envelope, Subscriptions};

/// Free-form protocol_version string exchanged in identify payloads;
/// not the identify wire protocol name.
pub const IDENTIFY_PROTOCOL_VERSION: &str = "/aleph/id/1.0.0";

/// Application score granted to preferred peers. `set_application_score`
/// only affects currently-tracked peers, so the score is applied both
/// when the preferred set is updated (for already-connected peers) and on
/// ConnectionEstablished (for peers that connect later).
pub const PREFERRED_PEER_APP_SCORE: f64 = 100.0;

/// Connection-limit parameters for the event loop.
#[derive(Debug, Clone)]
pub struct NetworkSettings {
    pub low_water: usize,
    pub high_water: usize,
    pub per_subnet_cap: usize,
    pub max_protected_share: f32,
}

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    gossipsub: GossipsubBehaviour,
    identify: identify::Behaviour,
}

fn make_gossipsub_config() -> gossipsub::Config {
    // NOTE: topic-key coupling -- the Subscriptions registry is keyed by the human-readable topic
    // name, which matches `message.topic.to_string()` only because topic hashing is not enabled;
    // enabling `.hash_topics()` would break the fanout keying.

    // To content-address messages, we take the hash of the message and use it as an ID.
    let message_id_fn = |message: &GossipsubMessage| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        MessageId::from(s.finish().to_string())
    };

    gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(1))
        .validation_mode(ValidationMode::Strict)
        .message_id_fn(message_id_fn)
        .max_transmit_size(262144)
        .max_messages_per_rpc(Some(100))
        .duplicate_cache_time(Duration::from_secs(1800))
        // Peer exchange: advertise alternative peers on PRUNE (rust-libp2p
        // exchanges peer IDs); helps mesh self-healing alongside the
        // registry-driven dialing.
        .do_px()
        .build()
        .expect("static gossipsub config should be valid")
}

pub async fn new(
    id_keys: identity::Keypair,
    settings: NetworkSettings,
    bootstrap_peers: HashSet<PeerId>,
    connected_peers: Gauge,
    subscriptions: Arc<Subscriptions>,
    peerstore: Arc<Mutex<PeerStore>>,
) -> Result<(P2PClient, EventLoop), Box<dyn Error>> {
    let swarm = libp2p::SwarmBuilder::with_existing_identity(id_keys)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            // Mplex is deprecated; kept as fallback to talk to pre-N nodes.
            (yamux::Config::default, libp2p_mplex::Config::default),
        )?
        .with_dns()?
        .with_behaviour(|key| {
            let mut gossipsub = GossipsubBehaviour::new(
                MessageAuthenticity::Signed(key.clone()),
                make_gossipsub_config(),
            )
            .expect("static gossipsub behaviour config should be valid");

            let score_params = gossipsub::PeerScoreParams {
                // Makes set_application_score effective: preferred peers get a
                // positive bonus, everyone else stays at the neutral default of 0.
                app_specific_weight: 1.0,
                ..Default::default()
            };
            gossipsub
                .with_peer_score(score_params, gossipsub::PeerScoreThresholds::default())
                .expect("static peer score params should be valid");
            let identify = identify::Behaviour::new(
                identify::Config::new(IDENTIFY_PROTOCOL_VERSION.to_string(), key.public())
                    .with_agent_version(format!("aleph-p2p-service/{}", env!("CARGO_PKG_VERSION"))),
            );
            Behaviour {
                gossipsub,
                identify,
            }
        })?
        // Gossipsub mesh links see periodic traffic; non-mesh connections
        // must survive between heartbeats and maintenance ticks.
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();

    let (command_sender, command_receiver) = mpsc::channel(0);
    Ok((
        P2PClient {
            sender: command_sender,
        },
        EventLoop::new(
            swarm,
            command_receiver,
            subscriptions,
            connected_peers,
            peerstore,
            settings,
            bootstrap_peers,
        ),
    ))
}

type CommandResponseSender<T = ()> = oneshot::Sender<Result<T, Box<dyn Error + Send>>>;
type CommandResponseReceiver<T = ()> = oneshot::Receiver<Result<T, Box<dyn Error + Send>>>;

#[derive(Debug)]
pub struct NodeInfo {
    pub peer_id: PeerId,
    pub listen_multiaddrs: Vec<Multiaddr>,
    pub external_multiaddrs: Vec<Multiaddr>,
}

/// Point-in-time view of the connection table, used by the maintenance loop.
#[derive(Debug, Clone)]
pub struct NetworkSnapshot {
    pub connected: HashSet<PeerId>,
    /// Preferred peers and their known dial-hint addresses.
    pub preferred: Vec<(PeerId, Vec<Multiaddr>)>,
}

/// Per-peer view returned by [`P2PClient::get_peers`].
#[derive(Debug, Clone)]
pub struct PeerSnapshot {
    pub peer_id: PeerId,
    pub multiaddrs: Vec<Multiaddr>,
    pub preferred: bool,
    pub score: f64,
}

#[derive(Debug)]
enum Command {
    StartListening {
        addr: Multiaddr,
        sender: CommandResponseSender,
    },
    Dial {
        peer_id: PeerId,
        peer_addrs: Vec<Multiaddr>,
        sender: CommandResponseSender,
    },
    Identify {
        sender: CommandResponseSender<NodeInfo>,
    },
    Subscribe {
        topic: gossipsub::IdentTopic,
        sender: CommandResponseSender,
    },
    PublishMessage {
        topic: gossipsub::IdentTopic,
        message: Vec<u8>,
        sender: CommandResponseSender,
    },
    NetworkSnapshot {
        sender: CommandResponseSender<NetworkSnapshot>,
    },
    SetPreferredPeers {
        peers: Vec<(PeerId, Vec<Multiaddr>)>,
        sender: CommandResponseSender<(u32, u32)>, // (accepted, truncated)
    },
    GetPeers {
        sender: CommandResponseSender<Vec<PeerSnapshot>>,
    },
}

#[derive(Clone)]
pub struct P2PClient {
    sender: mpsc::Sender<Command>,
}

#[derive(Debug)]
pub struct ChannelClosed;

impl std::fmt::Display for ChannelClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "P2P event loop is not running")
    }
}

impl Error for ChannelClosed {}

impl P2PClient {
    async fn send_command_and_wait<TResponse>(
        &mut self,
        command: Command,
        receiver: CommandResponseReceiver<TResponse>,
    ) -> Result<TResponse, Box<dyn Error + Send>> {
        if self.sender.send(command).await.is_err() {
            return Err(Box::new(ChannelClosed));
        }
        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(Box::new(ChannelClosed)),
        }
    }

    pub async fn start_listening(&mut self, addr: Multiaddr) -> Result<(), Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(Command::StartListening { addr, sender }, receiver)
            .await
    }

    pub async fn dial_and_wait(
        &mut self,
        peer_id: PeerId,
        peer_addrs: Vec<Multiaddr>,
    ) -> Result<(), Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(
            Command::Dial {
                peer_id,
                peer_addrs,
                sender,
            },
            receiver,
        )
        .await
    }

    pub async fn identify(&mut self) -> Result<NodeInfo, Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(Command::Identify { sender }, receiver)
            .await
    }

    pub async fn subscribe(
        &mut self,
        topic: &gossipsub::IdentTopic,
    ) -> Result<(), Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(
            Command::Subscribe {
                topic: topic.clone(),
                sender,
            },
            receiver,
        )
        .await
    }

    pub async fn publish(
        &mut self,
        topic: &gossipsub::IdentTopic,
        message: &[u8],
    ) -> Result<(), Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(
            Command::PublishMessage {
                topic: topic.clone(),
                message: message.to_vec(),
                sender,
            },
            receiver,
        )
        .await
    }

    pub async fn network_snapshot(&mut self) -> Result<NetworkSnapshot, Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(Command::NetworkSnapshot { sender }, receiver)
            .await
    }

    pub async fn set_preferred_peers(
        &mut self,
        peers: Vec<(PeerId, Vec<Multiaddr>)>,
    ) -> Result<(u32, u32), Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(Command::SetPreferredPeers { peers, sender }, receiver)
            .await
    }

    pub async fn get_peers(&mut self) -> Result<Vec<PeerSnapshot>, Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(Command::GetPeers { sender }, receiver)
            .await
    }
}

pub struct EventLoop {
    swarm: Swarm<Behaviour>,
    command_receiver: mpsc::Receiver<Command>,
    subscriptions: Arc<Subscriptions>,
    pending_dials: HashMap<PeerId, LinkedList<CommandResponseSender>>,
    connected_peers: Gauge,
    peerstore: Arc<Mutex<PeerStore>>,
    settings: NetworkSettings,
    subnet_limits: SubnetLimits,
    /// Tracks the remote addresses per connected peer.
    connected: HashMap<PeerId, Vec<Multiaddr>>,
    /// Peers that are never disconnected for limit enforcement (bootstrap + preferred).
    protected: HashSet<PeerId>,
    /// Bootstrap peers stay protected regardless of the preferred set.
    bootstrap_peers: HashSet<PeerId>,
    /// Dial hints for preferred peers, used by the maintenance loop.
    dial_hints: HashMap<PeerId, Vec<Multiaddr>>,
}

impl EventLoop {
    fn new(
        swarm: Swarm<Behaviour>,
        command_receiver: mpsc::Receiver<Command>,
        subscriptions: Arc<Subscriptions>,
        connected_peers: Gauge,
        peerstore: Arc<Mutex<PeerStore>>,
        settings: NetworkSettings,
        bootstrap_peers: HashSet<PeerId>,
    ) -> Self {
        let subnet_limits = SubnetLimits::new(settings.per_subnet_cap);
        let protected = bootstrap_peers.clone();
        Self {
            swarm,
            command_receiver,
            subscriptions,
            pending_dials: Default::default(),
            connected_peers,
            peerstore,
            settings,
            subnet_limits,
            connected: HashMap::new(),
            protected,
            bootstrap_peers,
            dial_hints: HashMap::new(),
        }
    }

    pub async fn run(mut self) {
        loop {
            futures::select! {
                event = self.swarm.select_next_some() => self.handle_event(event).await,
                command = self.command_receiver.next() => match command {
                    Some(c) => self.handle_command(c).await,
                    // Command channel closed: shut down the event loop.
                    None => return,
                }
            }
        }
    }

    async fn handle_event(&mut self, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(GossipsubEvent::Message {
                message,
                ..
            })) => match message.source {
                None => warn!("Received pubsub message without source; discarding"),
                Some(source) => {
                    let receivers = self.subscriptions.publish(Envelope {
                        topic: message.topic.to_string(),
                        source_peer_id: source.to_string(),
                        data: message.data,
                        received_at_millis: now_millis(),
                    });
                    debug!(
                        "Forwarded pubsub message to {} local subscribers",
                        receivers
                    );
                }
            },
            SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(other)) => {
                debug!("Unhandled gossipsub event: {:?}", other);
            }
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
                ..
            })) => {
                debug!(
                    "Identify from {}: {} listen addrs",
                    peer_id,
                    info.listen_addrs.len()
                );
                let tcp_addrs = info
                    .listen_addrs
                    .into_iter()
                    .filter(|a| a.iter().any(|p| matches!(p, Protocol::Tcp(_))))
                    .collect::<Vec<_>>();
                if !tcp_addrs.is_empty() {
                    self.peerstore
                        .lock()
                        .expect("peerstore lock poisoned")
                        .record(&peer_id, tcp_addrs, crate::p2p::peerstore::now_unix());
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Identify(other)) => {
                debug!("Identify event: {:?}", other);
            }
            SwarmEvent::ConnectionEstablished {
                peer_id,
                endpoint,
                connection_id,
                ..
            } => {
                self.connected_peers.inc();
                if endpoint.is_dialer() {
                    // Only persist addresses we successfully dialed; inbound remote addrs
                    // are ephemeral source ports that are never dialable and would pollute
                    // the peerstore, evicting real listen addresses via the 8-addr cap.
                    self.peerstore
                        .lock()
                        .expect("peerstore lock poisoned")
                        .record(
                            &peer_id,
                            [endpoint.get_remote_address().clone()],
                            crate::p2p::peerstore::now_unix(),
                        );
                    if let Some(senders) = self.pending_dials.remove(&peer_id) {
                        debug!("Successfully dialed {}", peer_id);
                        for sender in senders {
                            let _ = sender.send(Ok(()));
                        }
                    }
                }

                let remote_addr = endpoint.get_remote_address().clone();
                self.connected
                    .entry(peer_id)
                    .or_default()
                    .push(remote_addr.clone());

                // Grant preferred/bootstrap peers their application-score
                // bonus on connect: set_application_score only applies to
                // currently-connected peers, so the call made when the
                // preferred set was updated cannot cover peers connecting
                // later. Scoring is enabled; the returned bool is false only
                // for peers not currently tracked by the scorer.
                if self.dial_hints.contains_key(&peer_id) || self.bootstrap_peers.contains(&peer_id)
                {
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .set_application_score(&peer_id, PREFERRED_PEER_APP_SCORE);
                }

                self.enforce_connection_limits(peer_id, connection_id, &remote_addr);
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                endpoint,
                num_established,
                ..
            } => {
                self.connected_peers.dec();
                self.subnet_limits
                    .on_disconnection(endpoint.get_remote_address());
                if num_established == 0 {
                    self.connected.remove(&peer_id);
                } else if let Some(addrs) = self.connected.get_mut(&peer_id) {
                    addrs.retain(|a| a != endpoint.get_remote_address());
                }
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(peer_id) = peer_id {
                    if let Some(senders) = self.pending_dials.remove(&peer_id) {
                        debug!("Failed to dial {}: {}", peer_id, error);
                        let arced_error = Arc::new(error);
                        for sender in senders {
                            let _ = sender.send(Err(Box::new(DialFailed(arced_error.clone()))));
                        }
                    }
                }
            }
            SwarmEvent::NewListenAddr { address, .. } => {
                let local_peer_id = *self.swarm.local_peer_id();
                info!(
                    "Local node is listening on {:?}",
                    address.with(Protocol::P2p(local_peer_id))
                );
            }
            other => debug!("Unhandled swarm event: {:?}", other),
        }
    }

    /// Applies connection limits to a freshly established connection.
    ///
    /// Over the high watermark, the peer is the unit: all of its connections
    /// are dropped. A subnet-cap violation only affects the address that was
    /// connected, so only the violating connection is closed.
    fn enforce_connection_limits(
        &mut self,
        peer_id: PeerId,
        connection_id: ConnectionId,
        remote_addr: &Multiaddr,
    ) {
        let exempt = self.protected.contains(&peer_id);
        let subnet_verdict = self.subnet_limits.on_connection(remote_addr, exempt);
        let over_high_water = self.connected.len() > self.settings.high_water && !exempt;
        if over_high_water {
            info!(
                "Disconnecting {} (over high water; subnet cap exceeded: {})",
                peer_id,
                subnet_verdict == Verdict::Reject,
            );
            let _ = self.swarm.disconnect_peer_id(peer_id);
        } else if subnet_verdict == Verdict::Reject {
            info!(
                "Closing connection {:?} to {} (subnet cap exceeded)",
                connection_id, peer_id,
            );
            self.swarm.close_connection(connection_id);
        }
    }

    async fn handle_command(&mut self, command: Command) {
        match command {
            Command::StartListening { addr, sender } => {
                let _ = match self.swarm.listen_on(addr) {
                    Ok(_) => sender.send(Ok(())),
                    Err(e) => sender.send(Err(Box::new(e))),
                };
            }
            Command::Dial {
                peer_id,
                peer_addrs,
                sender,
            } => match self.pending_dials.entry(peer_id) {
                Entry::Occupied(mut entry) => {
                    // A dial to this peer is already in flight: queue the sender and
                    // ignore the new `peer_addrs` (the in-flight dial's addresses win).
                    entry.get_mut().push_back(sender);
                }
                Entry::Vacant(entry) => {
                    let opts = DialOpts::peer_id(peer_id).addresses(peer_addrs).build();
                    match self.swarm.dial(opts) {
                        Ok(()) => {
                            entry.insert(LinkedList::from([sender]));
                        }
                        // Already connected (or already dialing): the goal of being
                        // connected to the peer is achieved, so report success.
                        Err(DialError::DialPeerConditionFalse(_)) => {
                            let _ = sender.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = sender.send(Err(Box::new(e)));
                        }
                    }
                }
            },
            Command::Identify { sender } => {
                let node_info = NodeInfo {
                    peer_id: *self.swarm.local_peer_id(),
                    listen_multiaddrs: self.swarm.listeners().cloned().collect(),
                    external_multiaddrs: self.swarm.external_addresses().cloned().collect(),
                };
                let _ = sender.send(Ok(node_info));
            }
            Command::Subscribe { topic, sender } => {
                let _ = match self.swarm.behaviour_mut().gossipsub.subscribe(&topic) {
                    Ok(_) => sender.send(Ok(())),
                    Err(e) => sender.send(Err(Box::new(e))),
                };
            }
            Command::PublishMessage {
                topic,
                message,
                sender,
            } => {
                let _ = match self.swarm.behaviour_mut().gossipsub.publish(topic, message) {
                    Ok(_) => sender.send(Ok(())),
                    Err(e) => sender.send(Err(Box::new(e))),
                };
            }
            Command::NetworkSnapshot { sender } => {
                let snapshot = NetworkSnapshot {
                    connected: self.connected.keys().copied().collect(),
                    preferred: self
                        .dial_hints
                        .iter()
                        .map(|(peer_id, addrs)| (*peer_id, addrs.clone()))
                        .collect(),
                };
                let _ = sender.send(Ok(snapshot));
            }
            Command::SetPreferredPeers { peers, sender } => {
                // Cap the protected set so a poisoned registry cannot monopolize
                // connection slots. Bootstrap peers are always protected on top.
                let cap =
                    (self.settings.high_water as f32 * self.settings.max_protected_share) as usize;
                let truncated = peers.len().saturating_sub(cap) as u32;
                let accepted_peers: Vec<_> = peers.into_iter().take(cap).collect();
                let accepted = accepted_peers.len() as u32;

                // Reset scores of peers that are no longer preferred.
                let new_set: HashSet<PeerId> =
                    accepted_peers.iter().map(|(peer_id, _)| *peer_id).collect();
                for old_peer in self.protected.clone() {
                    if !new_set.contains(&old_peer) && !self.bootstrap_peers.contains(&old_peer) {
                        self.protected.remove(&old_peer);
                        self.swarm
                            .behaviour_mut()
                            .gossipsub
                            .set_application_score(&old_peer, 0.0);
                    }
                }

                self.dial_hints.clear();
                for (peer_id, addrs) in accepted_peers {
                    self.protected.insert(peer_id);
                    // Scoring is enabled; this only affects peers currently
                    // tracked by the scorer (returns false otherwise).
                    // Preferred peers that connect later get their bonus from
                    // the ConnectionEstablished hook in handle_event.
                    self.swarm
                        .behaviour_mut()
                        .gossipsub
                        .set_application_score(&peer_id, PREFERRED_PEER_APP_SCORE);
                    self.dial_hints.insert(peer_id, addrs);
                }
                info!(
                    "Preferred peer set updated: {} accepted, {} truncated",
                    accepted, truncated
                );
                let _ = sender.send(Ok((accepted, truncated)));
            }
            Command::GetPeers { sender } => {
                let peers = self
                    .connected
                    .iter()
                    .map(|(peer_id, addrs)| PeerSnapshot {
                        peer_id: *peer_id,
                        multiaddrs: addrs.clone(),
                        preferred: self.protected.contains(peer_id),
                        score: self
                            .swarm
                            .behaviour()
                            .gossipsub
                            .peer_score(peer_id)
                            .unwrap_or(0.0),
                    })
                    .collect();
                let _ = sender.send(Ok(peers));
            }
        }
    }
}

/// Error returned to dial requesters when an outgoing connection attempt fails.
/// Wraps the swarm dial error so callers can inspect the failure reason.
#[derive(Debug)]
pub struct DialFailed(Arc<libp2p::swarm::DialError>);

impl DialFailed {
    pub fn dial_error(&self) -> &libp2p::swarm::DialError {
        &self.0
    }
}

impl std::fmt::Display for DialFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for DialFailed {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// Classification of a `dial_and_wait` failure, shared by the HTTP and gRPC
/// error mappings so both APIs report dial failures consistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialErrorKind {
    /// The remote answered with a peer ID different from the requested one.
    WrongPeerId,
    /// The connection attempt failed (peer unreachable, transport error, ...).
    Unreachable,
    /// Not a dial error (e.g. the event loop is gone).
    Internal,
}

/// Classify an error returned by [`P2PClient::dial_and_wait`].
///
/// Dial failures can surface either as an immediate `DialError` (the swarm
/// rejected the dial) or as a [`DialFailed`] reported by the event loop once
/// the connection attempt fails.
pub fn classify_dial_error(error: &(dyn Error + Send + 'static)) -> DialErrorKind {
    let dial_error = match error.downcast_ref::<DialFailed>() {
        Some(dial_failed) => Some(dial_failed.dial_error()),
        None => error.downcast_ref::<libp2p::swarm::DialError>(),
    };
    match dial_error {
        Some(libp2p::swarm::DialError::WrongPeerId { .. }) => DialErrorKind::WrongPeerId,
        Some(_) => DialErrorKind::Unreachable,
        None => DialErrorKind::Internal,
    }
}
