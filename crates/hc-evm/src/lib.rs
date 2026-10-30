//! `hc-evm`: a small, deterministic HyperEVM-like execution layer built on revm.
//!
//! * World state ([`EvmState`]) lives in ordered maps only; `postcard(EvmState)` is canonical and
//!   [`EvmState::state_root`] is the keccak of the canonical encoding of accounts/code/storage.
//! * Blocks are executed with [`EvmState::apply_block`] from raw signed Ethereum txs
//!   (legacy / EIP-2930 / EIP-1559). Invalid txs are skipped, never fatal.
//! * Gas: base fee 0, any gas price accepted, all fees burned (coinbase gets nothing).
//! * [`CORE_WRITER`] is a real EVM contract (genesis-deployed) that emits
//!   `RawAction(address indexed user, bytes data)`; those logs become [`CoreWriterAction`]s.
//! * Native value sent to [`USDC_BRIDGE`] leaves the EVM and is reported in
//!   [`EvmBlockResult::to_core`] in micro-USD.
//! * Core state is readable through precompiles (see [`precompiles`]) backed by a
//!   [`CoreSnapshot`] supplied by the caller.

use std::{collections::BTreeMap, sync::Arc};

use alloy_primitives::keccak256;
use alloy_sol_types::{sol, SolEvent};
use revm::{
    context::{result::ExecutionResult, BlockEnv, TxEnv},
    primitives::{Address, Bytes, TxKind, B256, KECCAK_EMPTY, U256},
    state::EvmState as RevmState,
};
use serde::{Deserialize, Serialize};

mod exec;
pub mod precompiles;
mod tx;

pub use tx::{decode_tx, DecodedTx};

pub type Addr = [u8; 20];
pub const CHAIN_ID: u64 = 31337;
/// System contract: `sendRawAction(bytes)` emits `RawAction(msg.sender, data)`.
pub const CORE_WRITER: Addr = [0x33; 20];
/// Native value sent here (from an EOA or a contract) is moved back to core.
pub const USDC_BRIDGE: Addr = [0x22; 20];
/// Gas limit of a big block (one per minute), and the most any call may use.
pub const BLOCK_GAS_LIMIT: u64 = 30_000_000;
/// Gas limit of a small block (one per second). HyperEVM runs both kinds:
/// small blocks for low latency, big ones for deployments and heavy calls.
pub const SMALL_BLOCK_GAS_LIMIT: u64 = 2_000_000;
/// Native token is USDC with 18 decimals. Core uses micro-USDC (6 decimals): 1 µUSDC = 10^12 wei.
pub const WEI_PER_MICRO_USD: u128 = 1_000_000_000_000;

/// Runtime bytecode of `contracts/CoreWriter.sol` (solc 0.8.28, optimizer 200, no metadata).
/// Rebuild with `cd contracts && forge build`; a unit test checks it matches `contracts/out`.
pub const CORE_WRITER_RUNTIME_HEX: &str = "608060405234801561000f575f5ffd5b5060043610610029575f3560e01c806317938e131461002d575b5f5ffd5b61004061003b366004610089565b610042565b005b336001600160a01b03167f8c7f585fb295f7eb1e6aeb8fba61b23a4fe60beda405f0045073b185c74412e3838360405161007d9291906100f7565b60405180910390a25050565b5f5f6020838503121561009a575f5ffd5b823567ffffffffffffffff8111156100b0575f5ffd5b8301601f810185136100c0575f5ffd5b803567ffffffffffffffff8111156100d6575f5ffd5b8560208284010111156100e7575f5ffd5b6020919091019590945092505050565b60208152816020820152818360408301375f818301604090810191909152601f909201601f1916010191905056";

sol! {
    /// Event emitted by the CoreWriter system contract.
    event RawAction(address indexed user, bytes data);
    /// CoreWriter entry point.
    function sendRawAction(bytes data);
}

/// keccak256("RawAction(address,bytes)").
pub const RAW_ACTION_TOPIC: [u8; 32] = RawAction::SIGNATURE_HASH.0;

/// Read-only view of core (exchange) state, captured by the caller before the EVM block.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct CoreSnapshot {
    /// Per asset, micro-USD per coin.
    pub mark_pxs: Vec<u64>,
    pub oracle_pxs: Vec<u64>,
    /// (user, asset) -> (szi lots, entry notional µUSD).
    pub positions: BTreeMap<(Addr, u32), (i64, i64)>,
    /// µUSD.
    pub account_values: BTreeMap<Addr, i64>,
    /// µUSD (raw cross balance).
    pub core_balances: BTreeMap<Addr, i64>,
    pub sz_decimals: Vec<u32>,
}

