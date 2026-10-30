//! Glue between the EVM and core: EVM block scheduling, CoreWriter action
//! decoding and receipt summaries.
//!
//! EVM blocks, as on HyperEVM, come in two sizes on one chain of block
//! numbers: a small block (2M gas) every second and a big block (30M gas)
//! every minute. Transactions finalized in a core block wait in the queue of
//! their sender's block type (`evmUserModify {usingBigBlocks}` picks it) until
//! the next block of that type, in arrival order.
//!
//! CoreWriter orders from the EVM run on core [`CORE_WRITER_DELAY_MS`] later,
//! so a contract gains no latency edge over an ordinary order; cancels run
//! right after their EVM block.
//!
//! CoreWriter payloads use Hyperliquid's public encoding: 1 byte version (=1),
//! a 3-byte big-endian action id, then the ABI-encoded parameters. Prices and
//! sizes are `10^8 × human value`. Supported actions:
//! - `1` limit order: `(uint32 asset, bool isBuy, uint64 limitPx, uint64 sz,
//!   bool reduceOnly, uint8 encodedTif /* 1 Alo, 2 Gtc, 3 Ioc */, uint128 cloid)`;
//! - `10` cancel by oid: `(uint32 asset, uint64 oid)`.

use crate::clearinghouse::AssetMeta;
use crate::events::{EvmLog, EvmTxSummary};
use crate::fixed::{pow10, Px};
use crate::types::{hex_encode, Address, AssetId, Hash32, Oid, Side, Tif};

/// Delay before a CoreWriter order runs on core. Hyperliquid documents a delay
/// of "a few seconds" for orders sent from CoreWriter.
pub const CORE_WRITER_DELAY_MS: u64 = 3_000;

/// A finalized EVM transaction waiting for its EVM block.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedEvmTx {
    pub hash: Hash32,
    pub raw: crate::action::RawEvmTx,
    pub gas_limit: u64,
}

/// A CoreWriter order waiting for its delay to pass.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DelayedCoreWriter {
    pub execute_at_ms: u64,
    pub user: Address,
    pub data: Vec<u8>,
    pub tx_hash: Hash32,
}

pub struct CoreWriterOrder {
    pub asset: AssetId,
    pub side: Side,
    pub px: Px,
    pub lots: u64,
    pub reduce_only: bool,
    pub tif: Tif,
}

pub enum CoreWriterCall {
    Order(CoreWriterOrder),
    Cancel { asset: AssetId, oid: Oid },
}

fn word(args: &[u8], i: usize) -> Result<&[u8], String> {
    args.get(i * 32..(i + 1) * 32).ok_or_else(|| "CoreWriter: truncated arguments".to_string())
}

/// Reads an ABI word as an unsigned integer that must fit in `bits`.
fn uint(args: &[u8], i: usize, bits: u32) -> Result<u128, String> {
    let w = word(args, i)?;
    if w[..16].iter().any(|b| *b != 0) {
        return Err("CoreWriter: integer out of range".into());
    }
    let v = u128::from_be_bytes(w[16..].try_into().expect("16 bytes"));
    if bits < 128 && v >> bits != 0 {
        return Err("CoreWriter: integer out of range".into());
    }
    Ok(v)
}

fn boolean(args: &[u8], i: usize) -> Result<bool, String> {
    match uint(args, i, 8)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err("CoreWriter: invalid bool".into()),
    }
}

/// Converts a `1e8`-scaled value to `10^-target_decimals` units, exactly.
fn rescale_1e8(v: u128, target_decimals: u32) -> Result<u64, String> {
    let v = if target_decimals >= 8 {
        v * pow10(target_decimals - 8) as u128
    } else {
        let d = pow10(8 - target_decimals) as u128;
        if !v.is_multiple_of(d) {
            return Err("CoreWriter: value has too many decimals".into());
        }
        v / d
    };
    u64::try_from(v).map_err(|_| "CoreWriter: value too large".to_string())
}

