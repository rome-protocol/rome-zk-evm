# Exit fixtures — provenance

**Producer:** `ghcr.io/foundry-rs/foundry:v1.3.6`, digest
`sha256:7026b071fb16606a14426acc0a0e2cd57f80b96abd7e7d371fef8dafe1712cdf`. Inside it:
`forge --version` (`anvil`/`cast --version` agree) prints `1.3.6-v1.3.6, Commit SHA:
d2415887096b10226d13af9240b5bef5e6b0d815` — the commit id here (`d2415887`) is that SHA's own prefix,
read directly off the image's own `--version` output, not a release-notes guess. Regenerated with `make exit-fixtures` — which
syncs this worktree to a build machine and runs `contracts/exit-portal/script/fixtures.sh` **inside that
image**, then pulls the results back. A local forge/anvil (whatever version is on `PATH`)
is never the producer of anything under this directory or of `contracts/exit-portal/RomeExitPortal.runtime.hex`.

## What each file is

- **`message_hash.json`** — the one `initiateExit` call this fixture set is built from: `nonce` (0, the
  portal's first exit), `l2_sender`/`sol_recipient`/`asset`/`amount_wei` (the message fields),
  `preimage_hex` (the 160-byte `abi.encode(uint256,address,bytes32,address,uint256)` preimage —
  `layouts::exit` pins this exact byte layout), `message_hash` (`keccak256(preimage_hex)`),
  `storage_slot` (`keccak256(abi.encode(message_hash, uint256(0)))` — `sentMessages` is declared at
  storage slot 0), and `portal` (the deployed contract's address on the anvil instance this run started).
- **`anvil_getProof.json`** — `eth_getProof(portal, [storage_slot], "latest")`, `cast rpc`'s own
  unwrapped `result` value, verbatim: the account proof of `portal` and the storage proof of
  `sentMessages[message_hash]` (`storageProof[0].value == "0x1"` — an **inclusion** proof). This is
  what `rome-zk-mpt::verify_account`/`verify_storage` prove against.
- **`anvil_getProof_unsent.json`** — the same call at the storage slot of a message that was **never**
  sent (same shape, `nonce = 999`): `storageProof[0].value == "0x0"`. The exclusion counter-fixture —
  `rome-zk-mpt` must refuse this as an inclusion proof, never accept an all-zero leaf as "sent".
- **`anvil_state_root.json`** — `{number, block_hash, state_root}` from `eth_getBlockByNumber("latest")`
  at the point the two proofs above were captured (see "Determinism" below for `block_hash`).
- **`contracts/exit-portal/RomeExitPortal.runtime.hex`** — `forge inspect RomeExitPortal deployedBytecode`
  (not under `fixtures/`; lives beside the contract it describes). A pure function of the Solidity
  source plus `foundry.toml`'s pinned `solc_version`/`evm_version`/optimizer settings — independent of
  anvil, computed before anvil even starts.
- **`tiber_eoa_getProof.json`** + **`tiber_eoa_block.json`** — **not produced by this script**: copied
  verbatim (the JSON-RPC `result` object only, envelope stripped) from a read-only capture of the
  real Tiber verifier (stock `reth v2.5.2`, chain 200101) — `eth_getProof(0xCec1437293cC1596
  4200D9E7667f7d29Ee92E11c, [], "0x32e0e")` and the matching `eth_getBlockByNumber`. The account is an EOA
  the dev key has never sent from: **one** account node (120 B — the whole trie is a single account, so
  the leaf is the root), balance `0x33b2e3c9fd0803ce8000000`, nonce 0, `storageHash`/`codeHash` equal to
  the well-known empty-trie-root and empty-code-hash constants (no storage, no code). `storageProof` is
  empty (no storage slot was queried) — `rome-zk-mpt::tiber_eoa_account_proof_verifies` proves only
  the account, not any storage slot, against this fixture.
- **`anvil_getProof_multi.json`** + **`anvil_state_root_multi.json`** + **`message_hash_multi.json`** — a
  second scenario on a **second, independent anvil instance** (port 8546, started and recorded only after
  the first scenario's own files above are already written, so they stay byte-identical no matter what
  this section does): the same portal contract, but after **20** `initiateExit` calls — nonces 0..19,
  amounts `(nonce+1)` ether, recipients alternating `0x01×32`/`0x02×32`. `message_hash_multi.json` records
  every one of the 20 messages' `nonce`/`sender`/`recipient`/`asset`/`amount`/`hash`/`storage_slot`, plus
  which two storage slots `anvil_getProof_multi.json` actually proves: exit 7's slot (an **inclusion**,
  `storageProof[0].value == "0x1"`) and a never-sent nonce's slot (an **exclusion**,
  `storageProof[1].value == "0x0"`), both in the *same* `eth_getProof` call against the real 20-key trie.
  This is the fixture `rome-zk-mpt`'s `multi_fixture_node_kinds_are_measured_not_assumed` reads to classify every
  node's real kind (see that test's own doc for what it actually found).
