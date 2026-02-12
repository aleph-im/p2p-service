use std::collections::{HashMap, LinkedList};
use std::collections::hash_map::{DefaultHasher, Entry};
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use futures::{
    channel::{mpsc, oneshot},
    SinkExt, StreamExt,
};
use libp2p::{
    core::upgrade,
    dns::TokioDnsConfig,
    gossipsub, identity, Multiaddr,
    noise,
    PeerId,
    swarm::{NetworkBehaviour, SwarmBuilder, SwarmEvent},
    Swarm,
    tcp::tokio::Transport as TcpTransport, Transport,
    yamux,
};
use libp2p::core::upgrade::SelectUpgrade;
use libp2p::gossipsub::{
    Behaviour as GossipsubBehaviour, Event as GossipsubEvent, Message as GossipsubMessage,
    MessageAuthenticity, MessageId, ValidationMode,
};
use libp2p::multiaddr::Protocol;
use libp2p::tcp::Config as GenTcpConfig;
use log::{debug, info};
use prometheus_client::metrics::gauge::Gauge;

use libp2p_mplex;

fn make_transport(
    id_keys: &identity::Keypair,
) -> std::io::Result<libp2p::core::transport::Boxed<(PeerId, libp2p::core::muxing::StreamMuxerBox)>>
{
    let tcp_transport = TcpTransport::new(GenTcpConfig::default().nodelay(true));
    let dns_transport =
        TokioDnsConfig::system(tcp_transport).expect("should be able to create DNS transport");

    let yamux_config = yamux::Config::default();
    // Mplex is deprecated, we just support it to communicate with older nodes.
    let mplex_config = libp2p_mplex::MplexConfig::new();
    let multiplex_upgrade = SelectUpgrade::new(yamux_config, mplex_config);

    Ok(dns_transport
        .upgrade(upgrade::Version::V1)
        .authenticate(
            noise::Config::new(id_keys).expect("signing libp2p-noise static DH keypair failed"),
        )
        .multiplex(multiplex_upgrade)
        .boxed())
}

pub async fn new(
    id_keys: identity::Keypair,
    connected_peers: Gauge,
) -> Result<(P2PClient, impl StreamExt<Item = Event>, EventLoop), Box<dyn Error>> {
    // Create a public/private key pair, either random or based on a seed.
    let peer_id = PeerId::from(id_keys.public());

    let transport = make_transport(&id_keys).expect("should be able to create transport");

    let swarm = {
        // To content-address message, we can take the hash of message and use it as an ID.
        let message_id_fn = |message: &GossipsubMessage| {
            let mut s = DefaultHasher::new();
            message.data.hash(&mut s);
            MessageId::from(s.finish().to_string())
        };

        // Set a custom gossipsub
        let gossipsub_config = gossipsub::ConfigBuilder::default()
            .heartbeat_interval(Duration::from_secs(1)) // This is set to aid debugging by not cluttering the log space
            .validation_mode(ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
            .message_id_fn(message_id_fn) // content-address messages. No two messages of the
            // same content will be propagated.
            .max_transmit_size(262144) // Inline messages can be quite large, up to over 200KB.
            .max_messages_per_rpc(Some(100))
            .duplicate_cache_time(Duration::from_secs(1800))
            .build()
            .expect("Valid config");

        // build a gossipsub network behaviour
        let gossipsub: GossipsubBehaviour =
            GossipsubBehaviour::new(MessageAuthenticity::Signed(id_keys), gossipsub_config)
                .expect("Correct configuration");

        let behaviour = MyBehaviour { gossipsub };

        SwarmBuilder::with_tokio_executor(transport, behaviour, peer_id)
            .build()
    };

    let (command_sender, command_receiver) = mpsc::channel(0);
    let (event_sender, event_receiver) = mpsc::channel(0);
    Ok((
        P2PClient {
            sender: command_sender,
        },
        event_receiver,
        EventLoop::new(swarm, command_receiver, event_sender, connected_peers),
    ))
}

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "MyBehaviourEvent")]
struct MyBehaviour {
    gossipsub: GossipsubBehaviour,
}

