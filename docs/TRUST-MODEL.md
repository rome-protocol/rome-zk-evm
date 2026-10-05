# Trust model on Solana devnet

This page describes Rome's shared programs on public Solana devnet. The programs are upgradeable.

## Rome's keys

Rome holds the program upgrade authority, `2H8bMM3AUTU6RZap3zyFU5coNUazo6z5Xg7ighfCTJ3q`, a single-signature
key rather than a multisig. It can upgrade zk-inbox, zk-settlement, veritas and zk-bridge.
An upgrade takes effect at once, with no notice period, and can change any rule described here.
The same address is the current treasury. It receives settlement fees and SOL from reclaimed chains.
Only the settlement upgrade authority can initialize the global configuration and choose its first
registry authority and treasury. That initialization can run only once.

Rome's registry authority is `7CsvZgCML2C7i4f1Qd6Au3cxonB4c8uAWn3NmMmU9MDk`, also a single-signature key. Its
settings change in the slot its transactions land. Verification keys can be set to activate later,
and the authority handover needs acceptance by the new key.
It can turn permissionless chain registration on or off. It can set the deposit for new registrations,
the default settlement fees for new chains, and the treasury address. It can change the fees for an
existing chain at any time, with no separate protocol cap. The chain authority pays this fee each time it posts a
root, so a fee it cannot pay stops posting.

It can change the global reclaim window, including for chains already registered, but not below
216,000 slots. It can change a chain's allowed timestamp drift: the most a block's timestamp may run
ahead of the Solana time at which its batch was opened. On a permissionless chain every proof must carry
the chain's current value, so a change applies to every batch not yet posted.

It can allow or revoke reserved chain IDs and migrate older chains' configuration. Chain IDs below
2^32 are reserved: registering one needs the registry authority's signature and its allowance of that
ID, and such a chain may post
unproved roots that can be finalized after its challenge window. It can propose a new registry authority;
that new key must accept before the change takes effect.

## Verification keys

A valid proof counts only if its key is registered for that chain. Whoever controls key registration
therefore controls which proving program's proofs can make that chain's roots final, subject to the
other checks in settlement.

The registry authority can register a verification key for a chain, retire it, or change the slot from
which it can be used, including moving an active key's slot into the future. Each change takes effect
in the slot its transaction lands, with no notice period: a new key's activation slot may be that same slot. A key can verify only from its recorded activation slot.
Retirement takes effect when its transaction lands. The slot is stored in the chain's on-chain registry, so a
change that takes effect later can be seen before it does. If no key is active, the chain cannot
post another proved root. Roots already final stay final. A retired key cannot be reactivated while
its entry remains in the registry. After that slot is reused, the registry authority can register
the same key again if a slot is available. A chain holds at most four keys at a time.

Each registry entry's scheme byte names a ZisK release: `1` is withdrawn 1.2.0-alpha and `2` is
open 1.3.1-alpha. Settlement links Veritas, which has one row per release with its PLONK verifying
key, status and pinned `rootCVadcopFinal` recursion root. The withdrawn row has no key in the
production build. `PostRootProved` takes the release from
the active entry matching the proof's programVK. It refuses a withdrawn release and a proof whose
recursion root differs from that release's pinned value before the pairing. Rome can change the
release table only by upgrading settlement; the registry authority chooses which available keys
are active for a chain. [ZisK releases](ZISK-RELEASES.md) lists the current rows.

A permissionless chain starts with no registered key and cannot choose one during registration. Until
Rome registers a key, the chain cannot post a root. If Rome has not registered one when the reclaim
window ends, anyone can reclaim the chain and its deposit goes to the treasury. Rome sets both the
window, which can be as short as 216,000 slots (about one day), and the treasury address.

## Your chain's key

In the devnet setup, your Solana payer key also becomes the chain authority recorded in the root
account. That key can open and finalize inbox batches. It can open, write and seal their chunks,
post proved roots and initialize the batch cursor. It can close eligible batch, chunk and
pending-root accounts to recover their rent.
It can also abandon an inbox batch that has not been finalized. Batch numbers are never reused and
settlement cannot skip one, so abandoning a batch that has not yet been posted stops the chain from
posting any later root. The node does not send AbandonBatch. After a restart, it finishes a
half-written batch under the same number if it can reconstruct it. Otherwise it stops without
abandoning the batch.

