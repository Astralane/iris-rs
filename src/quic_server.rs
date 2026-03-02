use tokio_util::sync::CancellationToken;
use crate::tpu_next_client::TpuClientNextSender;

pub struct QuicServer {}

impl QuicServer {
    pub fn run(bind_port: u16, tpu_sender: TpuClientNextSender, cancel: CancellationToken) -> Self {
        
    }
}
