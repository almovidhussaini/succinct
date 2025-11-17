//! Extremely simple bidder.
//!
//! Policy:
//! - Large orders bid BID_LARGE_AMOUNT (default 0.45 PROVE).
//! - Small orders bid BID_SMALL_AMOUNT (default 0.01 PROVE) when `BID_ENABLE_SMALL=true`.
//! - If small orders are disabled, they are skipped entirely.

use std::{env, path::Path, time::Duration};

use crate::metrics::BidderMetrics;
use alloy::primitives::{Address, U256};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{anyhow, Result};
use tokio::time::sleep;
use tonic::{transport::Channel, Status};
use tracing::{debug, error, info, warn};

use spn_network_types::{
    prover_network_client::ProverNetworkClient, BidHistory, BidRequest, BidRequestBody,
    FulfillmentStatus, GetFilteredBidHistoryRequest, GetNonceRequest, GetOwnerRequest,
    MessageFormat, Signable, TransactionVariant,
};
use spn_utils::time_now;

pub mod config;
pub mod grpc;
pub mod metrics;

/// Parse PROVE amount from string into 9-decimal base units.
///
/// Example:
/// - "0.45" → 450_000_000
/// - "0.01" → 10_000_000
fn parse_prove_amount(s: &str, default: u64) -> u64 {
    let t = s.trim();
    if t.is_empty() {
        return default;
    }
    if let Ok(v) = t.parse::<f64>() {
        return (v * 1e9).round() as u64;
    }
    default
}

#[derive(Clone)]
pub struct Bidder {
    network: ProverNetworkClient<Channel>,
    version: String,
    signer: PrivateKeySigner,
    metrics: BidderMetrics,
    domain_bytes: Vec<u8>,

    refresh_interval_secs: u64,
    request_limit: u32,

    small_max_gas_inclusive: u64,
    enable_small_orders: bool,
    history_probe_enabled: bool,
    history_probe_limit: u32,

    fixed_bid_amount: U256,
    small_bid_amount: U256,
}