Inbox batch, chunk and cursor addresses include both the settlement program and the chain ID.
Only the authority recorded by that settlement program can open your batches or write their chunks.
Another operator using the same chain ID under a different settlement program cannot occupy your
inbox accounts.

At registration the chain authority also fixes, for good, the inbox program that settlement reads the
chain's batches from and the chain's challenge window. The devnet setup uses Rome's zk-inbox and a
window of 172,800 slots.

The chain authority can propose the chain's exit portal, bridge program, withdrawal cap and poster
bond. An exit change takes effect no sooner than one challenge window after it is proposed, and anyone
can activate it then. The window is whatever value the chain authority chose at registration, so check
it before relying on that delay. Registration accepts a window of zero, and a chain registered that way
can never propose an exit change. Only one exit change can be pending, and it cannot be
withdrawn. The poster bond is recorded but not collected today.

For a permissionless chain, the chain authority can create its deposit queue after the first
proved batch has posted. It chooses the queue's initial inclusion deadline, per-batch and per-block
limits, minimum amount, fee and fee recipient. It can propose new values at an activation slot from
one to two challenge windows after the proposal; anyone can activate them once that slot arrives.
A new proposal replaces an earlier pending one. The deadline must be between one and 24 hours, the per-batch limit at most 256, the per-block limit between one and the per-batch limit, the minimum at least one base unit and the fee at most 0.01 SOL. The fee recipient must hold at least the rent-exempt minimum for an empty account and cannot be executable or a sysvar.
The challenge window is the value the authority fixed at registration. Rome's program upgrade
authority can change the program rules and these bounds by upgrading the programs.

The deposit is paid from the payer at registration. Once a refund is due, anyone can request it,
but settlement pays it only to the recorded chain authority. There is no instruction to rotate
that authority. Protect this key: losing it stops the signed posting and configuration actions.

## Rules enforced by the current programs

A permissionless chain cannot use the unproved root-posting path. To post a proved root, its
authority must supply a finalized inbox batch and a proof that settlement checks with the veritas
verifier built into it (settlement does not call the deployed veritas program), against an active
registered key. Settlement also checks the proof's public values against the chain ID, block range,
state root, the predecessor's last block hash, the inbox commitment (which covers the batch number),
the time the batch was opened, the chain's drift value and the gas used. A permissionless chain's
registration-time genesis is not a final root. Its first final root requires a proved batch.

No one can reclaim a chain after it has posted a root: reclaim requires the root's posted-batch
head to remain zero. There is no instruction to edit a registered chain's ID or its starting
genesis values. Later proved roots advance the chain state.

## Reclaim and the registration deposit

The recorded devnet reclaim window is 6,480,000 slots, about 30 days at 400 ms per slot.
Rome can change this global setting down to the 216,000-slot floor. Reclaim checks the value
that is current when it runs, measured from the chain's registration slot. If no root has been
posted and the window has elapsed, anyone can reclaim the chain. This closes its settlement
root, verifier registry and chain configuration accounts. Their SOL, including any unrefunded
deposit and account rent, goes to whatever treasury address the registry authority has set when
the reclaim runs.

The registration deposit becomes refundable after the first final root or after ten posted
roots. Anyone can trigger the refund, but it always goes to the chain authority. A refunded
deposit cannot be refunded again.

## What your users trust you to do

The sequencer chooses transaction order and can refuse transactions. Deposits wait in the bridge's deposit queue under the inclusion rule below, but there is no general forced-inclusion lane for EVM transactions. Users depend on the operator to keep the sequencer running, publish
batches to Solana, generate proofs and post proved roots. A valid proof does not post itself:
the chain authority must sign the root-posting transaction.

