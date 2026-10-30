//! Raw signed transaction decoding (EIP-2718 envelopes) and signer recovery.

use alloy_consensus::{transaction::SignerRecoverable, Transaction as _, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use revm::{context::TxEnv, primitives::TxKind};

use crate::Addr;

/// A decoded, signature-recovered transaction (for mempool admission and RPC).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedTx {
    /// keccak256 of the raw EIP-2718 bytes (the Ethereum tx hash).
    pub tx_hash: [u8; 32],
    /// Recovered signer.
    pub from: Addr,
    pub nonce: u64,
    /// EIP-155 / typed-tx chain id. Pre-EIP-155 legacy txs are rejected by [`decode_tx`].
    pub chain_id: u64,
    /// 0 = legacy, 1 = EIP-2930, 2 = EIP-1559.
    pub tx_type: u8,
    pub to: Option<Addr>,
    /// U256 big-endian.
    pub value_wei: [u8; 32],
    pub gas_limit: u64,
    /// Legacy/2930 gas price, or EIP-1559 max fee per gas.
    pub max_fee_per_gas: u128,
    /// EIP-1559 max priority fee; `None` for legacy/2930.
    pub max_priority_fee_per_gas: Option<u128>,
    pub input: Vec<u8>,
}

impl DecodedTx {
    /// Effective gas price with base fee 0 (what the sender actually pays per gas; it is burned).
    pub fn effective_gas_price(&self) -> u128 {
        match self.max_priority_fee_per_gas {
            Some(tip) => tip.min(self.max_fee_per_gas),
            None => self.max_fee_per_gas,
        }
    }
}

/// Decode + recover signer. Returns `None` for malformed RLP, trailing bytes, bad signatures,
/// unsupported tx types (EIP-4844 blob and EIP-7702 txs are not accepted) and pre-EIP-155
/// legacy txs without a chain id. The chain id is *not* checked here (callers compare it to
/// [`crate::CHAIN_ID`]; `apply_block` skips mismatches).
pub fn decode_tx(raw: &[u8]) -> Option<DecodedTx> {
    decode_full(raw).ok().map(|(d, _)| d)
}

pub(crate) fn decode_full(raw: &[u8]) -> Result<(DecodedTx, TxEnv), String> {
    let mut buf = raw;
    let env = TxEnvelope::decode_2718(&mut buf).map_err(|e| format!("decode: {e}"))?;
    if !buf.is_empty() {
        return Err("decode: trailing bytes".into());
    }
    let tx_type: u8 = match &env {
        TxEnvelope::Legacy(_) => 0,
        TxEnvelope::Eip2930(_) => 1,
        TxEnvelope::Eip1559(_) => 2,
        _ => return Err("unsupported tx type".into()),
    };
    let chain_id = env
        .chain_id()
        .ok_or_else(|| "missing chain id (pre-EIP-155)".to_string())?;
    let from = env
        .recover_signer()
        .map_err(|e| format!("bad signature: {e}"))?;
    let to = match env.kind() {
        TxKind::Call(a) => Some(a.into_array()),
        TxKind::Create => None,
    };
    let max_priority_fee_per_gas = if tx_type == 2 {
        env.max_priority_fee_per_gas()
    } else {
        None
    };
    let decoded = DecodedTx {
        tx_hash: env.tx_hash().0,
        from: from.into_array(),
        nonce: env.nonce(),
        chain_id,
        tx_type,
        to,
        value_wei: env.value().to_be_bytes(),
        gas_limit: env.gas_limit(),
        max_fee_per_gas: env.max_fee_per_gas(),
        max_priority_fee_per_gas,
        input: env.input().to_vec(),
    };
    let tx_env = TxEnv {
        tx_type,
        caller: from,
        gas_limit: env.gas_limit(),
        gas_price: env.max_fee_per_gas(),
        kind: env.kind(),
        value: env.value(),
        data: env.input().clone(),
        nonce: env.nonce(),
        chain_id: Some(chain_id),
        access_list: env.access_list().cloned().unwrap_or_default(),
        gas_priority_fee: max_priority_fee_per_gas,
        ..TxEnv::default()
    };
    Ok((decoded, tx_env))
}
