# Iris Batch Txn Result

### Bench methodology 
- Bench was between `sendTransaction` and `sendTransactionBatch` 
- in this we send 10000 txn 
	- for `sendTransaction` , 5 txn were sent at burst per second for 2000 times
	- for `sendTransactionBatch` , batch of 5 txn were sent per second for 2000 times
- both were sent at the same time

### Bench Result
- txn in a batch where relatively faster and consistently landing in a same slot then the txn sent in a burst 
- the avg slot difference between txn landed in the batch was `1.7` and median was `0` 
- the avg slot difference between txn landed in the burst was `2.11` and median was `2`


### Example
sendBatchTransaction:

| Batch_no | S.No | Txn signature                                                                            | slot_landed |
| -------- | ---- | ---------------------------------------------------------------------------------------- | ----------- |
| **0**    | 0    | 2Bv59L5eiLiLs13dTxVTZe3WVdjtouVfteW9KQ81Kv4MmCoxmVwzZmDqzBnLDtfs2ZVFdUKcoFxSzx8pChuLGDye | 312275048   |
| **0**    | 1    | bAxuDKnJXDBDfsaBQg3RqUkwNkT2puJtDWmYHjc1WseF7CtRzh3Nnsy1TJnW2YfK3S5euKAKC4t8qVTL61mKPtG  | 312275048   |
| **0**    | 2    | weHMfSEBs9jTuduAQ4mkc5Uoucx21GN4fCGJttxDpP2rBRZ7aFGLqJrx5MzAN7fjhDQKkuutRKbcU1u6UXWYTHH  | 312275048   |
| **0**    | 3    | 2L6Tjsv7m9QgjfW8Rei9sLBbPYsnYrnkTx3P2ZyZTHotUngrbNGgAjGfGsGWxXesL7WxiwdFBZxzgEmLWeBZB9iG | 312275048   |
| **0**    | 4    | 2WvSX2nqzxtYD55Ki5f49mQy9f1VsViBgCqD5wHYz8S1XDTmhgG1gVqGjUHFojZVoTwfir6vbrNf6Hb9FkNERJrK | 312275048   |
| **1**    | 5    | gvCCTwrfQamTa8RptzXYkrWafRLQfbCMSxhMtJUQ5hhY7KMy9x6Yym9waMzsHERp4yXV5HocX5cJBMRBdbv7fEw  | 312275051   |
| **1**    | 6    | 4GHLy9j5rKvMyoYucBMRrar4rGZDxk2kJkcHKn3KQcuKwAwHxEFXGoLv5Yn2ccGNn59t2w3C8C8ukYsnSHfzN6Aw | 312275051   |
| **1**    | 7    | 64et1PzP9hqtpAfQ7Tp8S1r5CsMuSKtH4b4YkwhsUVdmjrUkyHbxWsB5C2t8JwHkRbUnyJKTJJtqmXg7YGmLYX5X | 312275051   |
| **1**    | 8    | 5cdHuJXWo2YA74ST7uayemVZgiPt7ePC1rLxqGPdnY57oueJ5WQAeVbUkuYrWGjQDbKMEbgARUhqSrgXTCApk52r | 312275051   |
| **1**    | 9    | 5C1NGtvvi7XasjNbc8vzyifWyTg7bucMcFiJgiDk8VaG7tJD4fevgHwhjee3dyQ5it2oC5aK9ag2eu5vKNKk5GvL | 312275051   |


sendTransaction : 

| Batch_no | S.No | Txn signature                                                                            | slot_landed |
| -------- | ---- | ---------------------------------------------------------------------------------------- | ----------- |
| **0**    | 0    | incjRom9Gv5CVZ1v5nDWKNRgyQoNbpdHLhSwy96VVZvtFhhnZ7NPdWWU8BxXv1FYe9kfi5pzNeGAocwAy39Cj1o  | 312275048   |
| **0**    | 1    | 3uGhDoztzhkvVRvNpbNUo71SLbbCYw1N2hKcbd3dcBsHvzyqaJtkGRLnNZUU9uagGPLDZ8AWeAKSFw4aLxFkr981 | 312275048   |
| **0**    | 2    | 42v2Rr7ScTg4goQWcvD2hi5Nx6hodo7HVNwC7dEdfn7R4G68SVbuuGVN1kTSQkmni7EUfypvQjCLXZj5qK6H2EeE | 312275048   |
| **0**    | 3    | gMGNFBY9coutXTzNusNFsrnaGy3k5onCjtXfRW21cVK5Rt2AYpbigNQkbWgjtPBS7EMJxduh1EDtHMmJvZ14SnR  | 312275048   |
| **0**    | 4    | 4pUUyLs9s1Hunz4yEUme7U59yJEu8eNWyRMMokZxdNeayYqAcvMphvym2VuXQWki8UEvSdi43Bdv73a8fSv1VzB2 | 312275051   |
| **1**    | 5    | 5qikkNrtg12GgeU9L5GsybRvtyH12EqGKueaiEHZSPJikzV9bXGxzLTjARdZL23nfExWbvjDdiVbG6Cw5aCmLRph | 312275051   |
| **1**    | 6    | 4ZW3wrnsXRnQpQa4GwFCgs9EKoBfdbyakc9uJzadY6JXoWC9FkgTTx4fW59fLcrd2SoCZjK8Pi4ApUKxfk2r98we | 312275051   |
| **1**    | 7    | 2FJqM1UKTfVqtVENfNZjLjgoSvmXdVsbAzpTDKF1aZ7RMFXUHTvZ3AqJCWXL9D3GAYLSXNsUhv9RFVQQ1w4MXbqF | 312275051   |
| **1**    | 8    | 5UDvgC7CzJzgaaG7pDT3QcaFgEVv1yZmwR3JEssyM81mgDYqd5n9jeWTZZJVtRCQZf6SUf7TUzAxAXrDT6gYqgxB | 312275052   |
| **1**    | 9    | 66o6zv1vETvYyKrdLsmEtcB3qEZgnEkmQs1GPqEbtg3on6pZYat76M1hxcMXn77x3QehxrsnxrY8bP9Rm5qG94wo | 312275051   |
