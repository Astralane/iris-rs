use crate::utils::SendTransactionClient;
use crossbeam_channel::RecvTimeoutError;
use iris_quic_server::quic_server::IrisQuicServer;
use solana_sdk::signature::Keypair;
use std::net::UdpSocket;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub struct QuicTxForwarder {
    iris_quic_server: IrisQuicServer,
    forward_handle: std::thread::JoinHandle<()>,
}
impl QuicTxForwarder {
    pub fn spawn_new(
        tpu_socket: UdpSocket,
        tx_sender_client: Arc<dyn SendTransactionClient>,
        identity_keypair: &Keypair,
        cancel: CancellationToken,
        num_threads: usize,
    ) -> Self {
        const RECEIVE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(30);
        let (sender, receiver) = crossbeam_channel::unbounded();
        let server = IrisQuicServer::spawn_new(
            "iris_quic_forwarder_t",
            tpu_socket,
            sender,
            identity_keypair,
            cancel.clone(),
            num_threads,
        )
        .unwrap();

        let forward_handle = std::thread::Builder::new()
            .name("quic_forward_t".to_string())
            .spawn(move || {
                while !cancel.is_cancelled() {
                    let packet_batch = match receiver.recv_timeout(RECEIVE_CHECK_TIMEOUT) {
                        Ok(packets) => packets,
                        Err(e) => match e {
                            RecvTimeoutError::Timeout => continue,
                            RecvTimeoutError::Disconnected => {
                                break;
                            }
                        },
                    };

                    let tx_batch = packet_batch
                        .iter()
                        .map(|packet| packet.data(..).unwrap().to_vec())
                        .collect::<Vec<_>>();

                    tx_sender_client.send_transaction_batch(tx_batch);
                }
            })
            .unwrap();

        Self {
            iris_quic_server: server,
            forward_handle,
        }
    }

    pub fn join(self) -> std::thread::Result<()> {
        self.iris_quic_server.join()?;
        self.forward_handle.join()
    }
}
