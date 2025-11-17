use spn_metrics::{
    metrics,
    metrics::{Counter, Gauge},
    Metrics,
};

#[derive(Metrics, Clone)]
#[metrics(scope = "bidder")]
pub struct BidderMetrics {
    /// The number of proof requests that are currently biddable.
    pub biddable_requests: Gauge,

    /// The number of proof requests inspected this cycle.
    pub requests_evaluated: Counter,

    /// The number of proof requests bid on successfully.
    pub requests_bid: Counter,

    /// The number of proof request bid attempts that failed.
    pub request_bid_failures: Counter,

    /// The number of requests skipped because they violate a local policy.
    pub requests_skipped_small_disabled: Counter,

    /// The number of requests skipped due to malformed or missing metadata.
    pub requests_skipped_invalid: Counter,

    /// The number of times we moved forward with a bid after evaluation.
    pub bid_attempts: Counter,

    /// The total number of proof requests processed (bid on).
    pub total_requests_processed: Counter,

    /// The number of errors encountered during the main loop.
    pub main_loop_errors: Counter,

    /// The number of bid history RPC probes that were performed.
    pub bid_history_queries: Counter,

    /// The number of bid history RPC probes that failed.
    pub bid_history_query_failures: Counter,

    /// The size of the bid history returned by the most recent probe.
    pub bid_history_entries_last: Gauge,

    /// Requests where diagnostics revealed we were already winning.
    pub requests_already_leading: Counter,

    /// Requests where diagnostics revealed we were previously outbid.
    pub requests_detected_outbid: Counter,
}
