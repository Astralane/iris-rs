## Change Log 2.0 ( 26th October)

### ENV variables changed
* GOSSIP_KEYPAIR_FILE [OPTIONAL]
* GOSSIP_PORT_RANGE [OPTIONAL]
* DEDUP_CACHE_MAX_SIZE 
### Summary of changes
* Added support to deduplicate transactions before sending them to the validator.
* Modified TPU-level reconnections to minimize connection timeout errors [https://github.com/Astralane/iris-rs/pull/25]
* Introduced GOSSIP support, allowing Iris nodes to be available on the network.
---

## Change Log (17th june)

### ENV variables changed

* USE_TPU_CLIENT_NEXT( **REMOVED** )
* TX_MAX_RETRIES (**Renamed** from MAX_RETRIES)
* METRICS_UPDATE_INTERVAL_SECS  (**ADDED**)
* LEADERS_FANOUT (**ADDED**)
* OTPL_ENDPOINT (**ADDED**)
* TX_RETRY_INTERVAL_MS (**Renamed** from RETRY_INTERVAL_SECONDS)
* SHIELD_POLICY_KEY(**ADDED**)

**IRIS needs to be run with RUST_LOG="solana_tpu_client_next=debug"**

### Summary of changes


* otpl is used to send logs to us , in order to do analysis and debug issues on iris which include leaders iris cannot connect to (endpoint will be provided)
* metrics update interval secs is how fast you update metrics, 1 second is good for this
* added an mev protect feature to prevent users from sending transactions to malicious leaders we are using the following contract address from yellow stone 4QXuzwHutRGjMHRfpGgZpaC9LEYR2wqVmLJBbPbK1zQo
