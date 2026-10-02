// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {RomeExitPortal} from "../src/RomeExitPortal.sol";

/// @dev Minimal cheatcode surface — no forge-std dependency, same pattern as the test suite.
interface VmScript {
    function envUint(string calldata name) external view returns (uint256);
    function startBroadcast(uint256 privateKey) external;
    function stopBroadcast() external;
}

/// @notice Deploys RomeExitPortal to whatever chain `--rpc-url` points at. The deployer key is read
/// ONLY from the `PRIVATE_KEY` environment variable — never a script argument, never a file. See
/// the operator's deploy script, the one caller: it sets this env var right before invoking
/// `forge script` and nothing else ever carries the key.
contract DeployRomeExitPortal {
    VmScript constant vm = VmScript(address(uint160(uint256(keccak256("hevm cheat code")))));

    function run() external returns (RomeExitPortal portal) {
        uint256 privateKey = vm.envUint("PRIVATE_KEY");
        vm.startBroadcast(privateKey);
        portal = new RomeExitPortal();
        vm.stopBroadcast();
    }
}
