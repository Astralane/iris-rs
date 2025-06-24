use solana_client::client_error::reqwest::Url;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

pub struct SmartRpcClient {
    clients: Vec<Arc<RpcClient>>,
    best_rpc: Arc<AtomicUsize>,
    runtime: Option<Runtime>,
    task: Option<tokio::task::JoinHandle<()>>,
    cancel: CancellationToken,
}

impl SmartRpcClient {
    pub fn new(
        urls: &[Url],
        refresh_interval: Duration,
        commitment: Option<CommitmentConfig>,
    ) -> SmartRpcClient {
        let clients = urls
            .into_iter()
            .map(|url| Arc::new(RpcClient::new(url.to_string())))
            .collect::<Vec<_>>();
        let current_handle = tokio::runtime::Handle::try_current();
        let runtime = if current_handle.is_ok() {
            None
        } else {
            Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("smart-rpc-rt")
                    .enable_all()
                    .build()
                    .unwrap(),
            )
        };
        let rt_handle = match runtime.as_ref() {
            Some(rt) => rt.handle().clone(),
            None => current_handle.unwrap(),
        };
        let best_rpc = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let task_handle = rt_handle.spawn(Self::watcher_loop(
            clients.clone(),
            best_rpc.clone(),
            refresh_interval,
            cancel.clone(),
        ));

        Self {
            clients,
            best_rpc,
            runtime,
            task: Some(task_handle),
            cancel,
        }
    }

    pub fn stop(&mut self) {
        self.cancel.cancel();
    }

    pub async fn join(self) {
        if let Some(task_handle) = self.task {
            task_handle.await.unwrap()
        }
    }

    pub fn best_rpc(&self) -> Arc<RpcClient> {
        self.clients[self.best_rpc.load(std::sync::atomic::Ordering::SeqCst)].clone()
    }

    async fn watcher_loop(
        rpc: Vec<Arc<RpcClient>>,
        best_index: Arc<AtomicUsize>,
        refresh_interval: std::time::Duration,
        cancel: CancellationToken,
    ) {
        let mut interval = tokio::time::interval(refresh_interval);
        loop {
            let tick = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => (),
            };
            let slot_result_futs = rpc
                .iter()
                .map(|client| client.get_slot_with_commitment(CommitmentConfig::processed()));
            let slot_results = futures::future::join_all(slot_result_futs).await;

            let best_index_value = slot_results
                .into_iter()
                .enumerate()
                .filter_map(|(i, res)| res.ok().map(|slot| (i, slot)))
                .max_by_key(|(_, slot)| *slot)
                .map(|(i, _)| i);

            if let Some(best_index_value) = best_index_value {
                best_index.store(best_index_value, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}