/// Account record in [`EvmState`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub nonce: u64,
    /// U256 big-endian.
    pub balance: [u8; 32],
    /// keccak of the code, or KECCAK_EMPTY.
    pub code_hash: [u8; 32],
}

/// Deterministic EVM world state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvmState {
    /// Only non-empty accounts (EIP-161) are kept.
    pub accounts: BTreeMap<Addr, AccountRecord>,
    /// code_hash -> code bytes.
    pub code: BTreeMap<[u8; 32], Vec<u8>>,
    /// (address, slot) -> value. Only non-zero values are kept.
    pub storage: BTreeMap<(Addr, [u8; 32]), [u8; 32]>,
    /// Number of the last applied block (0 = genesis).
    pub block_number: u64,
    /// Hashes of the last 256 blocks (for BLOCKHASH and parent hash).
    pub block_hashes: BTreeMap<u64, [u8; 32]>,
}

/// Block-level environment supplied by consensus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmBlockEnv {
    pub number: u64,
    pub timestamp_s: u64,
    /// Returned by COINBASE; receives no fees (fees are burned).
    pub coinbase: Addr,
    /// [`SMALL_BLOCK_GAS_LIMIT`] or [`BLOCK_GAS_LIMIT`].
    pub gas_limit: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Log {
    pub address: Addr,
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub tx_hash: [u8; 32],
    pub from: Addr,
    pub to: Option<Addr>,
    pub contract_address: Option<Addr>,
    pub success: bool,
    pub gas_used: u64,
    /// Effective gas price paid (and burned) per gas.
    pub effective_gas_price: u128,
    pub logs: Vec<Log>,
    /// Return data, or revert data.
    pub output: Vec<u8>,
    pub nonce: u64,
    /// U256 big-endian.
    pub value_wei: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreWriterAction {
    /// `msg.sender` of the call to CORE_WRITER (EOA or contract).
    pub user: Addr,
    /// Raw bytes passed to `sendRawAction`.
    pub data: Vec<u8>,
    /// Hash of the tx that emitted it.
    pub tx_hash: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvmBlockResult {
    pub block_hash: [u8; 32],
    pub parent_hash: [u8; 32],
    pub number: u64,
    /// [`EvmState::state_root`] after the block.
    pub state_root: [u8; 32],
    pub receipts: Vec<Receipt>,
    pub core_writer_actions: Vec<CoreWriterAction>,
    /// (sender, micro-USD) bridged back to core, in execution order.
    pub to_core: Vec<(Addr, u128)>,
    pub gas_used: u64,
    /// Invalid txs (tx hash = keccak(raw)), with reason.
    pub skipped: Vec<([u8; 32], String)>,
}

/// Result of [`EvmState::call`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallResult {
    pub success: bool,
    pub output: Vec<u8>,
    pub gas_used: u64,
    /// Decoded `Error(string)` / panic reason for reverts, halt reason for halts, or the
    /// validation error if the call could not be executed at all.
    pub revert_reason: Option<String>,
    pub logs: Vec<Log>,
}

fn u256_be(v: U256) -> [u8; 32] {
    v.to_be_bytes::<32>()
}

impl EvmState {
    /// Genesis: empty state with the CoreWriter system contract deployed at [`CORE_WRITER`].
    pub fn genesis() -> EvmState {
        let code = alloy_primitives::hex::decode(CORE_WRITER_RUNTIME_HEX).expect("valid hex");
        let code_hash = keccak256(&code).0;
        let mut s = EvmState {
            accounts: BTreeMap::new(),
            code: BTreeMap::new(),
            storage: BTreeMap::new(),
            block_number: 0,
            block_hashes: BTreeMap::new(),
        };
        s.code.insert(code_hash, code);
        s.accounts.insert(
            CORE_WRITER,
            AccountRecord {
                nonce: 1,
                balance: [0; 32],
                code_hash,
            },
        );
        s
    }

    pub fn balance(&self, a: Addr) -> [u8; 32] {
        self.accounts.get(&a).map(|r| r.balance).unwrap_or([0; 32])
    }

    /// Balance as U256 (convenience).
    pub fn balance_u256(&self, a: Addr) -> U256 {
        U256::from_be_bytes(self.balance(a))
    }

    pub fn nonce(&self, a: Addr) -> u64 {
        self.accounts.get(&a).map(|r| r.nonce).unwrap_or(0)
    }

    pub fn code(&self, a: Addr) -> Vec<u8> {
        self.accounts
            .get(&a)
            .and_then(|r| self.code.get(&r.code_hash))
            .cloned()
            .unwrap_or_default()
    }

    pub fn storage(&self, a: Addr, slot: [u8; 32]) -> [u8; 32] {
        self.storage.get(&(a, slot)).copied().unwrap_or([0; 32])
    }

    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    /// Hash of the latest applied block (zero at genesis).
    pub fn latest_block_hash(&self) -> [u8; 32] {
        self.block_hashes
            .last_key_value()
            .map(|(_, h)| *h)
            .unwrap_or([0; 32])
    }

    /// keccak256(postcard((accounts, code, storage))). Block metadata (number, recent hashes)
    /// is deliberately excluded so the root is stable after the block hash is recorded.
    pub fn state_root(&self) -> [u8; 32] {
        let bytes = postcard::to_stdvec(&(&self.accounts, &self.code, &self.storage))
            .expect("postcard encoding of ordered maps cannot fail");
        keccak256(bytes).0
    }

    /// Canonical full encoding (postcard).
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("postcard encoding cannot fail")
    }

    pub fn decode(bytes: &[u8]) -> Option<EvmState> {
        postcard::from_bytes(bytes).ok()
    }

    /// Bridge-in from core: credit native balance (micro-USD -> wei).
    pub fn credit_micro_usd(&mut self, to: Addr, micro_usd: u128) {
        let wei = U256::from(micro_usd) * U256::from(WEI_PER_MICRO_USD);
        self.add_balance(to, wei);
    }

    fn add_balance(&mut self, to: Addr, wei: U256) {
        if wei.is_zero() {
            return;
        }
        let rec = self.accounts.entry(to).or_insert(AccountRecord {
            nonce: 0,
            balance: [0; 32],
            code_hash: KECCAK_EMPTY.0,
        });
        rec.balance = u256_be(U256::from_be_bytes(rec.balance).saturating_add(wei));
    }

    /// Remove and return the whole balance of `a` (dropping the account if it becomes empty).
    fn take_balance(&mut self, a: Addr) -> U256 {
        let Some(rec) = self.accounts.get_mut(&a) else {
            return U256::ZERO;
        };
        let bal = U256::from_be_bytes(rec.balance);
        rec.balance = [0; 32];
        self.prune_if_empty(a);
        bal
    }

    fn prune_if_empty(&mut self, a: Addr) {
        let empty = match self.accounts.get(&a) {
            Some(r) => r.nonce == 0 && r.balance == [0; 32] && r.code_hash == KECCAK_EMPTY.0,
            None => false,
        };
        if empty && !self.has_storage(a) {
            self.accounts.remove(&a);
        }
    }

    fn has_storage(&self, a: Addr) -> bool {
        self.storage
            .range((a, [0u8; 32])..=(a, [0xffu8; 32]))
            .next()
            .is_some()
    }

    fn clear_storage(&mut self, a: Addr) {
        let keys: Vec<_> = self
            .storage
            .range((a, [0u8; 32])..=(a, [0xffu8; 32]))
            .map(|(k, _)| *k)
            .collect();
        for k in keys {
            self.storage.remove(&k);
        }
    }

    /// Apply a revm state diff. HashMap iteration order is irrelevant: every update targets a
    /// distinct key of an ordered map.
    fn commit_changes(&mut self, changes: RevmState) {
        for (address, account) in changes {
            if !account.is_touched() {
                continue;
            }
            let addr = address.into_array();
            if account.is_selfdestructed() {
                self.accounts.remove(&addr);
                self.clear_storage(addr);
                continue;
            }
            if account.is_created() {
                self.clear_storage(addr);
            }
            let info = &account.info;
            if info.code_hash != KECCAK_EMPTY && !self.code.contains_key(&info.code_hash.0) {
                if let Some(code) = &info.code {
                    self.code
                        .insert(info.code_hash.0, code.original_bytes().to_vec());
                }
            }
            for (slot, value) in account.storage.iter() {
                let key = (addr, u256_be(*slot));
                let v = value.present_value();
                if v.is_zero() {
                    self.storage.remove(&key);
                } else {
                    self.storage.insert(key, u256_be(v));
                }
            }
            self.accounts.insert(
                addr,
                AccountRecord {
                    nonce: info.nonce,
                    balance: u256_be(info.balance),
                    code_hash: info.code_hash.0,
                },
            );
            self.prune_if_empty(addr);
        }
    }

    fn block_env(&self, env: &EvmBlockEnv) -> BlockEnv {
        let parent = self.latest_block_hash();
        let mut prevrandao_src = Vec::with_capacity(40);
        prevrandao_src.extend_from_slice(&parent);
        prevrandao_src.extend_from_slice(&env.number.to_be_bytes());
        BlockEnv {
            number: U256::from(env.number),
            beneficiary: Address::from(env.coinbase),
            timestamp: U256::from(env.timestamp_s),
            gas_limit: env.gas_limit,
            basefee: 0,
            difficulty: U256::ZERO,
            prevrandao: Some(keccak256(&prevrandao_src)),
            ..BlockEnv::default()
        }
    }

    /// Execute raw signed Ethereum txs (EIP-2718 bytes: legacy, EIP-2930, EIP-1559) in order as
    /// one EVM block. Invalid txs (undecodable, bad signature, wrong chain id, bad nonce,
    /// insufficient balance, gas over the remaining block gas) are skipped, not fatal.
    pub fn apply_block(
        &mut self,
        env: &EvmBlockEnv,
        raw_txs: &[Vec<u8>],
        core: &CoreSnapshot,
    ) -> EvmBlockResult {
        let core = Arc::new(core.clone());
        let parent_hash = self.latest_block_hash();
        let block = self.block_env(env);
        let mut receipts = Vec::new();
        let mut core_writer_actions = Vec::new();
        let mut to_core = Vec::new();
        let mut skipped = Vec::new();
        let mut gas_used: u64 = 0;

        for raw in raw_txs {
            let (decoded, tx_env) = match tx::decode_full(raw) {
                Ok(v) => v,
                Err(e) => {
                    skipped.push((keccak256(raw).0, e));
                    continue;
                }
            };
            if decoded.chain_id != CHAIN_ID {
                skipped.push((
                    decoded.tx_hash,
                    format!("wrong chain id {}", decoded.chain_id),
                ));
                continue;
            }
            if decoded.gas_limit > env.gas_limit.saturating_sub(gas_used) {
                skipped.push((
                    decoded.tx_hash,
                    "gas limit exceeds remaining block gas".into(),
                ));
                continue;
            }
            let outcome = match exec::execute_tx(self, block.clone(), tx_env, core.clone(), false) {
                Ok(o) => o,
                Err(e) => {
                    skipped.push((decoded.tx_hash, e.to_string()));
                    continue;
                }
            };
            self.commit_changes(outcome.changes);

            // Bridge: everything that reached USDC_BRIDGE leaves the EVM. Successful transfers are
            // credited to their sender on core; sub-micro-USD dust is refunded to the sender.
            // (Value reaching the bridge any other way is simply burned.)
            let _burned = self.take_balance(USDC_BRIDGE);
            let success = outcome.result.is_success();
            if success {
                for (from, value) in outcome.bridge_transfers {
                    let wei_per = U256::from(WEI_PER_MICRO_USD);
                    let micro = value / wei_per;
                    let dust = value % wei_per;
                    self.add_balance(from, dust);
                    if !micro.is_zero() {
                        to_core.push((from, micro.saturating_to::<u128>()));
                    }
                }
            }

            let tx_gas = outcome.result.tx_gas_used();
            gas_used += tx_gas;
            let logs: Vec<Log> = outcome.result.logs().iter().map(convert_log).collect();
            if success {
                for l in &logs {
                    if let Some(action) = parse_core_writer_log(l, decoded.tx_hash) {
                        core_writer_actions.push(action);
                    }
                }
            }
            let (output, contract_address) = match &outcome.result {
                ExecutionResult::Success { output, .. } => (
                    output.data().to_vec(),
                    output.address().map(|a| a.into_array()),
                ),
                ExecutionResult::Revert { output, .. } => (output.to_vec(), None),
                ExecutionResult::Halt { .. } => (Vec::new(), None),
            };
            receipts.push(Receipt {
                tx_hash: decoded.tx_hash,
                from: decoded.from,
                to: decoded.to,
                contract_address,
                success,
                gas_used: tx_gas,
                effective_gas_price: decoded.effective_gas_price(),
                logs,
                output,
                nonce: decoded.nonce,
                value_wei: decoded.value_wei,
            });
        }

        self.block_number = env.number;
        let state_root = self.state_root();
        let tx_hashes: Vec<[u8; 32]> = receipts.iter().map(|r| r.tx_hash).collect();
        let header = (
            env.number,
            env.timestamp_s,
            env.coinbase,
            env.gas_limit,
            parent_hash,
            &tx_hashes,
            state_root,
            gas_used,
        );
        let block_hash = keccak256(postcard::to_stdvec(&header).expect("encode header")).0;
        self.block_hashes.insert(env.number, block_hash);
        while self.block_hashes.len() > 256 {
            self.block_hashes.pop_first();
        }

        EvmBlockResult {
            block_hash,
            parent_hash,
            number: env.number,
            state_root,
            receipts,
            core_writer_actions,
            to_core,
            gas_used,
            skipped,
        }
    }

    /// eth_call / eth_estimateGas: execute against the current state without committing.
    /// Nonce and balance checks are disabled, gas price is 0, `from` defaults to the zero address
    /// and `gas` to [`BLOCK_GAS_LIMIT`].
    #[allow(clippy::too_many_arguments)]
    pub fn call(
        &self,
        env: &EvmBlockEnv,
        from: Option<Addr>,
        to: Option<Addr>,
        data: &[u8],
        value_wei: u128,
        gas: Option<u64>,
        core: &CoreSnapshot,
    ) -> CallResult {
        let caller = from.unwrap_or([0; 20]);
        let tx = TxEnv {
            tx_type: 0,
            caller: Address::from(caller),
            gas_limit: gas.unwrap_or(BLOCK_GAS_LIMIT),
            gas_price: 0,
            kind: match to {
                Some(a) => TxKind::Call(Address::from(a)),
                None => TxKind::Create,
            },
            value: U256::from(value_wei),
            data: Bytes::copy_from_slice(data),
            nonce: self.nonce(caller),
            chain_id: Some(CHAIN_ID),
            ..TxEnv::default()
        };
        let block = self.block_env(env);
        match exec::execute_tx(self, block, tx, Arc::new(core.clone()), true) {
            Err(e) => CallResult {
                success: false,
                output: Vec::new(),
                gas_used: 0,
                revert_reason: Some(e.to_string()),
                logs: Vec::new(),
            },
            Ok(o) => {
                let gas_used = o.result.tx_gas_used();
                let logs = o.result.logs().iter().map(convert_log).collect();
                match o.result {
                    ExecutionResult::Success { output, .. } => CallResult {
                        success: true,
                        output: output.into_data().to_vec(),
                        gas_used,
                        revert_reason: None,
                        logs,
                    },
                    ExecutionResult::Revert { output, .. } => CallResult {
                        success: false,
                        revert_reason: Some(revert_reason(&output)),
                        output: output.to_vec(),
                        gas_used,
                        logs,
                    },
                    ExecutionResult::Halt { reason, .. } => CallResult {
                        success: false,
                        output: Vec::new(),
                        gas_used,
                        revert_reason: Some(format!("halt: {reason:?}")),
                        logs,
                    },
                }
            }
        }
    }
}