A depositor can call `Deposit` after the permissionless chain's first proved root and queue setup,
when its active exit configuration names Rome's zk-bridge. The call locks the chain's vault mint, pays
the queue fee and appends an ordered record for an L2
recipient. The operator chooses when to open and finalize batches and must keep proving them for
the credit to become final. On a batch that finalizes, the next waiting deposit may be left out
only while it is inside the queue's deadline, or while the batch takes at least its per-block
limit. The age check uses the batch's opening time, but that reading can be held for at most 24
hours after the batch opens. The program uses the later of opening time and finalization time
minus 24 hours. A batch that takes fewer than the per-block limit cannot leave the next deposit
out past its deadline plus 24 hours. This limits batches that finalize. It does not
make an operator post. Anyone can close a deposit record after its crediting batch's root is final
and `CloseBatch` advances the deposit cursor. `CloseDeposit` returns only the record rent to the
recorded depositor. It does not return the tokens locked in the vault.

Withdrawals need a final root and an activated exit configuration with a portal, a nonzero cap and
a bridge program. Anyone can prove an exit from a final root. Only the bridge program named in the
chain's exit configuration can consume its exit record for release.

Rome's shared zk-bridge program is deployed on devnet at
`27TbMDUyVynpFpqeKygpUMcDzWKHfW4k9aRN5yCysLEQ`. Each chain has its own vault, keyed by its
settlement program and chain ID. Only that chain's authority can create its vault. Anyone can fund it
or call ReleaseExit for a valid exit. A release checks the proved exit record from the vault's
settlement program and the configured bridge. It checks the refund address and the recipient's
associated token account.
It consumes the record and transfers the tokens in one transaction, so the same exit cannot pay twice.
The recipient's token account must exist; the bridge does not create it.
Without a program upgrade, Rome's keys cannot create a vault for a chain whose authority they do not
hold, change its mint, choose where its payouts go or withdraw from it without a proved exit.

The vault holds one SPL Token mint (not Token-2022) chosen by the chain authority. The program accepts
a mint with up to 18 decimals; it does not require wrapped SOL. Only the chain's native EVM asset can exit. The
bridge converts its 18-decimal amount into the vault mint's units, rounding down. A deposit queue needs a mint with at most nine decimals.

Deposits into a permissionless rollup are available on devnet after its first proved root and
deposit-queue setup. A new chain's genesis has no spendable
balances unless its `chain.toml` declares one backed balance. The operator must lock the matching
amount in the chain's vault before Rome registers its verification key. The backed balance is written
in lamports of wrapped SOL, so this needs a wrapped SOL vault. The node
does not lock it, and settlement does not check the backing on chain. The registration deposit above
is only a bond for creating a chain.

The rules above are in [chain registration](../programs/zk-settlement/src/chain.rs), [governance](../programs/zk-settlement/src/governance.rs), [root settlement](../programs/zk-settlement/src/settle.rs), [ZisK releases](../programs/veritas/src/versions.rs), [exits](../programs/zk-settlement/src/exit.rs), the [inbox](../programs/zk-inbox/src/batch.rs), the [deposit queue](../programs/zk-bridge/src/deposit_queue.rs), [deposits](../programs/zk-bridge/src/deposit.rs), [closing deposit records](../programs/zk-bridge/src/close_deposit.rs) and the [vault](../programs/zk-bridge/src/release.rs). The program addresses are in [`programs.devnet.json`](../deploy/rollup/programs.devnet.json), and the current settlement settings are in the [devnet guide](RUN-ON-DEVNET.md).

## Prover host setup

The [prover host setup script](../deploy/rollup/prover/setup-prover-host.sh) installs the
ZisK 1.3.1-alpha GPU build and proving keys, sets the locked-memory limit and prepares Docker
with the GPU runtime. It checks the installer hash before running it, both key archive hashes
before unpacking them, and the hashes of `cargo-zisk` and `cargo-zisk-dev` after installation
before the script runs either binary. Its final checks cover the GPU, driver, proving keys,
locked-memory limit and container runtime.

The installer downloads and unpacks the release's binary archive and runs its toolchain installer
as root before this script checks either installed binary. The script does not hash-check that
archive or its other contents, including workers, libraries and sources. A successful setup
check therefore does not authenticate every executable or source file in the ZisK install.
