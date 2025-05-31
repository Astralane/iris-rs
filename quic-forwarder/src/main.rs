use quic_forwarder::forwarder::IrisQuicForwarder;
use solana_sdk::signature::Keypair;
use std::net::UdpSocket;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub fn main() {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let keypair = Keypair::new();
    let udp_socket = UdpSocket::bind("0.0.0.0:0").unwrap();
    println!("UDP bind to {}", udp_socket.local_addr().unwrap());
    let exit = Arc::new(AtomicBool::new(false));
    let forwarder = IrisQuicForwarder::create_new(
        "iris-quic-forward-t",
        udp_socket,
        sender,
        keypair,
        exit.clone(),
        4,
    )
    .unwrap();

    forwarder.join().unwrap()
}
