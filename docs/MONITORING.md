# Monitoring one rollup

Use this with [deploy/rollup](../deploy/rollup/README.md). It is a set of checks, not a monitoring stack. Scrape the local endpoints with your own Prometheus if you use one. The [example alert rules](monitoring/alerts.example.yml) need your scrape jobs and paging route.

| Service | Local endpoint | What to watch |
| --- | --- | --- |
| sequencer | `127.0.0.1:9001/metrics` | Blocks and idle ticks |
| batcher | `127.0.0.1:9002/metrics` | Group age and finalized batches |
| derive | `127.0.0.1:9003/metrics` | Derived head and critical errors |
| prover, when enabled | `127.0.0.1:9004/metrics` | Proof and settlement lag |

The compose file publishes these ports on loopback. `reth-verifier` has an RPC on `127.0.0.1:8547`, but no Rome `/metrics` endpoint. Run `./rollup check` from `deploy/rollup` on a timer. It checks that sequencer, batcher, reth-verifier and derive are running, plus prover and Postgres when enabled. It also checks the two RPCs and the Solana cursor and root accounts. Page if a required service or RPC stays unavailable. A scrape failure alone does not tell you why a service stopped.

The batcher, derive and prover exit on a fatal error. Docker restarts them, so a crash loop can look like a running service. Alert on container restarts too. For example, watch for a rising `docker inspect -f '{{.RestartCount}}' <container>` value.

## Blocks and batches

When transactions are reaching the sequencer, `rome_zk_sequencer_blocks_sealed_total` should rise. On a quiet chain, blocks can stop; `rome_zk_sequencer_idle_ticks_total` should then rise instead. `./rollup check` samples the RPC block number and the idle counter over three seconds. Page if neither moves while the service is running. If a known submitted transaction is not included, page even if idle ticks are rising. There is no metric that proves a transaction was submitted and is waiting for a block.

`rome_zk_batcher_oldest_unposted_block_age_seconds` is the age of the group the batcher is still filling. The batcher closes that group once it is `batcher.batch_close_after_secs` old (60 seconds in the example), and the gauge returns to zero. It is not updated while the batcher waits on Solana, so it cannot show a stuck batcher. `./rollup check` allows the close window plus 60 seconds (120 seconds with the example settings), but has the same blind spot. Page when the sequencer has sealed blocks and `rome_zk_batcher_batches_finalized_total` has not risen for 15 minutes. A finalized inbox batch means its data was posted. It does not mean a settlement root is final.

The batcher stops on a posting failure and Docker restarts it, so its counters start again at zero. `rome_zk_batcher_batches_failed_total` counts only a chunk that does not match what was sent and a batch that is still not finalized after repeated `FinalizeBatch` attempts. Other errors, such as a failed send, stop the batcher without counting here. Page on batcher restarts and on missing batch finalizations instead.

Compare the sequencer and verifier `eth_blockNumber` results at ports 8545 and 8547. The verifier normally trails by less than two batches. The example has 60 blocks per batch, so `./rollup check` uses **120 blocks**. It fails when the gap reaches that limit and the verifier head does not advance during its sample. `reth_blockchain_tree_canonical_chain_height` on the sequencer and `rome_zk_derive_head_block` give a metric view of the gap; confirm an alert against the RPC heads. Page if the verifier stops catching up.

After a derive restart, `rome_zk_derive_head_block` reads zero until derive finishes its next batch. On a quiet chain, a raw metric gap can look large while the RPC heads agree. The example rule ignores a zero derived head. Watch restarts separately.

Derive stops on a strict error. The process exits, Docker restarts it, and it usually stops again at the same batch. `rome_zk_derive_critical_total` starts at zero in each new process, so a scrape rarely sees it rise. Page on derive restarts and on the derived head falling behind; the derive logs name the error. At the same block height, compare the block hashes returned by both RPCs. Page on a mismatch. No exported metric compares those hashes for you.

## Payer and settlement

The payer needs SOL for Solana fees, for rent on inbox batch and chunk accounts, and, with a prover, for the 0.001 SOL settlement fee and rent on each posted root's pending account. For a posted batch, inbox rent can be reclaimed only after a root covering it is final. Nothing in `deploy/rollup` closes posted inbox accounts. The prover closes pending accounts only when `close_pending_after_batches` is set. Plan for rent to keep accumulating, with or without a prover.

Check the payer's public address with `solana balance <PAYER_ADDRESS> --url <SOLANA_RPC_URL>` on a timer. Set a page floor from your measured spend and funding lead time, before the balance reaches zero. The batcher exports no payer balance metric. A running prover exports `rome_zk_prover_payer_lamports`, but samples it opportunistically; use the direct balance check for funding decisions.

Until the chain posts its first root, anyone can reclaim it about 30 days after registration. That closes its chain accounts and stops the batcher. Track that date for a chain without a prover.

When the prover runs, `./rollup check` expects no more than **2** posted batches without a final root. It reads the inbox cursor and settlement root accounts directly. The prover's `rome_zk_prover_head` gauges are its own last reads. They are refreshed once per batch attempt and can be up to 30 minutes old during a proof. The final head is not refreshed while the prover waits for the next inbox batch. A proved root is final when it posts, so `rome_zk_prover_batches_behind` gives the same count from fresher reads. Page if it stays above **2**, and use `./rollup check` for the direct Solana read.

`./rollup check` also uses a **600 second** bound for `rome_zk_prover_lag_seconds`. This gauge keeps the value from the last batch the prover posted. On a quiet chain, one slow batch can leave it above 600 seconds with nothing left to prove, so `./rollup check` applies the bound only while `rome_zk_prover_batches_behind` is above zero. Page on it the same way. Without a prover, skip settlement and prover lag alerts. Inbox posting alone cannot settle the chain.

## Disk

Watch free space and growth for `sequencer-data` (the sequencer's reth data and ordered log) and `verifier-data` (the archive verifier's reth data). With `PROVER=on` there are three more: `postgres-data` (the prover's history, which grows with every batch), `prover-work` (one directory per batch in progress, removed after it posts by default) and `zisk-cache`. Page early enough to add space before a filesystem fills. These services do not export disk usage or free space metrics; use your host or container monitoring for those alerts.