#[allow(clippy::large_enum_variant)]
enum MyBehaviourEvent {
    Gossipsub(GossipsubEvent),
}

impl From<GossipsubEvent> for MyBehaviourEvent {
    fn from(event: GossipsubEvent) -> Self {
        MyBehaviourEvent::Gossipsub(event)
    }
}

impl Debug for MyBehaviourEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Gossipsub event")
    }
}

type CommandResponseSender<T = ()> = oneshot::Sender<Result<T, Box<dyn Error + Send>>>;
type CommandResponseReceiver<T = ()> = oneshot::Receiver<Result<T, Box<dyn Error + Send>>>;

#[derive(Debug)]
pub struct NodeInfo {
    pub peer_id: PeerId,
    pub multiaddrs: Vec<Multiaddr>,
}

#[derive(Debug)]
enum Command {
    StartListening {
        addr: Multiaddr,
        sender: CommandResponseSender,
    },
    Dial {
        peer_id: PeerId,
        peer_addr: Multiaddr,
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
}

#[derive(Clone)]
pub struct P2PClient {
    /// A channel to send commands to the P2P network task.
    sender: mpsc::Sender<Command>,
}

impl P2PClient {
    async fn send_command(&mut self, command: Command) {
        self.sender
            .send(command)
            .await
            .expect("Command receiver should not to be dropped");
    }

    async fn send_command_and_wait<TResponse>(
        &mut self,
        command: Command,
        receiver: CommandResponseReceiver<TResponse>,
    ) -> Result<TResponse, Box<dyn Error + Send>> {
        self.sender
            .send(command)
            .await
            .expect("Command receiver should not to be dropped");
        receiver.await.expect("Sender should not be dropped")
    }

    /// Start listening for P2P connections from other nodes.
    pub async fn start_listening(&mut self, addr: Multiaddr) -> Result<(), Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(Command::StartListening { addr, sender }, receiver)
            .await
    }

    /// Dial a peer.
    pub async fn dial(
        &mut self,
        peer_id: PeerId,
        peer_addr: Multiaddr,
    ) -> CommandResponseReceiver<()> {
        let (sender, receiver) = oneshot::channel();
        self.send_command(Command::Dial {
            peer_id,
            peer_addr,
            sender,
        })
        .await;
        receiver
    }

    /// Dial a peer and wait for the operation to complete.
    pub async fn dial_and_wait(
        &mut self,
        peer_id: PeerId,
        peer_addr: Multiaddr,
    ) -> Result<(), Box<dyn Error + Send>> {
        let (sender, receiver) = oneshot::channel();
        self.send_command_and_wait(
            Command::Dial {
                peer_id,
                peer_addr,
                sender,
            },
            receiver,
        )
        .await
    }

    /// Get information about this node.
    pub async fn identify(&mut self) -> CommandResponseReceiver<NodeInfo> {
        let (sender, receiver) = oneshot::channel();
        self.send_command(Command::Identify { sender }).await;
        receiver
    }

    /// Subscribe to a pubsub topic.
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

    /// Publish a message on a pubsub topic.
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
}

pub enum Event {
    PubsubMessage {
        propagation_source: PeerId,
        message_id: MessageId,
        message: GossipsubMessage,
    },
}

pub struct EventLoop {
    swarm: Swarm<MyBehaviour>,
    command_receiver: mpsc::Receiver<Command>,
    event_sender: mpsc::Sender<Event>,
    pending_dials: HashMap<PeerId, LinkedList<CommandResponseSender>>,
    connected_peers: Gauge,
}

