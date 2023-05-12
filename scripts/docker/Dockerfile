FROM rust:1.68 as builder
WORKDIR /usr/src/p2p-service
RUN cargo install ry  # YAML reading util, used to start the service
RUN apt-get update && apt-get install -y protobuf-compiler
COPY . .
RUN cargo install --path .

FROM debian:buster-slim
COPY ./scripts/docker/run-p2p-service.sh ./scripts/docker/wait-for-it.sh /opt/p2p-service/
COPY --from=builder /usr/local/cargo/bin/ry /usr/local/cargo/bin/aleph-p2p-service /usr/local/bin/
ENTRYPOINT ["bash", "/opt/p2p-service/run-p2p-service.sh"]
