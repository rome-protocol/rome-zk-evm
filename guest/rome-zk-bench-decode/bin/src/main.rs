//! Measurement guest: ZisK entrypoint over `rome-zk-bench-decode-lib::run`. **Not the production guest** — no
//! witnessed block execution, no public-values-v2 commit; this measures the guest's two open unknowns (ruzstd
//! decode share, single-execution ceiling) before that guest is built.
#![no_main]

ziskos::entrypoint!(main);

fn main() {
    let input: rome_zk_bench_decode_lib::BenchInput = ziskos::io::read();
    let (block_count, acc) = rome_zk_bench_decode_lib::run(&input);
    // Commit acc and block_count separately, not mixed into one hash: this way `ziskemu`'s
    // committed output carries `acc` in the clear, so it can be compared byte-for-byte against a real
    // batch's on-chain accumulator value (guest README).
    ziskos::io::commit_slice(&acc);
    ziskos::io::commit_slice(&block_count.to_le_bytes());
}
