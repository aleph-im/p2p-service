FROM rust:1.63 as builder
WORKDIR /usr/src/p2p-service
RUN apt-get update && apt-get install -y protobuf-compiler
COPY . .
RUN cargo install --path .

FROM debian:buster-slim
COPY --from=builder /usr/local/cargo/bin/aleph-p2p-service /usr/local/bin/aleph-p2p-service
ENTRYPOINT ["aleph-p2p-service"]
