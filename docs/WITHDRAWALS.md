# Withdrawing from your chain to Solana

A withdrawal moves coins from your chain back to Solana. The user burns them on the chain, the exit prover proves
the burn to the settlement program on Solana, and the chain's vault pays out the matching amount of wrapped SOL.
This page covers what a user does, what the operator runs, how long it takes and what can go wrong.

Withdrawals work for the chain's native coin only. One gwei on the chain is one lamport of wrapped SOL, so burning
1 coin (10^18 wei) pays out 1 SOL, and the payout is rounded down to a whole lamport.

## What a user does

A user starts a withdrawal with one transaction to the exit portal, a contract that is part of every chain's
genesis at `0x4200000000000000000000000000000000000016`. Any EVM wallet can send it. The call is
`initiateExit(bytes32 solRecipient)` and it is payable: the value you send is the amount that leaves the chain.

`solRecipient` is the Solana address that should receive the funds, written as 32 bytes. Turn a Solana address
into that form with Python, which `./rollup` needs anyway:

```
python3 -c 'import sys
a = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
n = 0
for c in sys.argv[1]: n = n * 58 + a.index(c)
print("0x" + n.to_bytes(32, "big").hex())' YOUR_SOLANA_ADDRESS
```

Then send the transaction. This example withdraws 0.5 coins with Foundry's `cast`:

```
cast send 0x4200000000000000000000000000000000000016 'initiateExit(bytes32)(bytes32)' \
  0xYOUR_RECIPIENT_AS_32_BYTES --value 0.5ether \
  --rpc-url https://YOUR-CHAIN-RPC --private-key "$L2_PRIVATE_KEY"
```

The coins are burned as soon as the transaction lands. Nothing in the portal can give them back, so check the
recipient before you send. The recipient must be a wallet address, not a token account: the payout goes to that
wallet's wrapped SOL token account, and if it does not exist yet, the release creates it. A token account address
as the recipient would send the coins to an account that nobody can spend from.

Keep the message hash. It names your withdrawal from here on. It is the second topic of the `ExitInitiated` event
in the receipt:

```
cast receipt TX_HASH --rpc-url https://YOUR-CHAIN-RPC --json | jq -r '.logs[0].topics[1]'
```

## What the operator runs

Set `EXITS=on` in `.env`, run `./rollup init` and `./rollup up`. That starts the `exit-prover` service from the
node image. It needs no GPU. It reads the chain from your verifier node, reads and writes Solana through
`SOLANA_RPC_URL`, and signs with its own exit payer key, which pays the Solana fees. Keep the payer funded, because
every proved withdrawal costs a transaction fee and the rent of one small account, and every payout costs another fee.

The exit payer is a different key from the batcher's payer on purpose. Anyone can start a withdrawal, even for one
wei, and each one costs the exit payer a fee and some rent. If the batcher shared that key, users could drain the
account that posts your chain's batches. With a key of its own, the worst a flood of tiny withdrawals can do is
empty the exit payer, and the chain keeps running. `./rollup check` fails when the balance gets low.

The exit prover does nothing until the chain's exit configuration is active, and it does not fail while it
waits. Activating it is a one-time step by the chain authority: `./rollup exit-config propose`, wait out the
delay, then `./rollup exit-config activate`. Both are described in the
[devnet guide](RUN-ON-DEVNET.md). Without an active exit configuration, no withdrawal can be proved. The vault must
also hold enough wrapped SOL to cover what users withdraw (`./rollup vault show`, `./rollup vault fund`).

Payouts are automatic. As soon as the exit prover has proved a withdrawal, it sends the release too, and the vault
pays the recipient. The release is open to anyone and has no waiting period, and the program reads the recipient from
the proved record, so nobody who sends the transaction can redirect the money. The exit prover also pays out any proved
withdrawal it finds still open, for example after a restart. The exit prover pays the fees and the rent of the proof
record from its own exit payer, and the release gives that rent back to the payer that proved it.

One case waits. When the recipient has no wrapped SOL token account yet, the release has to create one, and that
costs rent. If the payout is below `release_create_account_min_lamports` (10,000,000 lamports, 0.01 SOL, by default),
the exit prover does not pay it out on its own: anyone could otherwise send many tiny withdrawals to new addresses and
make the exit payer buy a token account for each. Such a withdrawal is counted in `rome_zk_exit_release_waiting`, and
the exit prover logs its message hash. The user or the operator pays it out by hand:

```
./rollup release-exit --message-hash 0xTHE_MESSAGE_HASH --confirm
```

Without `--confirm` the command only prints what it would send. Set `auto_release = false` in the exit prover's config
to leave every payout to this command. Both settings are in `exit-prover.toml`, with their defaults commented out.

`./rollup check` watches all of this when `EXITS=on`:

| Item | Passes when | Fails by name |
| --- | --- | --- |
| service exit-prover | the container is running | `ServiceMissing` |
| exit prover | its metrics answer | `ExitProverUnreachable` |
| exits active | the chain's exit configuration names a portal and a cap, and the exit prover could read it and the root from Solana; a change that is proposed but not yet active is a warning that names its activation slot | `ExitsNotActive`, `ExitConfigUnreadableByExitProver`, `ExitsPendingActivation` (warning) |
| stuck exits | no withdrawal is parked as stuck | `ExitsStuck` |
| exit payer balance | the exit prover's own payer holds at least `EXIT_PAYER_FLOOR_LAMPORTS` (0.1 SOL by default) | `ExitPayerBelowFloor`, `ExitPayerBalanceUnreadable` |
| exit log scan | the exit prover's failed reads of the verifier node (`eth_blockNumber`, `eth_getLogs`) did not grow since the previous check | `ExitLogScanFailing` |