impl Bidder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        network: ProverNetworkClient<Channel>,
        version: String,
        signer: PrivateKeySigner,
        metrics: BidderMetrics,
        domain_bytes: Vec<u8>,

        // The original 8 legacy args (unused)
        _throughput_mgas: f64,
        _max_concurrent_proofs: u32,
        _bid_amount: U256,
        _buffer_sec: u64,
        _groth16_buffer_sec: u64,
        _plonk_buffer_sec: u64,
        _groth16_enabled: bool,
        _plonk_enabled: bool,
    ) -> Self {
        load_env_files();

        const D_REFRESH_INTERVAL: u64 = 3;
        const D_REQUEST_LIMIT: u32 = 100;
        const D_SMALL_MAX_GAS: u64 = 8_000_000_000;

        // Defaults
        const DEFAULT_SMALL: u64 = 10_000_000; // 0.01 PROVE
        const DEFAULT_LARGE: u64 = 450_000_000; // 0.45 PROVE

        let refresh_interval_secs = env_u64("BID_REFRESH_INTERVAL_SEC", D_REFRESH_INTERVAL);
        let request_limit = env_u32("BID_REQUEST_LIMIT", D_REQUEST_LIMIT);
        let small_max_gas_inclusive = env_u64("BID_SMALL_MAX_GAS", D_SMALL_MAX_GAS);
        let enable_small_orders = env_bool("BID_ENABLE_SMALL", false);
        let history_probe_enabled = settings.debug_probe_history;
        let history_probe_limit = settings.debug_history_limit;

        let small_raw = env::var("BID_SMALL_AMOUNT").unwrap_or_else(|_| "0.01".into());
        let large_raw = env::var("BID_LARGE_AMOUNT").unwrap_or_else(|_| "0.45".into());

        let small_bid_units = parse_prove_amount(&small_raw, DEFAULT_SMALL);
        let large_bid_units = parse_prove_amount(&large_raw, DEFAULT_LARGE);

        let small_bid_amount = U256::from(small_bid_units);
        let fixed_bid_amount = U256::from(large_bid_units);

        info!(
            "init: large_bid={}, small_bid={}, small_max_gas={}, enable_small={}, refresh_interval={}, request_limit={}, history_probe_enabled={}, history_probe_limit={}",
            fixed_bid_amount,
            small_bid_amount,
            small_max_gas_inclusive,
            enable_small_orders,
            refresh_interval_secs,
            request_limit,
            history_probe_enabled,
            history_probe_limit
        );

        Self {
            network,
            version,
            signer,
            metrics,
            domain_bytes,
            refresh_interval_secs,
            request_limit,
            small_max_gas_inclusive,
            enable_small_orders,
            history_probe_enabled,
            history_probe_limit,
            fixed_bid_amount,
            small_bid_amount,
        }
    }

    pub async fn run(mut self) -> Result<()> {
        info!("starting bidder");

        let prover_bytes = self
            .network
            .clone()
            .get_owner(GetOwnerRequest {
                address: self.signer.address().to_vec(),
            })
            .await?
            .into_inner()
            .owner;

        let prover = Address::from_slice(&prover_bytes);

        loop {
            if let Err(e) = self.bid_requests(prover).await {
                error!("bidding error: {:?}", e);
                self.metrics.main_loop_errors.increment(1);
            }
            sleep(Duration::from_secs(self.refresh_interval_secs)).await;
        }
    }

    async fn bid_requests(&mut self, prover: Address) -> Result<()> {
        let req = spn_network_types::GetFilteredProofRequestsRequest {
            version: Some(self.version.clone()),
            fulfillment_status: Some(FulfillmentStatus::Requested.into()),
            execution_status: None,
            execute_fail_cause: None,
            minimum_deadline: Some(time_now()),
            vk_hash: None,
            requester: None,
            fulfiller: None,
            limit: Some(self.request_limit),
            page: None,
            from: None,
            to: None,
            mode: None,
            not_bid_by: Some(prover.to_vec()),
            settlement_status: None,
        };

        let resp = self
            .network
            .clone()
            .get_filtered_proof_requests(req)
            .await?;
        let mut requests = resp.into_inner().requests;

        if requests.is_empty() {
            self.metrics.biddable_requests.set(0.0);
            info!("no biddable requests");
            return Ok(());
        }

        self.metrics.biddable_requests.set(requests.len() as f64);
        requests.sort_by_key(|r| (r.gas_limit, r.deadline));

        let mut bids_submitted = 0u32;
        let prover_bytes = prover.to_vec();

        for request in requests {
            let request_id_hex = hex::encode(&request.request_id);
            let classification = if request.gas_limit <= self.small_max_gas_inclusive {
                BidClassification::Small
            } else {
                BidClassification::Large
            };
            let bid_amount = match classification {
                BidClassification::Small => self.small_bid_amount,
                BidClassification::Large => self.fixed_bid_amount,
            };

            self.metrics.requests_evaluated.increment(1);
            info!(
                request.id = %request_id_hex,
                request.gas_limit = request.gas_limit,
                request.deadline = request.deadline,
                request.classification = classification.as_str(),
                request.bid_amount = %bid_amount,
                "evaluating proof request"
            );

            if request.request_id.len() != 32 {
                self.metrics.requests_skipped_invalid.increment(1);
                warn!(
                    request.id = %request_id_hex,
                    reason = "invalid_request_id_length",
                    request.request_id_len = request.request_id.len(),
                    "skipping request due to malformed identifier"
                );
                continue;
            }

            let history_insight = if self.history_probe_enabled {
                match self
                    .fetch_bid_history_insight(
                        &request.request_id,
                        &request_id_hex,
                        prover_bytes.as_slice(),
                    )
                    .await
                {
                    Ok(insight) => {
                        self.metrics
                            .bid_history_entries_last
                            .set(insight.total_entries() as f64);
                        if insight.we_are_leading() {
                            self.metrics.requests_already_leading.increment(1);
                        }
                        if insight.detected_outbid() {
                            self.metrics.requests_detected_outbid.increment(1);
                        }
                        info!(
                            request.id = %request_id_hex,
                            history.total_entries = insight.total_entries(),
                            history.highest_amount = ?insight.highest_amount_string(),
                            history.highest_bidder = ?insight.highest_bidder_short(),
                            history.our_latest_amount = ?insight.our_latest_amount_string(),
                            history.we_are_leading = insight.we_are_leading(),
                            history.detected_outbid = insight.detected_outbid(),
                            "bid history snapshot"
                        );
                        debug!(
                            request.id = %request_id_hex,
                            ?insight.entries,
                            "bid history entries"
                        );
                        Some(insight)
                    }
                    Err(err) => {
                        warn!(
                            request.id = %request_id_hex,
                            error = ?err,
                            "bid history probe failed"
                        );
                        None
                    }
                }
            } else {
                None
            };

            if classification.is_small() && !self.enable_small_orders {
                self.metrics.requests_skipped_small_disabled.increment(1);
                info!(
                    request.id = %request_id_hex,
                    request.gas_limit = request.gas_limit,
                    reason = "small_bids_disabled",
                    small_threshold = self.small_max_gas_inclusive,
                    "skipping request due to configuration"
                );
                continue;
            }

            self.metrics.bid_attempts.increment(1);
            match self
                .bid_with_amount(prover, &request.request_id, bid_amount)
                .await
            {
                Ok(_) => {
                    info!(
                        request.id = %request_id_hex,
                        request.classification = classification.as_str(),
                        request.bid_amount = %bid_amount,
                        "bid submitted"
                    );
                    bids_submitted += 1;
                    self.metrics.requests_bid.increment(1);
                    self.metrics.total_requests_processed.increment(1);
                }
                Err(err) => {
                    self.metrics.request_bid_failures.increment(1);
                    self.log_bid_failure(
                        &request_id_hex,
                        request.gas_limit,
                        request.deadline,
                        classification,
                        bid_amount,
                        history_insight.as_ref(),
                        &err,
                    );
                }
            }
        }

        info!(
            "cycle complete: {} bids (large={}, small={})",
            bids_submitted, self.fixed_bid_amount, self.small_bid_amount
        );

        Ok(())
    }

    async fn bid_with_amount(
        &self,
        prover: Address,
        request_id: &[u8],
        amount: U256,
    ) -> std::result::Result<(), BidAttemptError> {
        let nonce = self
            .network
            .clone()
            .get_nonce(GetNonceRequest {
                address: self.signer.address().to_vec(),
            })
            .await
            .map_err(BidAttemptError::Rpc)?
            .into_inner()
            .nonce;

        let body = BidRequestBody {
            nonce,
            request_id: request_id.to_vec(),
            amount: amount.to_string(),
            domain: self.domain_bytes.clone(),
            prover: prover.to_vec(),
            variant: TransactionVariant::BidVariant.into(),
        };

        let req = BidRequest {
            format: MessageFormat::Binary.into(),
            signature: body.sign(&self.signer).into(),
            body: Some(body),
        };

        self.network
            .clone()
            .bid(req)
            .await
            .map(|_| ())
            .map_err(BidAttemptError::Rpc)
    }

    async fn fetch_bid_history_insight(
        &self,
        request_id: &[u8],
        request_id_hex: &str,
        prover_bytes: &[u8],
    ) -> Result<BidHistoryInsight> {
        self.metrics.bid_history_queries.increment(1);
        let request = GetFilteredBidHistoryRequest {
            request_id: request_id.to_vec(),
            limit: Some(self.history_probe_limit),
            page: Some(0),
            winner_first: Some(true),
        };

        let response = self
            .network
            .clone()
            .get_filtered_bid_history(request)
            .await
            .map_err(|status| {
                self.metrics.bid_history_query_failures.increment(1);
                warn!(
                    request.id = %request_id_hex,
                    rpc.code = ?status.code(),
                    rpc.message = status.message(),
                    "get_filtered_bid_history RPC failed"
                );
                anyhow!(status)
            })?;

        let bids = response.into_inner().bids;
        let mut entries = Vec::with_capacity(bids.len());
        for bid in bids {
            match HistoryEntrySummary::from_bid_history(bid, prover_bytes) {
                Ok(entry) => entries.push(entry),
                Err(err) => {
                    warn!(
                        request.id = %request_id_hex,
                        error = ?err,
                        "skipping malformed bid history entry"
                    );
                }
            }
        }

        Ok(BidHistoryInsight { entries })
    }

    fn log_bid_failure(
        &self,
        request_id_hex: &str,
        gas_limit: u64,
        deadline: u64,
        classification: BidClassification,
        bid_amount: U256,
        history: Option<&BidHistoryInsight>,
        error: &BidAttemptError,
    ) {
        let history_total = history.map(|h| h.total_entries()).unwrap_or(0);
        let history_we_are_leading = history.map(|h| h.we_are_leading()).unwrap_or(false);
        let history_detected_outbid = history.map(|h| h.detected_outbid()).unwrap_or(false);
        let history_highest_amount = history.and_then(|h| h.highest_amount_string());
        let history_our_amount = history.and_then(|h| h.our_latest_amount_string());

        match error {
            BidAttemptError::Rpc(status) => error!(
                request.id = %request_id_hex,
                request.gas_limit = gas_limit,
                request.deadline = deadline,
                request.classification = classification.as_str(),
                request.bid_amount = %bid_amount,
                env.refresh_interval = self.refresh_interval_secs,
                env.request_limit = self.request_limit,
                env.small_max_gas = self.small_max_gas_inclusive,
                env.enable_small = self.enable_small_orders,
                history.total_entries = history_total,
                history.we_are_leading = history_we_are_leading,
                history.detected_outbid = history_detected_outbid,
                history.highest_amount = ?history_highest_amount,
                history.our_latest_amount = ?history_our_amount,
                rpc.code = ?status.code(),
                rpc.message = status.message(),
                "bid RPC failed"
            ),
        }
    }
}

