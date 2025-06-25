use async_stream::stream;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, StreamExt};
use solana_client::nonblocking::pubsub_client::{PubsubClient, PubsubClientResult};
use solana_client::rpc_config::{RpcBlockSubscribeConfig, RpcBlockSubscribeFilter};
use solana_client::rpc_response::{Response, RpcBlockUpdate, SlotInfo, SlotUpdate};
use std::collections::VecDeque;

#[derive(Debug)]
pub struct SmartPubsubClient {
    clients: Vec<PubsubClient>,
}

type UnsubFn = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;
type SubscribeResult<'a, T> = PubsubClientResult<(BoxStream<'a, T>, UnsubFn)>;

const DEDUP_BUFFER_SIZE: usize = 64;

impl SmartPubsubClient {
    pub async fn new(ws_urls: &[String]) -> PubsubClientResult<Self> {
        let client_futures = ws_urls
            .iter()
            .map(|url| PubsubClient::new(url.as_str()))
            .collect::<Vec<_>>();
        let clients = futures_util::future::try_join_all(client_futures).await?;
        Ok(Self { clients })
    }

    pub async fn shutdown(self) {
        for client in self.clients.into_iter() {
            client.shutdown().await.unwrap()
        }
    }

    pub async fn slot_update_subscribe(&self) -> SubscribeResult<'_, SlotUpdate> {
        self.subscribe_and_dedup(|client| client.slot_updates_subscribe().boxed())
            .await
    }

    pub async fn slot_subscribe(&self) -> SubscribeResult<'_, SlotInfo> {
        self.subscribe_and_dedup(|client| client.slot_subscribe().boxed())
            .await
    }

    pub async fn block_subscribe(
        &self,
        filter: RpcBlockSubscribeFilter,
        config: Option<RpcBlockSubscribeConfig>,
    ) -> SubscribeResult<'_, Response<RpcBlockUpdate>> {
        self.subscribe_and_dedup(|client| {
            client
                .block_subscribe(filter.clone(), config.clone())
                .boxed()
        })
        .await
    }

    async fn subscribe_and_dedup<T, F>(&self, subscribe_fn: F) -> SubscribeResult<'_, T>
    where
        T: serde::Serialize + Clone + PartialEq + Send + 'static,
        F: Fn(&PubsubClient) -> BoxFuture<'_, PubsubClientResult<(BoxStream<'_, T>, UnsubFn)>>
            + Send,
    {
        let streams_create_fut = self
            .clients
            .iter()
            .map(|client| subscribe_fn(client))
            .collect::<Vec<_>>();
        let streams_with_unsub = futures_util::future::try_join_all(streams_create_fut).await?;
        let (streams, unsubs): (Vec<_>, Vec<_>) = streams_with_unsub.into_iter().unzip();
        let unsub_all = Box::new(move || {
            async move {
                futures_util::future::join_all(unsubs.into_iter().map(|unsub| unsub())).await;
            }
            .boxed()
        });
        let combined_stream = combine_and_dedup_stream(streams);
        Ok((combined_stream, unsub_all))
    }
}

pub fn combine_and_dedup_stream<T: serde::Serialize + Clone + PartialEq + Send + 'static>(
    streams: Vec<BoxStream<T>>,
) -> BoxStream<T> {
    let mut seen: VecDeque<T> = VecDeque::with_capacity(DEDUP_BUFFER_SIZE);
    let mut combined_stream = futures_util::stream::select_all(streams);
    let stream = stream! {
        while let Some(item) = combined_stream.next().await {
            if !seen.contains(&item) {
                seen.push_back(item.clone());
                if seen.len() > DEDUP_BUFFER_SIZE {
                    seen.pop_front();
                }
                yield item;
            }
        }
    };
    Box::pin(stream)
}
