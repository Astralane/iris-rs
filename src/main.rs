#![warn(unused_crate_dependencies)]
use crate::broadcaster::MevProtectedBroadcaster;
use crate::chain_state::spawn_chain_state_updater;
use crate::dedup_and_retry::{DedupAndRetry, DedupPacketPayload};
use crate::http_middleware::HttpLoggingMiddleware;
use crate::otel_tracer::{
    get_subscriber_with_otpl, init_subscriber, init_subscriber_without_signoz,
};
use crate::rpc::IrisRpcServer;
use crate::rpc_server::IrisRpcServerImpl;
use crate::runtime::{build_runtime, TokioRtConfig};
use crossbeam_channel::Sender;
use figment::providers::Env;
use figment::Figment;
use jsonrpsee::server::{ServerBuilder, ServerConfig};
use metrics_exporter_prometheus::PrometheusBuilder;
use rustls::crypto::CryptoProvider;
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair};
use solana_tpu_client_next::node_address_service::LeaderTpuCacheServiceConfig;
use std::fmt::Debug;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

mod broadcaster;
mod chain_state;
mod dedup_and_retry;
mod gossip_service;
mod http_middleware;
mod otel_tracer;
mod quic_server;
mod rpc;
mod rpc_server;
mod runtime;
mod shield;
mod store;
mod tpu_next_client;
mod types;
mod vendor;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    rpc_url: String,
    ws_url: String,
    address: SocketAddr,
    identity_keypair_file: Option<String>,
    grpc_url: Option<String>,
    //The number of connections to be maintained by the scheduler.
    num_connections: usize,
    /// Runtime config for the TPU client (default: 2 threads, no pinning).
    /// Env: TPU_CLIENT_RT__NUM_THREADS, TPU_CLIENT_RT__CPUS
    tpu_client_rt: Option<TokioRtConfig>,
    //The size of the channel used to transmit transaction batches to the worker tasks.
    worker_channel_size: usize,
    //The maximum number of reconnection attempts allowed in case of connection failure.
    max_reconnect_attempts: usize,
    //The number of slots to look ahead during the leader estimation procedure.
    //Determines how far into the future leaders are estimated,
    //allowing connections to be established with those leaders in advance.
    leaders_fanout: u8,
    prometheus_addr: SocketAddr,
    tx_retry_interval_ms: u32,
    shield_policy_key: Option<String>,
    otpl_endpoint: Option<String>,
    /// Runtime config for the OTLP exporter (default: 1 thread, no pinning).
    /// Required when otpl_endpoint is set — tonic gRPC needs a Tokio context.
    /// Env: OTEL_RT__NUM_THREADS, OTEL_RT__CPUS
    otel_rt: Option<TokioRtConfig>,
    /// Runtime config for the JSON-RPC server (default: 4 threads, no pinning).
    /// Env: RPC_RT__NUM_THREADS, RPC_RT__CPUS
    rpc_rt: Option<TokioRtConfig>,
    gossip_keypair_file: Option<String>,
    gossip_port_range: Option<(u16, u16)>,
    /// Runtime config for the QUIC server (default: 2 threads, no pinning).
    /// Env: QUIC_RT__NUM_THREADS, QUIC_RT__CPUS
    quic_rt: Option<TokioRtConfig>,
    quic_bind_address: Option<SocketAddr>,
    /// Runtime config for the chain-state updater (default: 2 threads, no pinning).
    /// Env: CHAIN_STATE_RT__NUM_THREADS, CHAIN_STATE_RT__CPUS
    chain_state_rt: Option<TokioRtConfig>,
    // LAN like tuning if colocated
    is_colocated: bool,
}

