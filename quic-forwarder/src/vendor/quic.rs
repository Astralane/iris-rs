use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use pem::Pem;
use quinn::rustls::KeyLogFile;
use quinn::ServerConfig;
use quinn_proto::crypto::rustls::QuicServerConfig;
use quinn_proto::IdleTimeout;
use solana_net_utils::{multi_bind_in_range_with_config, SocketConfig};
use solana_perf::packet::PACKET_DATA_SIZE;
use solana_quic_definitions::{QUIC_MAX_TIMEOUT, QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS};
use solana_sdk::signature::Keypair;
use solana_streamer::nonblocking::quic::ALPN_TPU_PROTOCOL_ID;
use solana_streamer::quic::QuicServerError;
use solana_tls_utils::{new_dummy_x509_certificate, tls_server_config_builder};

pub fn configure_server(
    identity_keypair: &Keypair,
) -> Result<(ServerConfig, String), QuicServerError> {
    let (cert, priv_key) = new_dummy_x509_certificate(identity_keypair);
    let cert_chain_pem_parts = vec![Pem {
        tag: "CERTIFICATE".to_string(),
        contents: cert.as_ref().to_vec(),
    }];
    let cert_chain_pem = pem::encode_many(&cert_chain_pem_parts);

    let mut server_tls_config =
        tls_server_config_builder().with_single_cert(vec![cert], priv_key)?;
    server_tls_config.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];
    server_tls_config.key_log = Arc::new(KeyLogFile::new());
    let quic_server_config = QuicServerConfig::try_from(server_tls_config)?;

    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_server_config));
    let config = Arc::get_mut(&mut server_config.transport).unwrap();

    // QUIC_MAX_CONCURRENT_STREAMS doubled, which was found to improve reliability
    const MAX_CONCURRENT_UNI_STREAMS: u32 =
        (QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS.saturating_mul(2)) as u32;
    config.max_concurrent_uni_streams(MAX_CONCURRENT_UNI_STREAMS.into());
    config.stream_receive_window((PACKET_DATA_SIZE as u32).into());
    config.receive_window((PACKET_DATA_SIZE as u32).into());
    let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
    config.max_idle_timeout(Some(timeout));

    // disable bidi & datagrams
    const MAX_CONCURRENT_BIDI_STREAMS: u32 = 0;
    config.max_concurrent_bidi_streams(MAX_CONCURRENT_BIDI_STREAMS.into());
    config.datagram_receive_buffer_size(None);

    // Disable GSO. The server only accepts inbound unidirectional streams initiated by clients,
    // which means that reply data never exceeds one MTU. By disabling GSO, we make
    // quinn_proto::Connection::poll_transmit allocate only 1 MTU vs 10 * MTU for _each_ transmit.
    // See https://github.com/anza-xyz/agave/pull/1647.
    config.enable_segmentation_offload(false);

    Ok((server_config, cert_chain_pem))
}

/// Binds the sockets to the specified address and port range if address is Some.
/// If the address is None, it binds to the specified bind_address and port range.
pub fn bind_sockets(
    bind_address: std::net::IpAddr,
    port_range: (u16, u16),
    address: Option<SocketAddr>,
    num_quic_endpoints: usize,
    quic_config: SocketConfig,
) -> Vec<UdpSocket> {
    let (bind_address, port_range) = address
        .map(|addr| (addr.ip(), (addr.port(), addr.port().saturating_add(1))))
        .unwrap_or((bind_address, port_range));

    let (_, sockets) =
        multi_bind_in_range_with_config(bind_address, port_range, quic_config, num_quic_endpoints)
            .expect("expected bind to succeed");
    sockets
}
