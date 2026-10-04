//! `pdas`: the addresses of a chain's accounts, as `name=address` lines. Derives them from the program ids and the
//! chain id and reads nothing, so it needs no RPC and no key. `rollup register` records the cursor and root lines.

use crate::error::Report;
use rome_zk_layouts::{chain_config, cursor, global_config, reserved_allow, root};
use solana_program::pubkey::Pubkey;

pub fn run(inbox: &Pubkey, settlement: &Pubkey, chain_id: u64) -> Report {
    let mut report = Report::default();
    report.line(format!("root={}", root::pda(settlement, chain_id).0));
    report.line(format!(
        "cursor={}",
        cursor::pda(inbox, settlement, chain_id).0
    ));
    report.line(format!(
        "global_config={}",
        global_config::pda(settlement).0
    ));
    report.line(format!(
        "reserved_allow={}",
        reserved_allow::pda(settlement, chain_id).0
    ));
    report.line(format!(
        "chain_config={}",
        chain_config::pda(settlement, chain_id).0
    ));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::fake::*;

    #[test]
    fn prints_the_five_addresses_the_layouts_derive() {
        let r = run(&inbox(), &program(), 77);
        let t = r.lines.join("\n");
        assert_eq!(r.lines.len(), 5, "{t}");
        assert!(
            t.contains(&format!("root={}", root::pda(&program(), 77).0)),
            "{t}"
        );
        assert!(
            t.contains(&format!(
                "cursor={}",
                cursor::pda(&inbox(), &program(), 77).0
            )),
            "{t}"
        );
        assert!(!r.sent());
    }
}
