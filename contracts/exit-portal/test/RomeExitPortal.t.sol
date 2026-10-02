// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {RomeExitPortal} from "../src/RomeExitPortal.sol";

/// @dev Minimal cheatcode surface — no forge-std dependency (no submodule / network fetch in CI).
/// Bound to the well-known `hevm cheat code` address the same way forge-std's own `Vm` does.
interface Vm {
    function deal(address who, uint256 newBalance) external;
    function expectRevert(bytes4 revertSelector) external;
    function load(address target, bytes32 slot) external view returns (bytes32);
    function recordLogs() external;

    struct Log {
        bytes32[] topics;
        bytes data;
        address emitter;
    }

    function getRecordedLogs() external returns (Log[] memory);
}

/// @dev No forge-std `Test` base either — plain `require`s. A `test*`-prefixed function that reverts
/// (an unmet require, or an unfulfilled `expectRevert`) is forge's own definition of a failing test.
contract RomeExitPortalTest {
    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    RomeExitPortal portal;
    bytes32 constant RECIPIENT = bytes32(uint256(1));

    function setUp() public {
        portal = new RomeExitPortal();
        vm.deal(address(this), 10_000 ether);
    }

    function test_initiateExit_setsSentMessageAndIncrementsNonce() public {
        uint256 amount = 1 ether;
        bytes32 hash = portal.initiateExit{value: amount}(RECIPIENT);

        require(portal.sentMessages(hash), "sentMessages not set");
        require(portal.messageNonce() == 1, "messageNonce did not increment to 1");

        bytes32 hash2 = portal.initiateExit{value: amount}(RECIPIENT);
        require(hash2 != hash, "second exit's hash must differ (nonce advanced)");
        require(portal.messageNonce() == 2, "messageNonce did not increment to 2");
    }

    function test_initiateExit_overUint128Reverts() public {
        uint256 amount = uint256(type(uint128).max) + 1;
        vm.deal(address(this), amount);
        vm.expectRevert(RomeExitPortal.AmountOverflow.selector);
        portal.initiateExit{value: amount}(RECIPIENT);
    }

    function test_initiateExit_zeroValueReverts() public {
        vm.expectRevert(RomeExitPortal.ZeroAmount.selector);
        portal.initiateExit{value: 0}(RECIPIENT);
    }

    function test_initiateExit_zeroRecipientReverts() public {
        vm.expectRevert(RomeExitPortal.ZeroRecipient.selector);
        portal.initiateExit{value: 1 ether}(bytes32(0));
    }

    /// @dev Recomputes the message hash from the EVENT's own fields (never from what the test itself
    /// passed in) and asserts it equals the emitted, indexed `messageHash` AND that
    /// `sentMessages[hash]` is true — proves the event and the storage write describe the same
    /// preimage, the one `layouts::exit` pins against this contract's own fixture.
    function test_event_fieldsMatchMessageHashPreimage() public {
        vm.recordLogs();
        uint256 amount = 3 ether;
        portal.initiateExit{value: amount}(RECIPIENT);

        Vm.Log[] memory logs = vm.getRecordedLogs();
        require(logs.length == 1, "expected exactly one log");
        Vm.Log memory log = logs[0];
        require(
            log.topics[0] == keccak256("ExitInitiated(bytes32,uint256,address,bytes32,address,uint256)"),
            "unexpected event signature"
        );
        bytes32 emittedHash = log.topics[1];

        (uint256 nonce, address l2Sender, bytes32 solRecipient, address asset, uint256 emittedAmount) =
            abi.decode(log.data, (uint256, address, bytes32, address, uint256));

        bytes32 recomputed = keccak256(abi.encode(nonce, l2Sender, solRecipient, asset, emittedAmount));

        require(recomputed == emittedHash, "recomputed hash != emitted messageHash");
        require(portal.sentMessages(recomputed), "sentMessages[recomputed] not set");
        require(l2Sender == address(this), "l2Sender != caller");
        require(solRecipient == RECIPIENT, "solRecipient mismatch");
        require(asset == address(0), "asset must be address(0)");
        require(emittedAmount == amount, "amount mismatch");
    }

    /// @dev This is the exact storage slot the MPT proof will target on Solana: `sentMessages` MUST be
    /// declared at slot 0 (nothing before it) so `keccak256(abi.encode(hash, uint256(0)))` is the right
    /// derivation off-chain and on-chain alike.
    function test_sentMessages_slotIsZero() public {
        bytes32 hash = portal.initiateExit{value: 1 ether}(RECIPIENT);
        bytes32 slot = keccak256(abi.encode(hash, uint256(0)));
        bytes32 value = vm.load(address(portal), slot);
        require(value == bytes32(uint256(1)), "sentMessages is not at storage slot 0");
    }

    /// @dev No receive()/fallback(): a plain value transfer (no calldata) must revert. ETH only ever
    /// enters through initiateExit, which always records a message for it.
    function test_plainTransferReverts() public {
        (bool ok,) = address(portal).call{value: 1 ether}("");
        require(!ok, "plain transfer must revert");
    }
}
