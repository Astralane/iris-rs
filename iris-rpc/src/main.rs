use figment::providers::Env;
use figment::Figment;
use metrics_exporter_prometheus::PrometheusBuilder;
use rustls::crypto::CryptoProvider;
use serde::{Deserialize, Serialize};
use smart_rpc_client::rpc_provider::SmartRpcClientProvider;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair};
use std::fmt::Debug;
use std::net::{SocketAddr, UdpSocket};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;

use iris_rpc::chain_state::ChainStateWsClient;
use iris_rpc::otel_tracer::init_tracing;
use iris_rpc::*;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    rpc_urls: Vec<String>,
    ws_urls: Vec<String>,
    address: SocketAddr,
    quic_bind_address: SocketAddr,
    quic_server_threads: Option<usize>,
    identity_keypair_file: Option<String>,
    grpc_url: Option<String>,
    tx_max_retries: u32,
    rpc_num_threads: Option<usize>,
    tpu_client_num_threads: Option<usize>,
    //The number of connections to be maintained by the scheduler.
    num_connections: usize,
    //Whether to skip checking the transaction blockhash expiration.
    skip_check_transaction_age: bool,
    //The size of the channel used to transmit transaction batches to the worker tasks.
    worker_channel_size: usize,
    //The maximum number of reconnection attempts allowed in case of connection failure.
    max_reconnect_attempts: usize,
    //The number of slots to look ahead during the leader estimation procedure.
    //Determines how far into the future leaders are estimated,
    //allowing connections to be established with those leaders in advance.
    leaders_fanout: u64,
    prometheus_addr: SocketAddr,
    metrics_update_interval_secs: Option<u64>,
    tx_retry_interval_seconds: u32,
    shield_policy_key: String,
    otpl_endpoint: Option<String>,
}

const DEFAULT_RPC_THREADS: usize = 2;
const DEFAULT_QUIC_THREADS: usize = 2;
const DEFAULT_TPU_CLIENT_THREADS: usize = 4;
const SIGNATURE_RETAIN_SLOTS: u64 = 800; // around 4 mins

fn main() -> anyhow::Result<()> {
    //for some reason ths is required to make rustls work
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install default crypto provider");

    dotenv::dotenv().ok();
    let config: Config = Figment::new().merge(Env::raw()).extract()?;

    let _guard = init_tracing(
        config.otpl_endpoint.clone(),
        config.address.port(),
        std::io::stdout,
    );

    let _num_cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);

    let identity_keypair = config
        .identity_keypair_file
        .as_ref()
        .map(|file| read_keypair_file(file).expect("Failed to read identity keypair file"))
        .unwrap_or(Keypair::new());

    let _metrics = PrometheusBuilder::new()
        .with_http_listener(config.prometheus_addr)
        .install()
        .expect("failed to install recorder/exporter");

    let rpc_provider = Arc::new(SmartRpcClientProvider::new(
        &config.rpc_urls,
        Duration::from_secs(5),
        Some(CommitmentConfig::confirmed()),
    ));

    info!("config: {:?}", config);
    let cancel = CancellationToken::new();
    let txn_store = Arc::new(store::TransactionStoreImpl::new());

    let (chain_state, chain_state_hdl) = ChainStateWsClient::spawn_new(
        cancel.clone(),
        SIGNATURE_RETAIN_SLOTS,
        config.ws_urls.clone(),
        config.grpc_url,
    );

    let blocklist_provider = tpu_client::shield::YellowstoneShieldProvider::new(
        Pubkey::from_str(&config.shield_policy_key)?,
        rpc_provider.clone(),
    );

    let (tx_client, tpu_next_handle) =
        tpu_client::tpu_next_client::TpuClientNextSender::spawn_client(
            config
                .tpu_client_num_threads
                .unwrap_or(DEFAULT_TPU_CLIENT_THREADS),
            rpc_provider,
            config.ws_urls,
            config.leaders_fanout as usize,
            &identity_keypair,
            config.metrics_update_interval_secs.unwrap_or(10),
            config.worker_channel_size,
            config.max_reconnect_attempts,
            blocklist_provider,
            cancel.clone(),
        );

    let tx_client = Arc::new(tx_client);

    let quic_handle = quic_forwarder::QuicTxForwarder::spawn_new(
        UdpSocket::bind(config.quic_bind_address)?,
        tx_client.clone(),
        &identity_keypair,
        cancel.clone(),
        config.quic_server_threads.unwrap_or(DEFAULT_QUIC_THREADS),
    );

    let rpc_handle = rpc_server::spawn_jsonrpc_server(
        config.address,
        config.rpc_num_threads.unwrap_or(DEFAULT_RPC_THREADS),
        tx_client,
        txn_store,
        Arc::new(chain_state),
        Duration::from_secs(config.tx_retry_interval_seconds as u64),
        cancel.clone(),
        config.tx_max_retries,
    );

    chain_state_hdl.join().unwrap();
    tpu_next_handle.join().unwrap();
    rpc_handle.join().unwrap();
    quic_handle.join().unwrap();
    Ok(())
}
