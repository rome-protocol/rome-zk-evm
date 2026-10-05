# ZisK releases and verification keys

Each chain has its own verification key registry. An entry holds a `programVK`, a public values layout, an activation slot, and a `scheme` byte. The byte names the ZisK release used to build that key. It is not supplied by the proof.

| `scheme` | ZisK release | Pinned recursion root | Status |
| --- | --- | --- | --- |
| `1` | `1.2.0-alpha` | `0x564c2b1bcbd5932c81cfad1fa786a98372eb3d6495257c2d944544334f84382f` | Withdrawn. Existing entries can be retired, but their proofs are refused. |
| `2` | `1.3.1-alpha` | `0xc3f12b9f8707c6a1e96df2bf6702c2ebdfbafedabeac654644a380befe091ac4` | Open. New entries and proofs are accepted. |

Numbers are never reused. Entries written before releases had names use `1`.

Settlement links Veritas and keeps one row for each release. A row holds that release's PLONK verifying key, its status, and its pinned `rootCVadcopFinal` recursion root. `PostRootProved` finds the active registry entry by the proof's `programVK`, then uses the entry's release to select the row. It refuses a proof with a different recursion root before checking the public values or the pairing. Veritas checks the same root again. A withdrawn release has no verifying key in the production build.

## Moving a chain to a new release

1. Install the new ZisK toolchain and proving keys beside the old install. Keep the old install available until the move is complete.
2. Set `ROME_ZK_TAG=v0.3.0`, `ROME_ZK_GUEST_TAG=v0.3.0`, and `ZISK_RELEASE=1.3.1-alpha` in the rollup's `.env`. Run `./rollup guest-build` to rebuild the chain's guest. The build writes an ELF and `vkey.json` for that release. The prover needs the matching `ZISK_HOME`, ELF, and `vkey.json`.
3. Send Rome the chain's genesis, guest tag, ELF hash, and `programVK`. Rome uses `rome-zk-ops vkey register --zisk 1.3.1-alpha --evm-tag v0.3.0` to rebuild and check the guest, then registers the key with a future activation slot. The command also needs the chain ID, genesis, key hashes, proving keys, signing keys, and activation slot. If the chain has already posted a root, it needs the previous guest and release as an anchor. Registration is a dry run until Rome adds `--confirm`.
4. Use `rome-zk-ops vkey show --settlement <program> --chain-id <id>` to read the new entry and its activation slot. Switch the prover to the new install and key at that slot. An entry under a closing release keeps verifying until Rome retires it. An entry under a withdrawn release, such as 1.2.0-alpha, verifies nothing, so a chain still on 1.2.0-alpha cannot post a root until its 1.3.1-alpha entry is active.
5. Rome retires the old entry after the switch. Retirement takes effect when its transaction lands. A retired entry cannot be reactivated while it remains in the registry.

The registry has four entry slots. Adding a key does not replace another live key. A future entry cannot verify proofs before its activation slot. The prover checks that its ZisK binary and proving key match the release in `vkey.json`, then verifies each proof locally before posting it.

## Closing or withdrawing a release

Rome's support policy keeps at most two releases open at once. When Rome announces that a release is closing, chains on devnet and testnet have 30 days to move. A closing release accepts no new registry entries, but existing entries can still verify proofs. After the window, Rome runs `rome-zk-ops vkey retire-version --settlement <program> --zisk <release> --registry-keypair <file> --payer-keypair <file>` to find and retire its remaining entries, then withdraws the release in a settlement upgrade. The command lists the entries without sending transactions unless Rome adds `--confirm`.

Rome can retire entries at once when a release must be withdrawn for a security issue. A withdrawn release accepts neither new entries nor proofs. Retirement remains available so its old entries can be cleared. These release states are set in the program; the 30-day move window is Rome's support policy, not an on-chain timer.

The command syntax and required flags are in the [operator CLI](../crates/rome-zk-ops/README.md). The [trust model](TRUST-MODEL.md) explains who controls the registry.