// --- Helpers -------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum BidClassification {
    Small,
    Large,
}

impl BidClassification {
    fn as_str(&self) -> &'static str {
        match self {
            BidClassification::Small => "small",
            BidClassification::Large => "large",
        }
    }

    fn is_small(&self) -> bool {
        matches!(self, BidClassification::Small)
    }
}

#[derive(Debug, Clone)]
struct HistoryEntrySummary {
    bidder_hex: String,
    amount: U256,
    created_at: u64,
    bidder_is_us: bool,
}

impl HistoryEntrySummary {
    fn from_bid_history(bid: BidHistory, prover_bytes: &[u8]) -> Result<Self> {
        if bid.bidder.is_empty() {
            return Err(anyhow!("bid history entry missing bidder bytes"));
        }
        let bidder_hex = hex::encode(&bid.bidder);
        let amount = bid
            .amount
            .parse::<U256>()
            .map_err(|e| anyhow!("invalid bid amount '{}': {}", bid.amount, e))?;

        Ok(Self {
            bidder_hex,
            amount,
            created_at: bid.created_at,
            bidder_is_us: bid.bidder.as_slice() == prover_bytes,
        })
    }
}

#[derive(Debug, Clone)]
struct BidHistoryInsight {
    entries: Vec<HistoryEntrySummary>,
}

