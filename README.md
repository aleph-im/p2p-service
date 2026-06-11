# Aleph.im P2P Service

A P2P service for aleph.im nodes.

## Purpose

Aleph.im nodes use P2P communication for a variety of purposes. This service enables multiple
processes written in different languages to interact with the P2P network.

## Features

* Dial peers on the P2P network
* Publish/subscribe to P2P topics using gossipsub
* Get information about the local P2P node, such as the peer ID
* Maintain a healthy connection table: bootstrap anchoring, low/high watermarks,
  per-subnet caps and registry-driven preferred peers
* Persist known peers across restarts (peerstore file)

## Usage

The service exposes two network interfaces:

* a gRPC API on `grpc_port` (default 4030) for control and pubsub,
* an HTTP server on `metrics_port` (default 4040) serving `/metrics`
  (Prometheus format) and `/health`.

See [the demo directory](scripts/demo) for details on how to set up and configure the service.

### gRPC API

The proto contract lives in [`proto/aleph_p2p.proto`](proto/aleph_p2p.proto) and is shared
with pyaleph (the Core Channel Node software). The `AlephP2P` service provides:

* `Identify`: returns the local peer ID and its listen/external multiaddrs.
* `Dial`: connects to a remote peer given its peer ID and multiaddr; returns once
  the connection is established or failed.
* `Publish`: publishes a payload on a gossipsub topic. With `echo=true`, the message
  is also delivered to local `Subscribe` streams for the topic, so local consumers
  see the node's own messages.
* `Subscribe`: server-streaming RPC delivering pubsub messages received on a topic.
  Each envelope carries the topic, the signed source peer ID and the local reception
  time. The per-subscriber buffer is bounded; on overflow the oldest messages are
  dropped and counted in metrics.
* `SetPreferredPeers`: replaces the full set of preferred peers (registry-backed
  nodes). Preferred peers get protected connection slots (never pruned by the
  connection limits) and a gossipsub application-score bonus. The protected set is
  capped at `max_protected_share` of `high_water`; entries beyond the cap are
  reported as truncated.
* `GetPeers`: lists currently connected peers with their multiaddrs, preferred flag
  and gossipsub score.

The node also subscribes at startup to every topic listed in the `p2p.topics`
configuration variable.

**Security note:** The gRPC API is unauthenticated and intended for deployment-internal use only. Bind it to localhost or an internal container network and firewall the port (the demo compose binds `127.0.0.1`). Anyone with network access to the port can publish messages, change the preferred peer set, or trigger dials.

### Metrics and health

The HTTP server on `metrics_port` exposes:

* `GET /metrics`: Prometheus metrics (connected peers, messages sent/received,
  event counters, memory usage).
* `GET /health`: liveness probe.

## Configuration

The service reads a YAML configuration file (`--config`, default `config.yml`).
All fields have defaults; an empty `p2p: {}` section is valid.

| Field | Default | Description |
| --- | --- | --- |
| `p2p.port` | `4025` | TCP port for P2P (libp2p) communication. |
| `p2p.grpc_port` | `4030` | Port of the gRPC control/pubsub API. |
| `p2p.metrics_port` | `4040` | Port of the HTTP metrics/health server. |
| `p2p.peers` | aleph.im bootstrap nodes | Bootstrap peers, multiaddr format with trailing `/p2p/<peer id>`. |
| `p2p.topics` | `[ALIVE, ALEPH-TEST]` | Gossipsub topics to subscribe to at startup. |
| `p2p.nb_api_workers` | `4` | Workers of the HTTP metrics server. 1 is typically sufficient as it only serves metrics and health checks. |
| `p2p.low_water` | `80` | Maintain at least this many connections (the maintenance loop dials known peers below it). |
| `p2p.high_water` | `160` | Disconnect non-protected peers above this many connections. |
| `p2p.per_subnet_cap` | `4` | Maximum connections per IPv4 /24 subnet for non-protected peers. |
| `p2p.max_protected_share` | `0.5` | Maximum share of `high_water` that preferred peers may occupy. |
| `p2p.peerstore_path` | `peerstore.json` | Path of the persisted peerstore file. |
| `p2p.maintenance_interval_secs` | `30` | Seconds between mesh maintenance passes. |
| `sentry.dsn` | unset | Sentry DSN; error reporting is disabled when unset. |
| `sentry.traces_sample_rate` | unset | Sentry traces sample rate. |

The peerstore file persists known peers and their dialable addresses across
restarts, so a restarted node can refill its connection table without relying
solely on the bootstrap peers.

### Migration from v1.x

RabbitMQ support (the `p2p-publish`/`p2p-subscribe` exchanges) and the HTTP
control endpoints (`/api/p2p/dial`, `/api/p2p/identify`) were removed in this
release. pyaleph releases supporting the v2.0 gRPC API use it instead. Old
configuration keys (`rabbitmq` section, `control_port`, `listen_port`, ...) are
ignored harmlessly, so existing configuration files keep working.

Prometheus scrape targets must be moved from the old `control_port` to
`metrics_port` (default 4040).

Operators who had customized `control_port` must now set `grpc_port` explicitly; the old key is ignored. It is also recommended to point `peerstore_path` at a mounted volume so that known peers survive container recreation (the default `peerstore.json` lands in the container filesystem and is lost on recreation).

## Building

The build requires a Rust toolchain and the protobuf compiler (`protoc`):

```shell
apt-get install protobuf-compiler  # or the equivalent for your distribution
cargo build --release
```

A Docker image can be built with [scripts/docker/build-docker-image.sh](scripts/docker/build-docker-image.sh),
and `docker compose up` in [scripts/demo](scripts/demo) starts a ready-to-use service.

## FAQ

### How can I create a private key?

The P2P service does not create a private key automatically.
You must create an RSA key in the PKCS8 DER binary format.
The easiest solution is to use `openssl`:

```shell
openssl genrsa 2048 | openssl pkcs8 -topk8 -inform PEM -outform DER -nocrypt -out node-secret.pkcs8.der
```
