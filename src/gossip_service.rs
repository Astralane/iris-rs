use solana_gossip::cluster_info::{
    ClusterInfo, NodeConfig, DEFAULT_CONTACT_DEBUG_INTERVAL_MILLIS,
    DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS,
};
use solana_gossip::contact_info::ContactInfo;
use solana_gossip::gossip_service::GossipService;
use solana_gossip::node::Node;
use solana_net_utils::multihomed_sockets::BindIpAddrs;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::timing::timestamp;
use solana_streamer::socket::SocketAddrSpace;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tracing::info;

const ENTRYPOINTS: [&str; 5] = [
    "entrypoint.mainnet-beta.solana.com:8001",
    "entrypoint2.mainnet-beta.solana.com:8001",
    "entrypoint3.mainnet-beta.solana.com:8001",
    "entrypoint4.mainnet-beta.solana.com:8001",
    "entrypoint5.mainnet-beta.solana.com:8001",
];

pub fn run_gossip_service(
    port_range: (u16, u16),
    gossip_keypair: Option<Keypair>,
    exit: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let gossip_hdl = std::thread::Builder::new()
        .name("iris-gossip-t".to_string())
        .spawn(move || {
            let gossip_service = make_gossip_service(port_range, gossip_keypair, exit);
            gossip_service.join().expect("gossip service handle");
        })
        .expect("failed to spawn gossip service thread");

    tokio::spawn(async move {
        while !gossip_hdl.is_finished() {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    })
}

fn make_gossip_service(
    port_range: (u16, u16),
    gossip_keypair: Option<Keypair>,
    exit: Arc<AtomicBool>,
) -> GossipService {
    let gossip_keypair = Arc::new(gossip_keypair.unwrap_or(Keypair::new()));
    let bind_address = solana_net_utils::parse_host("0.0.0.0").expect("invalid bind address");

    let cluster_endpoints_sockets = ENTRYPOINTS
        .iter()
        .flat_map(|entry| entry.parse().ok())
        .collect::<Vec<SocketAddr>>();

    let cluster_endpoints = cluster_endpoints_sockets
        .iter()
        .map(ContactInfo::new_gossip_entry_point)
        .collect::<Vec<_>>();

    let advertised_ip = solana_net_utils::get_public_ip_addr_with_binding(
        &cluster_endpoints_sockets[0],
        bind_address,
    )
    .unwrap();

    info!("Advertised IP: {:?}", advertised_ip);

    let ledger_path = Path::new("ledger");
    let node_config = NodeConfig {
        advertised_ip,
        gossip_port: port_range.0,
        port_range,
        bind_ip_addrs: Arc::new(BindIpAddrs::new(vec![bind_address]).unwrap()),
        public_tpu_addr: None,
        public_tpu_forwards_addr: None,
        vortexor_receiver_addr: None,
        num_tvu_receive_sockets: 1.try_into().unwrap(),
        num_tvu_retransmit_sockets: 1.try_into().unwrap(),
        num_quic_endpoints: 1.try_into().unwrap(),
    };

    let my_shred_version = solana_net_utils::get_cluster_shred_version_with_binding(
        &cluster_endpoints_sockets[0],
        bind_address,
    )
    .unwrap();
    info!("Shred version: {:?}", my_shred_version);
    let socket_addr_space = SocketAddrSpace::Unspecified;

    let mut node = Node::new_with_external_ip(&gossip_keypair.pubkey(), node_config);
    node.info.set_shred_version(my_shred_version);
    node.info.set_wallclock(timestamp());

    let mut cluster_info =
        ClusterInfo::new(node.info.clone(), gossip_keypair.clone(), socket_addr_space);
    cluster_info.set_entrypoints(cluster_endpoints);
    cluster_info.set_bind_ip_addrs(node.bind_ip_addrs.clone());
    cluster_info.set_contact_debug_interval(DEFAULT_CONTACT_DEBUG_INTERVAL_MILLIS);
    cluster_info.restore_contact_info(ledger_path, DEFAULT_CONTACT_SAVE_INTERVAL_MILLIS);
    let cluster_info = Arc::new(cluster_info);

    let gossip_service = GossipService::new(
        &cluster_info,
        None,
        node.sockets.gossip.clone(),
        None,
        true,
        None,
        exit.clone(),
    );
    gossip_service
}
