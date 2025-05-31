use iris_quic_forwarder::forwarder::IrisQuicForwarder;
use iris_quic_forwarder::vendor::quic_networking::configure_client_endpoint;
use quinn::Connection;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::v0::Message;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{EncodableKey, Keypair, Signer};
use solana_sdk::transaction::VersionedTransaction;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

struct Producer {
    signer: Keypair,
    connection: Connection,
    interval: Duration,
}
pub const MEMO_PROGRAM: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

impl Producer {
    pub async fn create_and_connect(
        socket: SocketAddr,
        signer: Keypair,
        dest_socket: SocketAddr,
        interval: Duration,
    ) -> Self {
        let endpoint = configure_client_endpoint(socket, Some(&signer)).unwrap();
        let connecting = endpoint.connect(dest_socket, "producer").unwrap();
        let connection = connecting.await.unwrap();
        Self {
            signer,
            connection,
            interval,
        }
    }

    pub async fn send_transactions(&self) {
        let wire_transactions =
            bincode::serialize(&dummy_memo_transaction(&self.signer, Default::default()))
                .ok()
                .unwrap();
        let mut send_stream = self.connection.open_uni().await.unwrap();
        send_stream.write_all(&wire_transactions).await.unwrap()
    }
}

pub fn dummy_memo_transaction(signer: &Keypair, blockhash: Hash) -> VersionedTransaction {
    let compute_budget_ix =
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(23000);
    let data = Keypair::new().pubkey();
    let memo_instruction = Instruction {
        accounts: vec![AccountMeta::new(signer.pubkey(), true)],
        program_id: MEMO_PROGRAM,
        data: data.to_string().as_bytes().to_vec(),
    };
    let versioned_message = Message::try_compile(
        &signer.pubkey(),
        &vec![compute_budget_ix, memo_instruction],
        &[],
        blockhash,
    )
    .unwrap();
    solana_sdk::transaction::VersionedTransaction::try_new(
        VersionedMessage::V0(versioned_message),
        &[signer],
    )
    .unwrap()
}
#[tokio::test(flavor = "multi_thread")]
async fn test_forwarder() {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let keypair = Keypair::new();
    let exit = Arc::new(AtomicBool::new(false));
    let forwarder = IrisQuicForwarder::create_new(
        "iris-quic-forward-t",
        UdpSocket::bind("127.0.0.1:52104").unwrap(),
        sender,
        keypair,
        exit.clone(),
        4,
    )
    .unwrap();
    let signer = Keypair::read_from_file("/Users/nuel/.config/solana/id.json").unwrap();
    let hdl = std::thread::spawn(move || {
        while let Ok(tx) = receiver.recv() {
            println!("received a packet {:?}", tx);
        }
    });
    let producer = Producer::create_and_connect(
        SocketAddr::new("127.0.0.1".parse().unwrap(), 52105),
        signer,
        SocketAddr::new("127.0.0.1".parse().unwrap(), 52104),
        Duration::from_millis(400),
    )
    .await;
    producer.send_transactions().await;
    tokio::time::sleep(Duration::from_secs(10)).await;
    exit.store(true, std::sync::atomic::Ordering::Relaxed);
    forwarder.join().unwrap();
    hdl.join().unwrap();
}
