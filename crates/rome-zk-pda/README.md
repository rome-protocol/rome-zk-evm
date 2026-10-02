# rome-zk-pda

`create_or_adopt_pda`: the single helper both on-chain programs use to bring a program-derived account
into existence. Every account this system creates (a batch, a chunk, a pending post, a chain's root and
registry, its cursor and configuration) sits at a predictable, program-derived address — and Solana's
system program refuses `create_account` on any address that already holds lamports. This crate closes the
griefing class that follows from that: with sequential, never-reused ids there is no "try a different
address" fallback, so a trivial pre-funding transfer to a predictable future address would otherwise block
that account, and everything sequenced after it, permanently.

## What it guarantees

- If the target address holds no lamports, `create_or_adopt_pda` creates it exactly as a plain
  `create_account` CPI would.
- If it already holds some lamports (the griefing case — nothing but this program's own `invoke_signed`
  can ever make the PDA itself sign, so lamports are the only thing an outsider can have put there
  beforehand), it adopts the account in place instead: transfers in whatever rent is still short, then
  `allocate`s the space and `assign`s it to the owning program, both via `invoke_signed` with the PDA's own
  seeds. Either path ends with the account owned by the target program, sized exactly as requested, ready
  for the caller to write its header into — the caller cannot tell afterward which path ran.
- **This helper does not decide "already truly initialized."** It only ever distinguishes "no lamports
  yet" (create) from "some lamports already there" (adopt) — it never inspects the account's owner or
  data length. A caller that needs an "already initialized" guarantee (a batch cursor that must only ever
  be bootstrapped once, for example) checks that itself, by owner and data length, *before* calling this
  helper — never by relying on `create_account`'s own error, which no longer distinguishes "already
  initialized" from "merely pre-funded."

## Config

None — this is a pure, program-side helper with no runtime configuration.

## How to test

```sh
cargo test -p rome-zk-pda
```

This crate's own unit tests cover the pure rent-shortfall arithmetic
(`rent_shortfall`), which needs no Solana runtime. The create-or-adopt behavior itself — including the
pre-funded-address attack scenario this crate exists to close — is exercised end to end, against the real
programs, in [`programs/zk-inbox`](../../programs/zk-inbox)'s and
[`programs/zk-settlement`](../../programs/zk-settlement)'s own test suites (see each program's README for
the exact command, which needs `cargo build-sbf` run first).

## Security notes

Every account-creation call site in both on-chain programs goes through this one helper — there is no
second, ad hoc `create_account` call anywhere in either program's account-creation path. This is what
makes the pre-funded-PDA attack class unconstructable rather than merely mitigated: it is not that each
site remembers to add a check, it is that there is no other way to create one of these accounts. If a
future call site creates an account any other way, that is the regression to watch for in review.

## Design references

The pre-funded-PDA invariant this crate implements is stated directly in the design's security model
("no instruction may be blocked by lamports sent to a predictable PDA"). See
[`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md#threat-model) for this attack class in the
context of the other attack classes the two programs close or bound.

## Depends on

`solana-program` only (pinned to the workspace's exact version — see the top-level
[`README.md`](../../README.md)). Deliberately not built on [`rome-zk-layouts`](../rome-zk-layouts), even
though that crate also touches account creation indirectly: `rome-zk-layouts` is kept free of any
`solana-program` dependency for its off-chain consumers, and this crate's whole purpose needs
`solana-program`'s CPI helpers directly.
