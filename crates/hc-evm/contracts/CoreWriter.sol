// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @notice System contract deployed at genesis at 0x3333333333333333333333333333333333333333.
/// Its runtime bytecode is embedded in hc-evm (`CORE_WRITER_RUNTIME`). The node collects every
/// `RawAction` log emitted by this address in successful transactions and forwards it to core.
contract CoreWriter {
    event RawAction(address indexed user, bytes data);

    function sendRawAction(bytes calldata data) external {
        emit RawAction(msg.sender, data);
    }
}