fn main() -> anyhow::Result<()> {
    //for some reason ths is required to make rustls work
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install default crypto provider");

    dotenv::dotenv().ok();
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    //read config from env variables
    // `__` in an env var name is treated as a nested field separator, e.g.
    // TPU_CLIENT_RT__NUM_THREADS=4  →  config.tpu_client_rt.num_threads = 4
    let config: Config = Figment::new()
        .merge(Env::raw().split("__"))
        .extract()
        .expect("config not valid");

    init_tracing(&config, env_filter);
    info!("config: {:?}", config);

    let identity_keypair = config
        .identity_keypair_file
        .as_ref()
        .and_then(|file| read_keypair_file(file).ok())
        .expect("No identity keypair file");

    let shield_policy_key = config
        .shield_policy_key
        .map(|s| Pubkey::from_str(&s))
        .and_then(|p| p.ok())
        .expect("Failed to parse shield policy key");

    PrometheusBuilder::new()
        .with_http_listener(config.prometheus_addr)
        .install()
        .expect("failed to install recorder/exporter");

    let shutdown = Arc::new(AtomicBool::new(false));
    let cancel = CancellationToken::new();

    let _gossip_t = if let Some(port_range) = config.gossip_port_range {
        let gossip_keypair = config
            .gossip_keypair_file
            .as_ref()
            .and_then(|file| read_keypair_file(file).ok());
        Some(gossip_service::spawn_gossip_service(
            port_range,
            gossip_keypair,
            shutdown.clone(),
        ))
    } else {
        None
    };

    let rpc = Arc::new(RpcClient::new(config.rpc_url.to_owned()));
    let tpu_cache_config = LeaderTpuCacheServiceConfig {
        lookahead_leaders: config.leaders_fanout + 1,
        refresh_nodes_info_every: Duration::from_secs(5 * 60),
        max_consecutive_failures: 10,
    };
    info!("leader updater created");
    let (mev_protected_broadcaster, _broadcaster_jh) =
        MevProtectedBroadcaster::run(shield_policy_key, rpc.clone(), cancel.clone());

    let tpu_client_rt = build_runtime(
        "tpu_client_next_rt",
        &config.tpu_client_rt.unwrap_or(TokioRtConfig::threads(2)),
    );

    let (tpu_sender, client) = tpu_next_client::spawn_tpu_client_next(
        mev_protected_broadcaster,
        tpu_client_rt.handle(),
        rpc,
        config.ws_url.clone(),
        tpu_cache_config,
        config.leaders_fanout as usize,
        config.num_connections as usize,
        identity_keypair,
        config.worker_channel_size,
        config.max_reconnect_attempts,
        cancel.clone(),
    )
    .expect("cannot create tpu client next");

    let (chain_state, chain_update_t_handle) = spawn_chain_state_updater(
        800, // around 4 mins
        config.ws_url,
        config.grpc_url,
        config.chain_state_rt.unwrap_or(TokioRtConfig::threads(2)),
    );

    let (dedup_sender, dedup_receiver) = crossbeam_channel::bounded(2 * 1024);

    let dedup_and_retry_t = DedupAndRetry::new(
        tpu_sender,
        dedup_receiver,
        Arc::new(chain_state),
        cancel.clone(),
    );

    let maybe_quic_server =
        if let Some((quic_rt_config, bind_addr)) = config.quic_rt.zip(config.quic_bind_address) {
            info!("running with quic server at port {bind_addr:?}");
            Some(quic_server::spawn_new(
                "iris-quic-server",
                bind_addr,
                quic_rt_config,
                &Keypair::new(),
                dedup_sender.clone(),
                config.is_colocated,
                cancel.clone(),
            )?)
        } else {
            info!("running without quic server");
            None
        };

    info!("server starting in {:?}", config.address);
    let _rpc_t = spawn_json_rpc_server(
        config.rpc_rt.unwrap_or(TokioRtConfig::threads(4)),
        dedup_sender,
        config.address,
        cancel.clone(),
    );
    // Block until chain state disconnects — it is a critical component and its exit
    // signals the whole process to shut down. The service manager will restart us.
    info!("waiting for chain state to exit");
    chain_update_t_handle
        .join()
        .expect("chain update join failed");
    tracing::error!("chain state exited — initiating shutdown");
    cancel.cancel();
    // Also signal the gossip service which uses the AtomicBool directly.
    shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(quic_t) = maybe_quic_server {
        quic_t.join().unwrap();
    }
    tpu_client_rt
        .block_on(client.shutdown())
        .expect("cannot join tpu client");
    dedup_and_retry_t
        .join()
        .expect("dedup_and_retry thread join failed");
    Ok(())
}

fn init_tracing(config: &Config, env_filter: EnvFilter) {
    match &config.otpl_endpoint {
        Some(endpoint) => {
            // tonic gRPC batch exporters require a Tokio runtime context at construction time.
            let otel_rt = build_runtime(
                "otel-rt",
                &config.otel_rt.clone().unwrap_or(TokioRtConfig::threads(1)),
            );
            let _guard = otel_rt.enter();
            let subscriber = get_subscriber_with_otpl(
                env_filter,
                endpoint.to_string(),
                config.address.port(),
                std::io::stdout,
            );
            // Leak the runtime so it stays alive for the duration of the process —
            // the batch exporters need it running to flush telemetry on shutdown.
            std::mem::forget(otel_rt);
            init_subscriber(subscriber);
        }
        None => init_subscriber_without_signoz(std::io::stdout),
    }
}

fn spawn_json_rpc_server(
    rt_config: TokioRtConfig,
    dedup_sender: Sender<DedupPacketPayload>,
    bind_address: SocketAddr,
    cancel: CancellationToken,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rpc-t".to_string())
        .spawn(move || {
            let rt = build_runtime("json_rpc_next_rt", &rt_config);
            let _guard = rt.enter();
            let task = async move {
                let json_rpc = IrisRpcServerImpl::new(dedup_sender);
                let server_config = ServerConfig::builder()
                    .max_request_body_size(4 * 1024) // 4kb
                    .max_connections(10_000)
                    .set_keep_alive(Some(Duration::from_secs(60)))
                    .build();

                let http_middleware = tower::ServiceBuilder::new().layer_fn(HttpLoggingMiddleware);
                let server = ServerBuilder::with_config(server_config)
                    .set_http_middleware(http_middleware)
                    .build(bind_address)
                    .await
                    .unwrap();
                let handle = server.start(json_rpc.into_rpc());
                tokio::select! {
                    _ = handle.stopped() => {}
                    _ = cancel.cancelled() => {}
                }
            };
            rt.block_on(task)
        })
        .unwrap()
}
