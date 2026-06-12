FROM rust:1.88
WORKDIR /usr/src/p2p-service
RUN apt-get update && apt-get install -y protobuf-compiler
COPY . .
RUN cargo install --path . --locked

COPY ./scripts/docker/run-p2p-service.sh /opt/p2p-service/
ENTRYPOINT ["bash", "/opt/p2p-service/run-p2p-service.sh"]