pub fn decode_core_writer(data: &[u8], metas: &[AssetMeta]) -> Result<CoreWriterCall, String> {
    if data.len() < 4 || data[0] != 1 {
        return Err("CoreWriter: unsupported encoding version".into());
    }
    let id = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    let args = &data[4..];
    match id {
        1 => {
            let asset = uint(args, 0, 32)? as AssetId;
            let meta = metas.get(asset as usize).ok_or("CoreWriter: unknown asset")?;
            let tif = match uint(args, 5, 8)? {
                1 => Tif::Alo,
                2 => Tif::Gtc,
                3 => Tif::Ioc,
                _ => return Err("CoreWriter: invalid tif".into()),
            };
            let lots = rescale_1e8(uint(args, 3, 64)?, meta.sz_decimals)?;
            if lots == 0 {
                return Err("Order has invalid size.".into());
            }
            Ok(CoreWriterCall::Order(CoreWriterOrder {
                asset,
                side: Side::from_is_buy(boolean(args, 1)?),
                px: Px(rescale_1e8(uint(args, 2, 64)?, 6)?),
                lots,
                reduce_only: boolean(args, 4)?,
                tif,
            }))
        }
        10 => Ok(CoreWriterCall::Cancel { asset: uint(args, 0, 32)? as AssetId, oid: uint(args, 1, 64)? as Oid }),
        other => Err(format!("CoreWriter: unsupported action id {other}")),
    }
}

pub fn receipt_summary(r: &hc_evm::Receipt) -> EvmTxSummary {
    EvmTxSummary {
        tx_hash: Hash32(r.tx_hash),
        from: Address(r.from),
        to: r.to.map(Address),
        contract_address: r.contract_address.map(Address),
        success: r.success,
        gas_used: r.gas_used,
        nonce: r.nonce,
        logs: r
            .logs
            .iter()
            .map(|l| EvmLog { address: Address(l.address), topics: l.topics.iter().map(|t| Hash32(*t)).collect(), data: format!("0x{}", hex_encode(&l.data)) })
            .collect(),
        value_wei: wei_string(&r.value_wei),
        output: format!("0x{}", hex_encode(&r.output)),
    }
}

/// Decimal string of a big-endian U256.
pub fn wei_string(be: &[u8; 32]) -> String {
    // Repeated division by 10 on a big-endian byte array.
    let mut n = be.to_vec();
    let mut digits = Vec::new();
    while n.iter().any(|b| *b != 0) {
        let mut rem = 0u32;
        for b in n.iter_mut() {
            let cur = (rem << 8) | *b as u32;
            *b = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        return "0".into();
    }
    digits.reverse();
    String::from_utf8(digits).expect("ascii digits")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clearinghouse::MarginTier;
    use crate::fixed::Usd;

    fn abi(words: &[u128]) -> Vec<u8> {
        words.iter().flat_map(|w| {
            let mut b = [0u8; 32];
            b[16..].copy_from_slice(&w.to_be_bytes());
            b
        }).collect()
    }

    #[test]
    fn decodes_limit_order() {
        let metas = vec![AssetMeta {
            name: "BTC".into(),
            sz_decimals: 5,
            max_leverage: 40,
            margin_tiers: vec![MarginTier { lower_bound: Usd::ZERO, max_leverage: 40 }],
            impact_ntl: Usd::ZERO,
        }];
        // Buy 0.01 BTC @ 100000, GTC.
        let mut data = vec![1, 0, 0, 1];
        data.extend(abi(&[0, 1, 100_000 * 100_000_000, 1_000_000, 0, 2, 0]));
        let CoreWriterCall::Order(o) = decode_core_writer(&data, &metas).unwrap() else { panic!() };
        assert_eq!((o.side, o.px, o.lots, o.tif), (Side::Bid, Px(100_000_000_000), 1_000, Tif::Gtc));
        // Size finer than the lot size is rejected.
        let mut bad = vec![1, 0, 0, 1];
        bad.extend(abi(&[0, 1, 100_000 * 100_000_000, 1, 0, 2, 0]));
        assert!(decode_core_writer(&bad, &metas).is_err());
    }

    #[test]
    fn wei_decimal() {
        let mut b = [0u8; 32];
        b[24..].copy_from_slice(&1_000_000_000_000_000_000u64.to_be_bytes());
        assert_eq!(wei_string(&b), "1000000000000000000");
        assert_eq!(wei_string(&[0u8; 32]), "0");
    }
}
