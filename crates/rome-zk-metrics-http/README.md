# rome-zk-metrics-http

One function: `serve(addr, render)`. Binds `addr`, and for every connection reads up to 512 bytes,
answers `GET /metrics` with `render()`'s bytes (`Content-Type: text/plain; version=0.0.4`, the Prometheus
text-exposition format) and 200, and answers anything else with an empty 404. `render` is called fresh on
every request — this crate never caches a snapshot.

## Why this crate exists

`rome-zk-sequencer` hand-rolled this responder in its own `metrics.rs` (no HTTP framework — one endpoint
doesn't need one). It was pulled out so `rome-zk-batcher` — which has no reth dependency, and
should never grow one just to expose a `/metrics` port — can reuse the exact same responder rather than a
second hand-rolled copy. `rome_zk_sequencer::metrics::serve_metrics` is now a thin call into
[`serve`](src/lib.rs); its own behaviour and tests are unchanged.

## Depends on

Nothing but `tokio` (`net`, `io-util`, `rt` — just enough to bind a listener, read/write a socket, and
`tokio::spawn` one task per connection). No Solana, no reth, no alloy.

## How to test

```sh
cargo test -p rome-zk-metrics-http
```
