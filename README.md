# Aleph.im P2P Service

A P2P service for aleph.im nodes.

## Purpose

Aleph.im nodes use P2P communication for a variety of purposes. This service enables multiple
processes written in different languages to interact with the P2P network.

## Features

* Dial peers on the P2P network
* Publish/subscribe to P2P topics using gossipsub
* Get information about the local P2P node, such as the peer ID.

## Usage

The P2P service features are exposed in the form of HTTP endpoints and RabbitMQ exchanges.
See [the demo directory](scripts/demo) for details on how to set up and configure the service.

### Pubsub

Users can publish on a P2P topic by posting a RabbitMQ message to the `p2p-publish`
exchange and setting the topic as routing key. The service will automatically forward
the message to the P2P network.

Subscribing works similarly. When configuring the node, add topics to subscribe to
in the `p2p.topics` configuration variable. The service will automatically subscribe
to these topics at startup and will forward messages to the `p2p-subscribe` exchange.
The service uses the following routing key: `<protocol>.<topic>.<peer_id>` where:

* the protocol is the source of the pubsub message. `p2p`is the only supported value at the moment 
  but this could change in the future, i.e. if adding support for IPFS pubsubs.
* the topic is the P2P topic from which the message originates.
* the peer ID is the ID of the node that published the message.

### Dialing

Users can dial other peers by posting to the `/api/p2p/dial` endpoint and specifying
the peer ID and the multiaddress of the node to contact in the request body.

### Requesting information about the local node

The `/api/p2p/identify` endpoint enables users to fetch the peer ID of the node.

## FAQ

### How can I create a private key?

The P2P service does not create a private key automatically.
You must create an RSA key in the PKCS8 DER binary format.
The easiest solution is to use `openssl`:

```shell
openssl genrsa 2048 | openssl pkcs8 -topk8 -inform PEM -outform DER -nocrypt -out node-secret.pkcs8.der
```
