use futures::StreamExt;
use lapin::message::Delivery;
use lapin::options::{
    BasicConsumeOptions, BasicPublishOptions, ExchangeDeclareOptions, QueueBindOptions,
    QueueDeclareOptions,
};
use lapin::publisher_confirm::PublisherConfirm;
use lapin::types::FieldTable;
use lapin::{BasicProperties, Channel, Consumer};
use lapin::{Connection, ConnectionProperties, ExchangeKind};

use crate::config::AppConfig;

/// A client for the exchanges we create in RabbitMQ. The client abstracts two exchanges:
/// 1. A queue where other processes on the node publish messages to be sent to the P2P network
///    (= the "pub" exchange/queue).
/// 2. An exchange where the P2P service will publish pubsub messages received over the P2P
///    network for the other processes on the node to consume (= the "sub" exchange).
pub struct RabbitMqClient {
    channel: Channel,
    pub_consumer: Consumer,
    pub pub_exchange: String,
    pub sub_exchange: String,
}

impl RabbitMqClient {
    pub async fn next(&mut self) -> Option<lapin::Result<Delivery>> {
        self.pub_consumer.next().await
    }

    pub async fn publish(
        &mut self,
        routing_key: &str,
        data: &[u8],
    ) -> lapin::Result<PublisherConfirm> {
        self.channel
            .basic_publish(
                &self.sub_exchange,
                routing_key,
                BasicPublishOptions::default(),
                data,
                BasicProperties::default(),
            )
            .await
    }
}

fn make_amqp_uri(app_config: &AppConfig) -> String {
    format!(
        "amqp://{}:{}@{}:{}",
        app_config.rabbitmq.username,
        app_config.rabbitmq.password,
        app_config.rabbitmq.host,
        app_config.rabbitmq.port.0
    )
}

pub async fn new(app_config: &AppConfig) -> Result<RabbitMqClient, Box<dyn std::error::Error>> {
    // Create exchange
    let addr = make_amqp_uri(app_config);

    let conn = Connection::connect(&addr, ConnectionProperties::default())
        .await
        .expect("RabbitMQ connection should be created");
    let channel = conn.create_channel().await.unwrap();

    let pub_exchange_name = &app_config.rabbitmq.pub_exchange;
    let sub_exchange_name = &app_config.rabbitmq.sub_exchange;

    let pub_queue_name = format!("{}-queue", pub_exchange_name);

    channel
        .exchange_declare(
            pub_exchange_name,
            ExchangeKind::Topic,
            ExchangeDeclareOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("should be able to declare pub exchange");

    let pub_queue = channel
        .queue_declare(
            &pub_queue_name,
            QueueDeclareOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("should be able to create pub queue");

    channel
        .queue_bind(
            pub_queue.name().as_str(),
            pub_exchange_name,
            "",
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("should be able to bind pub queue");

    let pub_consumer = channel
        .basic_consume(
            pub_queue.name().as_str(),
            "pub_consumer",
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("should be able to create pub consumer");

    channel
        .exchange_declare(
            sub_exchange_name,
            ExchangeKind::Topic,
            ExchangeDeclareOptions::default(),
            FieldTable::default(),
        )
        .await
        .expect("should be able to declare sub exchange");

    Ok(RabbitMqClient {
        channel,
        pub_consumer,
        pub_exchange: pub_exchange_name.to_owned(),
        sub_exchange: sub_exchange_name.to_owned(),
    })
}
