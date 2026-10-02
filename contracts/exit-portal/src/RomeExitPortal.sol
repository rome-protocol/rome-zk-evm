// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// @title RomeExitPortal
/// @notice L2 predeploy: the sole entry point for an L2-to-Solana native-asset exit. `initiateExit`
/// records a message hash in contract storage (an OP-Stack-`L2ToL1MessagePasser`-shaped sentMessages
/// mapping, but smaller: no gas limit, no target, no calldata — a fixed-shape native-asset transfer
/// only) and emits it as an event. The ETH stays here: there is no withdraw function, so a proved exit
/// releases the Solana-side twin from an off-chain vault rather than reclaiming this contract's balance
/// — the L2 balance is burned by construction. `programs/zk-settlement`'s `ProveExit` proves
/// inclusion of `sentMessages[messageHash] == true` at storage slot 0 against a finalized `state_root`
/// via an Ethereum Merkle-Patricia proof (`eth_getProof`); no code here ever runs again after
/// `initiateExit` returns — verification and release are entirely off-L2.
///
/// Storage layout is part of the contract: `sentMessages` MUST be slot 0 (the MPT proof's target slot
/// is derived on-chain as `keccak256(abi.encode(messageHash, uint256(0)))`) and `messageNonce` slot 1.
/// Declare nothing before them.
contract RomeExitPortal {
    /// @dev slot 0 — the MPT proof target. `sentMessages[messageHash]` at storage slot
    /// `keccak256(abi.encode(messageHash, uint256(0)))`.
    mapping(bytes32 => bool) public sentMessages;

    /// @dev slot 1 — monotonic per-portal exit counter, the message preimage's first field.
    uint256 public messageNonce;

    /// @notice Emitted once per successful `initiateExit`. `messageHash` is the only indexed field.
    event ExitInitiated(
        bytes32 indexed messageHash,
        uint256 nonce,
        address l2Sender,
        bytes32 solRecipient,
        address asset,
        uint256 amount
    );

    error ZeroAmount();
    error AmountOverflow();
    error ZeroRecipient();

    /// @notice Burns `msg.value` on L2 and records an exit message for later Solana-side release.
    /// @param solRecipient the Solana account (32 bytes) that receives the released twin asset.
    /// @return messageHash keccak256 of the 160-byte preimage
    /// `abi.encode(uint256(nonce), msg.sender, solRecipient, address(0), uint256(msg.value))` — the
    /// `layouts::exit` module pins this exact ABI layout against this contract's own fixture.
    function initiateExit(bytes32 solRecipient) external payable returns (bytes32 messageHash) {
        // Cheapest checks first.
        if (msg.value == 0) revert ZeroAmount();
        if (msg.value > type(uint128).max) revert AmountOverflow();
        if (solRecipient == bytes32(0)) revert ZeroRecipient();

        uint256 nonce = messageNonce++;
        messageHash = keccak256(
            abi.encode(uint256(nonce), msg.sender, solRecipient, address(0), uint256(msg.value))
        );
        sentMessages[messageHash] = true;

        emit ExitInitiated(messageHash, nonce, msg.sender, solRecipient, address(0), msg.value);
    }

    // No receive()/fallback(): a plain transfer to this contract reverts. ETH only ever enters via
    // initiateExit's payable call, which always records a message for it.
}
