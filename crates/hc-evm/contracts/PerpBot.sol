// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface ICoreWriter {
    function sendRawAction(bytes calldata data) external;
}

/// @notice Example HyperEVM-style contract for the hc MVP chain.
/// - `placeLimit` sends a Hyperliquid-encoded limit order action through CoreWriter
///   (core sees the order as coming from THIS contract's address).
/// - `mark` reads the mark-price precompile (0x...0806).
/// - `withdrawToCore` forwards native USDC to the bridge address; core credits this contract's account.
contract PerpBot {
    address constant CORE_WRITER = 0x3333333333333333333333333333333333333333;
    address constant USDC_BRIDGE = 0x2222222222222222222222222222222222222222;
    address constant MARK_PX = 0x0000000000000000000000000000000000000806;

    /// Hyperliquid TIF encoding: 1 = Alo, 2 = Gtc, 3 = Ioc.
    function placeLimit(uint32 asset, bool isBuy, uint64 limitPx1e8, uint64 sz1e8, uint8 tif) external {
        bytes memory encodedAction =
            abi.encode(asset, isBuy, limitPx1e8, sz1e8, false, tif, uint128(0));
        // 1 byte version (=1) | 3 bytes action id (=1, limit order) big-endian | abi-encoded action
        bytes memory data = abi.encodePacked(uint8(1), uint24(1), encodedAction);
        ICoreWriter(CORE_WRITER).sendRawAction(data);
    }

    function mark(uint32 asset) external view returns (uint64) {
        (bool ok, bytes memory res) = MARK_PX.staticcall(abi.encode(asset));
        require(ok, "markPx precompile failed");
        return abi.decode(res, (uint64));
    }

    function withdrawToCore() external payable {
        (bool ok,) = USDC_BRIDGE.call{value: msg.value}("");
        require(ok, "bridge transfer failed");
    }
}
