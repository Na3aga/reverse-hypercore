// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface ICoreWriter {
    function sendRawAction(bytes calldata data) external;
}

struct Position {
    int64 szi;
    uint64 entryNtl;
    int64 isolatedRawUsd;
    uint32 leverage;
    bool isIsolated;
}

/// Test contract: reads core precompiles, records results in storage, and calls CoreWriter.
contract Probe {
    uint64 public lastMark;
    int64 public lastSzi;
    uint64 public lastEntryNtl;
    int64 public lastAccountValue;

    function markPx(uint32 asset) public view returns (uint64) {
        (bool ok, bytes memory res) = address(0x806).staticcall(abi.encode(asset));
        require(ok, "markPx");
        return abi.decode(res, (uint64));
    }

    function oraclePx(uint32 asset) public view returns (uint64) {
        (bool ok, bytes memory res) = address(0x807).staticcall(abi.encode(asset));
        require(ok, "oraclePx");
        return abi.decode(res, (uint64));
    }

    function position(address user, uint32 asset) public view returns (Position memory) {
        (bool ok, bytes memory res) = address(0x800).staticcall(abi.encode(user, asset));
        require(ok, "position");
        return abi.decode(res, (Position));
    }

    function accountValue(address user) public view returns (int64) {
        (bool ok, bytes memory res) = address(0x80F).staticcall(abi.encode(user));
        require(ok, "accountValue");
        return abi.decode(res, (int64));
    }

    function coreBalance(address user) public view returns (int64) {
        (bool ok, bytes memory res) = address(0x810).staticcall(abi.encode(user));
        require(ok, "coreBalance");
        return abi.decode(res, (int64));
    }

    /// Reads precompiles, stores the results, and sends `data` to CoreWriter as this contract.
    function act(address user, uint32 asset, bytes calldata data) external {
        lastMark = markPx(asset);
        Position memory p = position(user, asset);
        lastSzi = p.szi;
        lastEntryNtl = p.entryNtl;
        lastAccountValue = accountValue(user);
        ICoreWriter(0x3333333333333333333333333333333333333333).sendRawAction(data);
    }
}
