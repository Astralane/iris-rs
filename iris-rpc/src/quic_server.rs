use std::net::{SocketAddr, UdpSocket};

pub struct QuicServer {
    thread_handles: Vec<std::thread::JoinHandle<()>>,
}