use iris_quic_server::quic_server::IrisQuicServer;
use iris_quic_server::vendor::quic_networking::configure_client_endpoint;
use quinn::Connection;
use solana_perf::packet::PACKET_DATA_SIZE;
use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::v0::Message;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{EncodableKey, Keypair, Signer};
use solana_sdk::transaction::VersionedTransaction;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct TestQuicClient {
    signer: Keypair,
    connection: Connection,
    interval: Duration,
    rpc: Arc<solana_client::nonblocking::rpc_client::RpcClient>,
}
pub const MEMO_PROGRAM: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

impl TestQuicClient {
    pub async fn create_and_connect(
        socket: SocketAddr,
        signer: Keypair,
        dest_socket: SocketAddr,
        interval: Duration,
    ) -> Self {
        let endpoint = configure_client_endpoint(socket, Some(&signer)).unwrap();
        let connecting = endpoint.connect(dest_socket, "producer").unwrap();
        let connection = connecting.await.unwrap();
        let rpc = Arc::new(solana_client::nonblocking::rpc_client::RpcClient::new(
            "http://rpc:8899".to_string(),
        ));
        println!("Connected");
        Self {
            signer,
            connection,
            interval,
            rpc,
        }
    }

    pub async fn send_dummy_txns(&self) {
        let hash = self.rpc.get_latest_blockhash().await.unwrap();
        let wire_transactions = bincode::serialize(&dummy_memo_transaction(&self.signer, hash))
            .ok()
            .unwrap();
        self.send_data_over_stream(&wire_transactions).await;
    }

    pub async fn send_data_over_stream(&self, wire_transactions: &[u8]) {
        let mut send_stream = self.connection.open_uni().await.unwrap();
        send_stream.write_all(&wire_transactions).await.unwrap();
    }
}

pub fn dummy_memo_transaction(signer: &Keypair, blockhash: Hash) -> VersionedTransaction {
    let compute_budget_ix =
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(933866);
    let compute_unit_price =
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(100_000);
    let data = vec![4u8; 1024 - 16];
    let memo_instruction = Instruction {
        accounts: vec![AccountMeta::new(signer.pubkey(), true)],
        program_id: MEMO_PROGRAM,
        data,
    };
    let versioned_message = Message::try_compile(
        &signer.pubkey(),
        &vec![compute_budget_ix, compute_unit_price, memo_instruction],
        &[],
        blockhash,
    )
    .unwrap();
    let tx = solana_sdk::transaction::VersionedTransaction::try_new(
        VersionedMessage::V0(versioned_message),
        &[signer],
    )
    .unwrap();
    let hash = tx.verify_and_hash_message().unwrap();
    let serialized = bincode::serialize(&tx).unwrap();
    println!(
        "created dummy txns with signature {:?} hash: {:?}, size: {:?} max size: {:?}",
        tx.signatures[0],
        hash,
        serialized.len(),
        PACKET_DATA_SIZE
    );
    tx
}
#[test]
fn test_forwarder() {
    let (sender, receiver) = crossbeam_channel::unbounded();
    let keypair = Keypair::new();
    let cancel = CancellationToken::new();
    let server = IrisQuicServer::spawn_new(
        "iris-quic-forward-t",
        UdpSocket::bind("127.0.0.1:52104").unwrap(),
        sender,
        &keypair,
        cancel.clone(),
        4,
    )
    .unwrap();
    let signer = Keypair::read_from_file("/Users/nuel/.config/solana/id.json").unwrap();
    let hdl = std::thread::spawn(move || {
        while let Ok(batch) = receiver.recv() {
            // deserialize transaction and check if the signature matches the scent one
            let tx: Vec<VersionedTransaction> = batch
                .iter()
                .map(|packet| packet.deserialize_slice(..).unwrap())
                .collect();
            println!(
                "received signature: {:?}, hash_result {:?}",
                tx[0].signatures[0],
                tx[0].verify_and_hash_message().unwrap()
            )
        }
    });
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    rt.block_on(async move {
        let client = TestQuicClient::create_and_connect(
            SocketAddr::new("0.0.0.0".parse().unwrap(), 52105),
            signer,
            SocketAddr::new("100.81.253.106".parse().unwrap(), 13000),
            Duration::from_millis(400),
        )
        .await;
        client.send_dummy_txns().await;
        println!("sent dummy txns");
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    });
    std::thread::sleep(std::time::Duration::from_secs(10));
    cancel.cancel();
    println!("exit signalled");
    server.join().unwrap();
    println!("server exited");
    hdl.join().unwrap();
}
