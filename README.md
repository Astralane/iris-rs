# iris-rs
A fast and lightweight solana transaction sender, based on amazing previous works like [atlas](https://github.com/helius-labs/atlas-txn-sender) and agave's [tpu-client-next](https://github.com/anza-xyz/agave/blob/master/tpu-client-next)

## Change Log (17th june)

### ENV variables changed

* USE_TPU_CLIENT_NEXT( **REMOVED** )
* TX_MAX_RETRIES (**Renamed** from MAX_RETRIES)
* METRICS_UPDATE_INTERVAL_SECS  (**ADDED**)
* LEADER_FORWARD_COUNT (**ADDED**)
* OTPL_ENDPOINT (**ADDED**)
* TX_RETRY_INTERVAL_MS (**Renamed** from RETRY_INTERVAL_SECONDS)
* SHIELD_POLICY_KEY(**ADDED**)
* ~~RPC_NUM_THREADS(**ADDED**)~~
* ~~TPU_CLIENT_NUM_THREADS (**ADDED**)~~
* ~~QUIC_SERVER_THREADS (**ADDED**)~~
* ~~RPC_URLS (**MODIFIED**)~~
* ~~WS_URLS (**MODIFIED**)~~
* ~~QUIC_BIND_ADDRESS (**ADDED** -> needs to be open at firewall)~~


**IRIS needs to be run with RUST_LOG="solana_tpu_client_next=info,iris_rpc=info,quinn=trace,quin_proto=trace,info"**

### Summary of changes


* otpl is used to send logs to us , in order to do analysis and debug issues on iris which include leaders iris cannot connect to (endpoint will be provided)
* metrics update interval secs is how fast you update metrics, 1 second is good for this
* added an mev protect feature to prevent users from sending transactions to malicious leaders we are using the following contract address from yellow stone 8LXyNpkdnCKCCaUWBku1wD2B1HHaZ41FRgFEN6jNmytv
* ~~threads are added to give the operator more control over how much threads it should use , for small validators 8 threads should be good enough~~
* ~~Added backups for rpc url as well as http url since rpcs tend to crash sometimes its important to have a fail over , currently both of these variables accept array as an input . Please note all the addresses provided must be avaible for iris to start~~
* ~~Added quic bind address to create a more stream oriented connection as opposed to spamming http connections. This address must be whitelisted on your firewall with our txn router ips (PLEASE NOTE ADDRESS variable is still needed to provide backward compatibility)~~



