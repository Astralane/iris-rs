use crate::chain_listener::ChainListener;
use crate::solana_sender::SolanaRpcSender;
use crate::store::TransactionData;
use crate::utils::{SendTransactionClient, LOCAL_RPC_URL};
use log::error;
use solana_client::rpc_client::SerializableTransaction;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use weighted_rs::{RoundrobinWeight, Weight};

pub struct TransactionProcessor {
    thread_hdl: JoinHandle<()>,
}

impl TransactionProcessor {
    pub fn spawn_new_with_sender(
        runtime: tokio::runtime::Handle,
        tx_receiver: std::sync::mpsc::Receiver<TransactionData>,
        send_client: Arc<dyn SendTransactionClient>,
        client_weight: u32,
        other_rpcs: Vec<(String, u32)>,
        enable_routing: bool,
        chain_listener: Arc<ChainListener>,
    ) -> Self {
        let all_rpcs = vec![(LOCAL_RPC_URL.to_owned(), client_weight)]
            .into_iter()
            .chain(other_rpcs.into_iter())
            .collect::<Vec<(String, u32)>>();

        let mut client_map = HashMap::new();
        for (rpc_url, _) in all_rpcs.iter() {
            if !rpc_url.eq(LOCAL_RPC_URL) {
                let rpc = SolanaRpcSender::new(rpc_url.to_owned(), runtime.clone());
                client_map.insert(
                    rpc_url.to_owned(),
                    Arc::new(rpc) as Arc<dyn SendTransactionClient>,
                );
            } else {
                client_map.insert(rpc_url.to_string(), send_client.clone());
            }
        }
        let hdl = if enable_routing {
            //routing logic, send to one rpc based on weighted round-robin algorithm
            thread::spawn(move || {
                let mut weighted = RoundrobinWeight::new();
                for (rpc_url, weight) in all_rpcs {
                    weighted.add(rpc_url, weight as isize);
                }
                loop {
                    let tx = tx_receiver.recv().unwrap();
                    let rpc_url = weighted.next().unwrap();
                    if let Some(client) = client_map.get(&rpc_url) {
                        chain_listener
                            .track_signature(tx.versioned_transaction.get_signature().to_string());
                        client.send_transaction(tx)
                    } else {
                        error!("No client found for rpc_url: {}", rpc_url);
                    }
                }
            })
        } else {
            //aggregation logic, send to all known rpcs and race the transaction landing
            thread::spawn(move || loop {
                let tx = tx_receiver.recv().unwrap();
                for (rpc_url, _) in all_rpcs.iter() {
                    if let Some(client) = client_map.get(rpc_url) {
                        chain_listener
                            .track_signature(tx.versioned_transaction.get_signature().to_string());
                        client.send_transaction(tx.clone());
                    }
                }
            })
        };
        Self { thread_hdl: hdl }
    }
}