- **`anvil_getProof_ext.json`** + **`anvil_state_root_ext.json`** + **`message_hash_ext.json`** — a
  third scenario on a **third, independent anvil instance** (port 8547, recorded only after the two
  scenarios above are already written): the same portal contract after **500** `initiateExit` calls
  (nonces 0..499, amounts a flat 1 ether, recipients alternating `0x01×32`/`0x02×32` by parity) — deep
  enough to reliably produce real **Extension** nodes in the storage trie, unlike the 20-key `multi`
  scenario, which was measured as having none. `anvil_getProof_ext.json` carries two storage
  proofs in one `eth_getProof` call: nonce 21's slot (an **inclusion**, `storageProof[0].value ==
  "0x1"`, whose proof *traverses* a real Extension node before reaching its leaf) and nonce 516's slot
  — never sent — (an **exclusion**, `storageProof[1].value == "0x0"`, whose proof *diverges at* a real
  Extension node). `message_hash_ext.json` records both messages' full fields plus their derived
  hashes/slots. Nonce 21 and 516 were found by an **off-line grind** (not reproduced at script run time
  — the same pattern `proof_for_other_address_is_refused` already uses for its address grind): every
  one of the 500 sent slots' storage proofs, plus 250 never-sent candidate slots (nonces 500..749, same
  construction), were fetched in one batched `eth_getProof` call against this exact scenario and every
  node classified by item count (17 = branch; 2 = leaf when the hex-prefix terminator flag is set, else
  extension) — nonce 21 is the first sent slot whose proof contains an Extension node, nonce 516 the
  first never-sent slot whose proof's *last* node is an Extension node. Re-run twice back to back on
  fresh anvil instances during the grind: the portal address, the account proof, and both storage
  proofs (bytes, not just shape) were identical. This is the fixture `rome-zk-mpt`'s `ext_fixture_has_an_extension_node`
  / `inclusion_through_extension_verifies` / `exclusion_diverging_inside_extension_is_absent` read.

- **`anvil_exit_logs.json`** — `eth_getLogs({address: portal, fromBlock: "0x0", toBlock:
  "latest"})` against the BASE scenario's anvil instance, `cast rpc`'s own unwrapped `result` array,
  verbatim: the one `ExitInitiated` log the base scenario's single `initiateExit` call emits. This is
  what `rome-zk-exit-prover`'s log decoder reads — `topics[1]` (the event's one indexed field) must equal
  `message_hash.json`'s own `message_hash`; `data` is the 160-byte ABI-encoded remaining fields
  (`nonce`, `l2Sender`, `solRecipient`, `asset`, `amount`) in event-declaration order, matching
  `layouts::exit::ExitMessage::message_preimage`'s own field order.
- **`anvil_block_by_number.json`** — the SAME `eth_getBlockByNumber("latest", false)` call
  `anvil_state_root.json`'s `number`/`block_hash`/`state_root` fields were extracted from, written
  UNEXTRACTED (the whole `result` object) — `rome-zk-exit-prover`'s own `eth_getBlockByNumber` decoder
  reads this shape directly (a real verifier node's response carries every other header field too; a
  decoder that only ever saw three hand-picked keys would never catch a real field-name mismatch).

## Cross-checks the generating script asserts before writing anything

- `eth_getStorageAt(portal, storage_slot) == 0x…01` (matches `anvil_getProof.json`'s own
  `storageProof[0].value == "0x1"`).
- `anvil_getProof_unsent.json`'s `storageProof[0].value == "0x0"`.
- Node counts + max node length (`rome-zk-mpt`'s bounds: ≤ 64 nodes, ≤ 532 B/node), printed by the script on every
  run: this fixture's `accountProof` carries **3** nodes, `storageProof[0].proof` carries **2** nodes,
  the largest node is **308 B** — well inside both bounds.
- The multi scenario: `eth_getStorageAt` for both exit 7's slot (`...01`) and the never-sent slot (`0`);
  `anvil_getProof_multi.json`'s two `storageProof` entries' `value` fields (`0x1` then `0x0`, matching the
  request order). Measured node counts (a second anvil instance, so these can differ run to run only in
  which specific keys land where — the counts themselves are stable for this fixed 20-message scenario):
  **3** account nodes, **5** storage nodes total across the two keyed proofs (3 shared + a 1-node leaf for
  exit 7 + a 1-node branch divergence for the never-sent key), max **308 B**.
- The ext scenario: `eth_getStorageAt` for both nonce 21's slot (`...01`) and nonce 516's slot (`0`);
  `anvil_getProof_ext.json`'s two `storageProof` entries' `value` fields (`0x1` then `0x0`). Measured
  node counts: **3** account nodes (max 308 B), **8** storage nodes total across the two keyed proofs
  (5 for the inclusion proof — Branch, Branch, Extension, Branch, Leaf — + 3 for the exclusion proof —
  Branch, Branch, Extension), max **532 B** (a near-full 17-item branch — within, but at, the bound
  `rome-zk-mpt::MAX_NODE_BYTES` allows).

## Determinism

`anvil --gas-price 0 --base-fee 0 --timestamp 1700000000` (a fixed genesis timestamp) means every
account-state-derived byte reproduces exactly across regenerations: `message_hash.json`,
`anvil_getProof.json`, `anvil_getProof_unsent.json`, `RomeExitPortal.runtime.hex`, and
`anvil_state_root.json`'s own `number`/`state_root` fields — verified by running `make exit-fixtures`
twice back to back and diffing.

**Three fields do not reproduce: `anvil_state_root.json`'s, `anvil_state_root_multi.json`'s and
`anvil_state_root_ext.json`'s own `block_hash`** (and, by the same cause, `anvil_block_by_number.json`'s
own `hash`/`mixHash`/`parentHash` and `anvil_exit_logs.json`'s own `blockHash`/`transactionHash`). Anvil 1.3.6 assigns each block's own `mixHash`
(post-Merge `prevRandao`) from its own internal randomness on every run — there is no CLI flag in this
version to pin it (checked against `anvil --help`; `--timestamp`, `--gas-price` and `--base-fee` cover
every other source of run-to-run variance), and the second (multi-exit) and third (ext) anvil instances
have the identical limitation. Two side-by-side runs
of this script produce byte-identical `stateRoot`, `transactionsRoot`, `receiptsRoot`, `gasUsed` and
transaction hash for the same block, differing **only** in `mixHash` (and, downstream of it,
`parentHash`/`hash`). Since `state_root` — not `block_hash` — is the value the exit-proof pipeline
actually proves against (design: the MPT proof targets `state_root`; `block_hash` here is contextual, not
load-bearing), CI's drift check (`contracts` job) diffs every other byte of every file here and checks
all three `anvil_state_root*.json` files' `number`/`state_root` fields explicitly, rather than raw-diffing
any of them whole.
