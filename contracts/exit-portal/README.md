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
the chain's vault in `programs/zk-bridge`, on Solana, once `ProveExit` succeeds.

## Bound: censorship

Exit liveness depends on sequencer inclusion until the forced-inclusion lane ships: a censoring
sequencer can refuse to include an `initiateExit` transaction, and there is no alternate path in v1. See
`docs/ARCHITECTURE.md`'s threat-model row for the same bound stated at the system level.

## Build and test

Run `forge` inside the pinned image `ghcr.io/foundry-rs/foundry:v1.3.6` (digest
`sha256:7026b071fb16606a14426acc0a0e2cd57f80b96abd7e7d371fef8dafe1712cdf`) rather than a locally installed
forge, so the bytecode matches the committed runtime hex. From the repository root:

```
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/work -w /work/contracts/exit-portal \
  ghcr.io/foundry-rs/foundry:v1.3.6 "forge test -vvv"
```

The container runs as your own user, so the files it writes stay yours.

`foundry.toml` pins `solc_version = 0.8.28`, `evm_version = cancun`, `optimizer = true` /
`optimizer_runs = 200`, `bytecode_hash = none`, `cbor_metadata = false` — the runtime bytecode is
reproducible byte-for-byte from any forge of that solc version, which is what lets
`contracts/exit-portal/RomeExitPortal.runtime.hex` be committed and diff-checked in CI rather than
rebuilt fresh at deploy time.

## Regenerating fixtures

`script/fixtures.sh` regenerates `fixtures/exit/*` and `contracts/exit-portal/RomeExitPortal.runtime.hex`. Run
it the same way, inside the pinned image, from the repository root:

```
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$PWD":/work -w /work/contracts/exit-portal \
  ghcr.io/foundry-rs/foundry:v1.3.6 "bash script/fixtures.sh"
```

See `fixtures/exit/README.md` for what each file is and the one field (`anvil_state_root.json`'s
`block_hash`) that does not reproduce byte-for-byte, and why.

## Greenfield predeploy

`./rollup init` in [`deploy/rollup`](../../deploy/rollup) renders a new chain's genesis from
`deploy/rollup/genesis.json.template`, with `RomeExitPortal`'s runtime bytecode, read from the committed
`RomeExitPortal.runtime.hex`, pre-deployed at `0x4200000000000000000000000000000000000016` with a balance of
0. It refuses by name (`ExitPortalRuntimeMissing`) if that file is absent, empty or not valid hex.

## An existing chain

A chain's genesis cannot change once the chain is running: a reth node refuses to start on a genesis whose
hash differs from its own chain state. A chain started without the predeploy gets the portal as an ordinary
contract deployment instead (`script/DeployRomeExitPortal.s.sol`, which reads the deployer key from the
`PRIVATE_KEY` environment variable), at whatever address that deployment produces. The chain's exit
configuration then names that address, and the exit prover's `portal_from_block` is set to the block the
portal was deployed in.