/// `Error(string)` -> the string; panics/custom errors -> alloy's description.
fn revert_reason(output: &[u8]) -> String {
    use alloy_sol_types::SolError;
    if let Ok(r) = alloy_sol_types::Revert::abi_decode(output) {
        return r.reason;
    }
    alloy_sol_types::decode_revert_reason(output)
        .unwrap_or_else(|| "execution reverted".to_string())
}

fn convert_log(l: &revm::primitives::Log) -> Log {
    Log {
        address: l.address.into_array(),
        topics: l.data.topics().iter().map(|t| t.0).collect(),
        data: l.data.data.to_vec(),
    }
}

fn parse_core_writer_log(l: &Log, tx_hash: [u8; 32]) -> Option<CoreWriterAction> {
    if l.address != CORE_WRITER || l.topics.len() != 2 || l.topics[0] != RAW_ACTION_TOPIC {
        return None;
    }
    let topics: Vec<B256> = l.topics.iter().map(|t| B256::from(*t)).collect();
    let ev = RawAction::decode_raw_log(topics, &l.data).ok()?;
    Some(CoreWriterAction {
        user: ev.user.into_array(),
        data: ev.data.to_vec(),
        tx_hash,
    })
}

/// ABI-encode a `sendRawAction(bytes)` call (for EOAs calling CORE_WRITER directly).
pub fn encode_send_raw_action(data: &[u8]) -> Vec<u8> {
    use alloy_sol_types::SolCall;
    sendRawActionCall {
        data: Bytes::copy_from_slice(data),
    }
    .abi_encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_writer_bytecode_matches_artifact() {
        let json = include_str!("../contracts/out/CoreWriter.sol/CoreWriter.json");
        let needle = format!("\"deployedBytecode\":{{\"object\":\"0x{CORE_WRITER_RUNTIME_HEX}\"");
        assert!(
            json.contains(&needle),
            "CORE_WRITER_RUNTIME_HEX out of date with contracts/out"
        );
    }

    #[test]
    fn genesis_roundtrip() {
        let g = EvmState::genesis();
        assert_eq!(EvmState::decode(&g.encode()).unwrap(), g);
        assert!(!g.code(CORE_WRITER).is_empty());
    }
}
