# iris-rs
A fast and lightweight solana transaction sender, based on amazing previous works like [atlas](https://github.com/helius-labs/atlas-txn-sender) and agave's [tpu-client-next](https://github.com/anza-xyz/agave/blob/master/tpu-client-next)

## Change Log (17th june)

### ENV variables changed

* USE_TPU_CLIENT_NEXT( **REMOVED** )
* TX_MAX_RETRIES (**Renamed** from MAX_RETRIES)
* TX_RETRY_INTERVAL_SECONDS (**Renamed** from RETRY_INTERVAL_SECONDS)
* QUIC_BIND_ADDRESS (**ADDED** -> needs to be open at firewall)
* QUIC_SERVER_THREADS (**ADDED**)
* METRICS_UPDATE_INTERVAL_SECS  (**ADDED**)
* LEADER_FORWARD_COUNT (**ADDED**)
* OTPL_ENDPOINT (**ADDED**)

**IRIS needs to be run with RUST_LOG="solana_tpu_client_next=info,iris_rpc=info"**

### Summary of changes

* Added quic bind address to create a more stream oriented connection as opposed to spamming http connections. This address must be whitelisted on your firewall with our txn router ips (PLEASE NOTE ADDRESS variable is still needed to provide backward compatibility)
* otpl is used to send logs to us , in order to do analysis and debug issues on iris which include leaders iris cannot connect to (endpoint will be provided)
* metrics update interval secs is how fast you update metrics, 1 second is good for this
* quic server threads are added to give the operator more control over how much threads it should use , for small validators 8 threads should be good enough


