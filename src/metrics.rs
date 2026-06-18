use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::sync::Arc;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct EventLabel {
    pub event_type: String,
}

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<prometheus_client::registry::Registry>,

    // P2P metrics
    pub connected_peers: Gauge,
    pub total_messages_sent: Counter,
    pub total_messages_received: Counter,
    pub p2p_events: prometheus_client::metrics::family::Family<EventLabel, Counter>,

    // Memory metrics
    pub memory_usage_bytes: Gauge,

    // HTTP metrics
    pub http_requests_total: Counter,

    // Fetch protocol (provider side)
    pub fetch_served_total: Counter,
    pub fetch_serve_errors_total: Counter,
    pub fetch_bytes_served_total: Counter,
    // Fetch protocol (requester side)
    pub fetch_requests_total: Counter,
    pub fetch_request_errors_total: Counter,
    pub fetch_bytes_fetched_total: Counter,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let connected_peers = Gauge::default();
        registry.register(
            "p2p_connected_peers",
            "Number of currently connected peers",
            connected_peers.clone(),
        );

        let total_messages_sent = Counter::default();
        registry.register(
            "p2p_messages_sent_total",
            "Total number of P2P messages sent",
            total_messages_sent.clone(),
        );

        let total_messages_received = Counter::default();
        registry.register(
            "p2p_messages_received_total",
            "Total number of P2P messages received",
            total_messages_received.clone(),
        );

        let p2p_events = prometheus_client::metrics::family::Family::default();
        registry.register(
            "p2p_events_total",
            "Total number of P2P events by type",
            p2p_events.clone(),
        );

        let memory_usage_bytes = Gauge::default();
        registry.register(
            "process_memory_usage_bytes",
            "Current memory usage in bytes",
            memory_usage_bytes.clone(),
        );

        let http_requests_total = Counter::default();
        registry.register(
            "http_requests_total",
            "Total number of HTTP requests",
            http_requests_total.clone(),
        );

        let fetch_served_total = Counter::default();
        registry.register(
            "fetch_served_total",
            "Inbound fetch requests answered with content",
            fetch_served_total.clone(),
        );
        let fetch_serve_errors_total = Counter::default();
        registry.register(
            "fetch_serve_errors_total",
            "Inbound fetch requests rejected or failed (not found, over limit, backend error)",
            fetch_serve_errors_total.clone(),
        );
        let fetch_bytes_served_total = Counter::default();
        registry.register(
            "fetch_bytes_served_total",
            "Content bytes served to remote peers over the fetch protocol",
            fetch_bytes_served_total.clone(),
        );
        let fetch_requests_total = Counter::default();
        registry.register(
            "fetch_requests_total",
            "Outbound Fetch RPCs started",
            fetch_requests_total.clone(),
        );
        let fetch_request_errors_total = Counter::default();
        registry.register(
            "fetch_request_errors_total",
            "Outbound Fetch RPCs that failed (not found, deadline, aborted)",
            fetch_request_errors_total.clone(),
        );
        let fetch_bytes_fetched_total = Counter::default();
        registry.register(
            "fetch_bytes_fetched_total",
            "Content bytes fetched from remote peers over the fetch protocol",
            fetch_bytes_fetched_total.clone(),
        );

        Self {
            registry: Arc::new(registry),
            connected_peers,
            total_messages_sent,
            total_messages_received,
            p2p_events,
            memory_usage_bytes,
            http_requests_total,
            fetch_served_total,
            fetch_serve_errors_total,
            fetch_bytes_served_total,
            fetch_requests_total,
            fetch_request_errors_total,
            fetch_bytes_fetched_total,
        }
    }

    pub fn increment_event(&self, event_type: &str) {
        self.p2p_events
            .get_or_create(&EventLabel {
                event_type: event_type.to_string(),
            })
            .inc();
    }

    pub fn update_memory_usage(&self) {
        if let Ok(usage) = get_memory_usage() {
            self.memory_usage_bytes.set(usage as i64);
        }
    }
}

fn get_memory_usage() -> Result<u64, std::io::Error> {
    let contents = std::fs::read_to_string("/proc/self/status")?;
    for line in contents.lines() {
        if line.starts_with("VmRSS:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(kb) = parts[1].parse::<u64>() {
                    return Ok(kb * 1024); // Convert KB to bytes
                }
            }
        }
    }
    Ok(0)
}
