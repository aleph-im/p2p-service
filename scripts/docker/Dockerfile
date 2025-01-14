FROM rust:1.75
WORKDIR /usr/src/p2p-service
RUN cargo install ry  # YAML reading util, used to start the service
RUN apt-get update && apt-get install -y protobuf-compiler
COPY . .
RUN cargo install --path .

COPY ./scripts/docker/run-p2p-service.sh ./scripts/docker/wait-for-it.sh /opt/p2p-service/
ENTRYPOINT ["bash", "/opt/p2p-service/run-p2p-service.sh"]