impl EventLoop {
    fn new(
        swarm: Swarm<MyBehaviour>,
        command_receiver: mpsc::Receiver<Command>,
        event_sender: mpsc::Sender<Event>,
        connected_peers: Gauge,
    ) -> Self {
        Self {
            swarm,
            command_receiver,
            event_sender,
            pending_dials: Default::default(),
            connected_peers,
        }
    }

    pub async fn run(mut self) {
        loop {
            futures::select! {
                event = self.swarm.next() => self.handle_event(event.expect("Swarm stream to be infinite")).await,
                command = self.command_receiver.next() => match command {
                    Some(c) => self.handle_command(c).await,
                    // Command channel closed, shut down the event loop.
                    None => return,
                }
            }
        }
    }

    async fn handle_event(&mut self, event: SwarmEvent<MyBehaviourEvent, void::Void>) {
        match event {
            SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub_event)) => {
                debug!("{:?}", gossipsub_event);
                match gossipsub_event {
                    GossipsubEvent::Message {
                        propagation_source,
                        message_id,
                        message,
                    } => {
                        self.event_sender
                            .send(Event::PubsubMessage {
                                propagation_source,
                                message_id,
                                message,
                            })
                            .await
                            .expect("receiver should not be dropped");
                    }
                    gossipsub_event => {
                        debug!("Unhandled Gossipsub event: {:?}", gossipsub_event)
                    }
                }
            }
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                self.connected_peers.inc();
                if endpoint.is_dialer() {
                    if let Some(senders) = self.pending_dials.remove(&peer_id) {
                        debug!("Successfully dialed {}", peer_id);
                        for sender in senders {
                            let _ = sender.send(Ok(()));
                        }
                    }
                }
            }
            SwarmEvent::ConnectionClosed { .. } => {
                self.connected_peers.dec();
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(peer_id) = peer_id {
                    if let Some(senders) = self.pending_dials.remove(&peer_id) {
                        debug!("Failed to dial {}", peer_id);
                        let arced_error = Arc::new(error);
                        for sender in senders {
                            let _ = sender.send(Err(Box::new(arced_error.clone())));
                        }
                    }
                }
            }
            SwarmEvent::NewListenAddr { address, .. } => {
                let local_peer_id = *self.swarm.local_peer_id();
                info!(
                    "Local node is listening on {:?}",
                    address.with(Protocol::P2p(local_peer_id.into()))
                );
            }
            SwarmEvent::Dialing(peer_id) => {
                debug!("Dialing {}...", peer_id)
            }
            swarm_event => debug!("Unhandled swarm event: {:?}", swarm_event),
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
                peer_addr,
                sender,
            } => match self.pending_dials.entry(peer_id) {
                Entry::Occupied(mut entry) => {
                    let senders = entry.get_mut();
                    senders.push_back(sender);
                }
                Entry::Vacant(entry) => {
                    match self
                        .swarm
                        .dial(peer_addr.with(Protocol::P2p(peer_id.into())))
                    {
                        Ok(()) => {
                            entry.insert(LinkedList::from([sender]));
                        }
                        Err(e) => {
                            let _ = sender.send(Err(Box::new(e)));
                        }
                    }
                }
            },
            Command::Identify { sender } => {
                let multiaddrs = self
                    .swarm
                    .external_addresses()
                    .map(|record| record.addr.clone())
                    .collect();
                let _ = sender.send(Ok(NodeInfo {
                    peer_id: *self.swarm.local_peer_id(),
                    multiaddrs,
                }));
            }
            Command::Subscribe { topic, sender } => {
                if let Err(e) = self.swarm.behaviour_mut().gossipsub.subscribe(&topic) {
                    let _ = sender.send(Err(Box::new(e)));
                } else {
                    let _ = sender.send(Ok(()));
                }
            }
            Command::PublishMessage {
                topic,
                message,
                sender,
            } => {
                if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(topic, message) {
                    let _ = sender.send(Err(Box::new(e)));
                } else {
                    let _ = sender.send(Ok(()));
                }
            }
        }
    }
}