Without `EXITS=on` these items are skipped, with the reason.

## How long it takes, and why

A withdrawal can only be proved against a root that Solana already holds as final. So the time is the time your
chain needs to finish the batch that contains the withdrawal: the batch closes, the batcher posts it, the prover
proves it, and the root is final. That is the same wait as for any other part of your chain's state, and it
depends on `batch_close_after_secs` and on how long your prover takes. After the root is final, the exit prover
proves the withdrawal within a few polls (it polls every two seconds) and pays it out right after, unless it is one
of the small withdrawals that wait for a manual release.

Two more limits can add time:

- **The exit cap.** The settlement program lets only a fixed amount leave in each challenge window (172,800 slots,
  a little under a day), counted in gwei. A withdrawal that does not fit in what is left of the current window waits
  for the next one; the exit prover retries it there by itself. A single withdrawal larger than the whole cap can never
  be proved until the cap is raised.
- **The exit configuration.** Before it is active, nothing is proved at all. A change to it takes effect only after
  a delay of at least one challenge window.

## Where is my withdrawal?

1. **On the chain.** `cast call 0x4200000000000000000000000000000000000016 'sentMessages(bytes32)(bool)' 0xHASH
   --rpc-url https://YOUR-CHAIN-RPC` prints `true` once the withdrawal is recorded.
2. **Proved on Solana.** The operator sees it in `./rollup logs exit-prover` and in the exit prover's metrics:
   `rome_zk_exits_proved_total` counts proved withdrawals, `rome_zk_exit_pending` counts the ones it is still working
   on, `rome_zk_exits_released_total` counts the payouts it sent, `rome_zk_exit_release_waiting` counts the ones left for a
   manual release, and `rome_zk_exit_stuck` counts the ones it parked, by reason. Most parked withdrawals are tried again
   after a later final root; the table below says which ones need a restart instead. A dry run of the release is the quickest
   check for one withdrawal: `./rollup release-exit --message-hash 0xHASH` prints the transaction it would send when the
   withdrawal is proved and waiting for its payout.
3. **Paid.** The recipient's wrapped SOL token account holds the amount. After the release, the same dry run says
   `ExitRecordNotFound`, because the record is closed. If it is still waiting after the proof, look for its message hash
   in `./rollup logs exit-prover`: a small withdrawal to a new recipient waits for `./rollup release-exit`.

## What can go wrong

When the chain's portal refuses a withdrawal, the transaction reverts and nothing is burned:

| Refusal | Meaning | Fix |
| --- | --- | --- |
| `ZeroAmount` | the transaction sent no value | send a value above zero |
| `AmountOverflow` | the value is above 2^128 - 1 wei | send less |
| `ZeroRecipient` | the recipient was all zeros | give the Solana address as 32 bytes |

When a withdrawal is accepted on the chain but not yet proved, the exit prover either waits or parks it. A parked
withdrawal is tried again after a later final root, except `max_send_attempts` and `unsupported_asset`, which stay parked
until the exit prover restarts. It shows up as `ExitsStuck` in `./rollup check`, with its reason, and in `rome_zk_exit_stuck`:

| Reason | Meaning | Fix |
| --- | --- | --- |
| `max_send_attempts` | sending the proof failed several times in a row | check the Solana RPC and the payer balance, then restart the exit prover to try again |
| `max_window_requeues` | the window cap kept refusing it | the cap is too low for the traffic; the chain authority can raise it |
| `exceeds_window_cap` | this one withdrawal is larger than the whole cap | the chain authority raises the cap, or the user withdraws in smaller parts |
| `proof_too_large` | the proof does not fit in one Solana transaction | none yet; try again after the next final root, because a smaller state can shrink the proof |
| `proof_invalid` | the proof failed the exit prover's own check against the final root | the verifier node may be behind or damaged; check `./rollup check` and restart derive |
| `unsupported_asset` | the withdrawal is not for the native coin | not possible; only the native coin can leave |

Solana refuses a proof with one of these names; the exit prover handles each one:

| Refusal | Meaning |
| --- | --- |
| `ExitConfigUnset`, `ExitCapUnset` | exits are not active yet; the exit prover keeps waiting |
| `ExitNotSent` | the final root is older than the withdrawal; it is tried again at the next final root |
| `ExitCapExceeded` | this window is full; it is tried again in the next window |
| `ExitAlreadyProved` | someone proved it already; the exit prover counts it as done |
| `ExitProofInvalid`, `ExitProofTooLarge`, `UnsupportedAsset` | see the parked reasons above |

The release can refuse too:

| Refusal | Meaning | Fix |
| --- | --- | --- |
| `ExitRecordNotFound` | the withdrawal is not proved yet, or it was already paid | wait for the proof, or check the recipient's token account |
| `ExitRecordNotProved` | the record exists but is not in the proved state | nothing to do; it was released already |
| `VaultConfigNotFound` | the chain has no vault | the chain authority runs `./rollup vault init` |
| the transfer fails in the program | the vault holds less than the payout | fund the vault with `./rollup vault fund` |