impl BidHistoryInsight {
    fn total_entries(&self) -> usize {
        self.entries.len()
    }

    fn highest(&self) -> Option<&HistoryEntrySummary> {
        self.entries.first()
    }

    fn our_latest(&self) -> Option<&HistoryEntrySummary> {
        self.entries.iter().find(|entry| entry.bidder_is_us)
    }

    fn we_are_leading(&self) -> bool {
        self.highest()
            .map(|entry| entry.bidder_is_us)
            .unwrap_or(false)
    }

    fn detected_outbid(&self) -> bool {
        self.our_latest().is_some() && !self.we_are_leading()
    }

    fn highest_amount_string(&self) -> Option<String> {
        self.highest().map(|entry| entry.amount.to_string())
    }

    fn highest_bidder_short(&self) -> Option<String> {
        self.highest().map(|entry| short_hex(&entry.bidder_hex))
    }

    fn our_latest_amount_string(&self) -> Option<String> {
        self.our_latest().map(|entry| entry.amount.to_string())
    }
}

#[derive(Debug)]
enum BidAttemptError {
    Rpc(Status),
}

fn short_hex(input: &str) -> String {
    if input.len() <= 12 {
        input.to_string()
    } else {
        format!("{}...{}", &input[..6], &input[input.len() - 4..])
    }
}

fn load_env_files() {
    if let Ok(path) = env::var("BIDDER_ENV_FILE") {
        try_load_env_file(&path);
    }
    let candidates = [
        ".env",
        ".env.local",
        ".env_bidder",
        "infra/.env_bidder",
        "/etc/bidder.env",
        "/etc/default/bidder",
        "/etc/sysconfig/bidder",
    ];
    for path in candidates {
        try_load_env_file(path);
    }
}

fn try_load_env_file(path: &str) {
    if Path::new(path).exists() {
        match dotenv::from_filename(path) {
            Ok(_) => info!("loaded env file: {}", path),
            Err(e) => warn!("could not load {}: {:?}", path, e),
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_u32(name: &str, default: u32) -> u32 {
    env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    env::var(name)
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}
