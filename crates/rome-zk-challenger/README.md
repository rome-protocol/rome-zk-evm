# rome-zk-challenger

**Skeleton only — no behavior yet.** This crate will become the challenge-flow client: the service that
watches [`rome-zk-derive`](../rome-zk-derive)'s independently derived chain against what is posted to the
settlement program, and opens and resolves per-block disputes when they disagree.

## What it will guarantee

Once built, this crate is expected to implement:

- Non-exclusive, per-block disputes: any number of blocks in a pending batch may be disputed
  independently, and a dispute never blocks other disputes on the same batch from proceeding.
- A challenger bond posted per dispute, sized so that a challenger cannot cheaply flood a poster with
  disputes and force it onto the slower (CPU) proving fallback repeatedly within one window.
- Resolution by a genuine one-block proof — never by proving or disproving a whole batch — checked against
  the batch's own committed per-block state roots.
- Awareness of the forced-inclusion lane, once it exists: a dispute over an omitted forced message is a
  distinct case from a dispute over an ordinary sequencer-lane block.

None of the above exists in the code yet. Do not depend on this crate's behavior in anything else until it
does.

## Config

Not yet defined.

## How to test

```sh
cargo test -p rome-zk-challenger
```

There is nothing to test yet beyond the crate compiling.

## Security notes

Not applicable yet — there is no behavior to secure. This crate is one of the pieces that closes a gap
named in [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md#trust-model): until it exists, an unproved
(`PostRoot`) batch that nobody happens to be watching has no automated challenger enforcing its
window-based finality condition against an actually-wrong root.

## Design references

The lane design covers the challenge flow in full (the state machine, non-exclusivity, bonds and caps) and
bond and window sizing.

## Depends on

Expected to depend on [`rome-zk-derive`](../rome-zk-derive) (as its oracle for "what actually happened")
and [`zk-settlement-client`](../zk-settlement-client) (to open and resolve disputes on chain); expected to
share its Postgres schema with [`rome-zk-prover`](../rome-zk-prover) — none of this is wired up yet.
