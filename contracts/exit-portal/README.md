# `contracts/exit-portal` — the L2 exit portal

`RomeExitPortal` is the sole entry point for an L2-to-Solana native-asset exit ("withdrawals off
finalized roots"). It is small on purpose: no gas limit, no target, no calldata — a fixed-shape,
native-asset-only message, shaped like OP Stack's `L2ToL1MessagePasser` but smaller.

## What it does

`initiateExit(bytes32 solRecipient)` is `payable`. It:

1. Refuses `msg.value == 0` (`ZeroAmount`), `msg.value > type(uint128).max` (`AmountOverflow`), and
   `solRecipient == bytes32(0)` (`ZeroRecipient`) — cheapest check first.
2. Takes the portal's own monotonic `messageNonce`, then computes
   `messageHash = keccak256(abi.encode(uint256(nonce), msg.sender, solRecipient, address(0), uint256(msg.value)))`
   — a 160-byte preimage. `asset` is hardcoded `address(0)` (native ETH; v1 supports no other asset).
3. Records `sentMessages[messageHash] = true` and emits `ExitInitiated(messageHash, nonce, msg.sender,
   solRecipient, address(0), msg.value)` with `messageHash` as the only indexed field.

**`sentMessages` is declared at storage slot 0, `messageNonce` at slot 1 — nothing before them.** This
is not incidental: the Solana-side `ProveExit` instruction (`programs/zk-settlement`) proves
inclusion of `sentMessages[messageHash] == true` at storage slot `keccak256(abi.encode(messageHash,
uint256(0)))` via an Ethereum Merkle-Patricia proof (`eth_getProof`) against the batch's finalized
`state_root` — a slot at any other offset breaks that proof's own derivation.

**ETH stays in this contract.** There is no `withdraw`, no `receive()`, no `fallback()` — a plain
transfer reverts. The L2 balance is burned by construction; the Solana-side twin asset is released from
an off-chain vault (`programs/zk-bridge`) once `ProveExit` succeeds.

## Bound: censorship

Exit liveness depends on sequencer inclusion until the forced-inclusion lane ships: a censoring
sequencer can refuse to include an `initiateExit` transaction, and there is no alternate path in v1. See
`docs/ARCHITECTURE.md`'s threat-model row for the same bound stated at the system level.

## Build and test

Every `forge`/`anvil` invocation runs inside the pinned image `ghcr.io/foundry-rs/foundry:v1.3.6`
(digest `sha256:7026b071fb16606a14426acc0a0e2cd57f80b96abd7e7d371fef8dafe1712cdf`) on a build machine — never
a developer's own forge/anvil. From a checkout on that machine:

```
cd <checkout>
sudo docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/work -w /work/contracts/exit-portal \
  ghcr.io/foundry-rs/foundry:v1.3.6 "forge test -vvv"
```

Runs as the invoking user (never `--user root`, never a `chown` cleanup after — the same shape the CI
`contracts` job and `make exit-fixtures` both use). Plain `sudo` is fine here: nothing this command needs
is read back out of the *invoking shell's* own environment by a bare `-e VAR` — `-e HOME=/tmp` is a
literal value on docker's own argv. Contrast the deploy script's `-e PRIVATE_KEY`,
which *is* a passthrough of the caller's own env var and so needs `sudo --preserve-env=PRIVATE_KEY`
specifically.

`foundry.toml` pins `solc_version = 0.8.28`, `evm_version = cancun`, `optimizer = true` /
`optimizer_runs = 200`, `bytecode_hash = none`, `cbor_metadata = false` — the runtime bytecode is
reproducible byte-for-byte from any forge of that solc version, which is what lets
`contracts/exit-portal/RomeExitPortal.runtime.hex` be committed and diff-checked in CI rather than
rebuilt fresh at deploy time.

## Regenerating fixtures

`make exit-fixtures` runs `contracts/exit-portal/script/fixtures.sh` inside the pinned image on a build
machine, then pulls the results back into this checkout: `fixtures/exit/*` and
`contracts/exit-portal/RomeExitPortal.runtime.hex`. See `fixtures/exit/README.md` for what each file is
and the one field (`anvil_state_root.json`'s `block_hash`) that does not reproduce byte-for-byte, and why.

## Deploying to Tiber (no reset)

The operator's deploy script for Tiber has a `--dry-run` mode that prints exactly what a real deploy would run
without fetching any key or touching the network. `--confirm` is the operator's go step: it runs on the Tiber
VM itself, where the VM's own service account can read the deployer key from the secret store. The deployer
key never touches this repo, argv, or disk — see the script's own header for the exact mechanism.

## Greenfield predeploy

A new chain's `genesis.json` (rendered by the operator's genesis render script from
`genesis.json.template`) carries `RomeExitPortal`'s runtime bytecode pre-deployed at
`0x4200000000000000000000000000000000000016`, sourced from the committed
`RomeExitPortal.runtime.hex` — refused by name (`ExitPortalRuntimeMissing`) if that file is absent or
not valid hex.

**Tiber itself never gets this predeploy from a routine render.** The genesis renderer only writes a fresh
`genesis.json` where none exists (or where the render is byte-identical). Against Tiber's tracked,
live, dev-account-only `genesis.json` a fresh render differs (this predeploy) — the routine file push
therefore calls it with `--keep-existing`: the file is kept byte-for-byte, one line reports
`differs in: alloc[0x42…16]`, and the push continues; a bare run refuses by name (`GenesisDrift`). So a
routine push can never swap the live genesis for one carrying the portal (a `reth` node refuses to start
on a genesis-hash mismatch against its own chain state). Tiber gets the portal the way any contract
reaches a live chain: the deploy script's `--confirm` run (see above). Only a full reset
renders fresh — it removes the existing `genesis.json` before re-deploying.
