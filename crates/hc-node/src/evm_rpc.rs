//! Ethereum JSON-RPC over the HyperEVM-like state (`POST /evm`), enough for
//! Foundry's `cast` and `forge create`: chain/block queries, balances, nonces,
//! code, `eth_call`, gas estimation, raw transaction submission and receipts.
//! The native token is USDC with 18 decimals.

use crate::node::Shared;
use hc_core::types::{hex_decode, hex_encode, Address, Hash32};
use serde_json::{json, Value};

fn hex_u64(v: u64) -> Value {
    json!(format!("{v:#x}"))
}

fn hex_bytes(b: &[u8]) -> Value {
    json!(format!("0x{}", hex_encode(b)))
}

fn u256_hex(be: &[u8; 32]) -> Value {
    let s = hex_encode(be);
    let t = s.trim_start_matches('0');
    json!(format!("0x{}", if t.is_empty() { "0" } else { t }))
}

fn parse_addr(v: &Value) -> Result<Address, String> {
    v.as_str().and_then(Address::from_hex).ok_or_else(|| "invalid address".into())
}

fn parse_data(v: Option<&Value>) -> Result<Vec<u8>, String> {
    match v.and_then(Value::as_str) {
        None => Ok(vec![]),
        Some(s) => hex_decode(s.strip_prefix("0x").unwrap_or(s)).ok_or_else(|| "invalid hex data".into()),
    }
}

fn parse_quantity(v: Option<&Value>) -> Result<u128, String> {
    match v.and_then(Value::as_str) {
        None => Ok(0),
        Some(s) => u128::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).map_err(|_| "invalid quantity".into()),
    }
}

pub fn handle(shared: &Shared, req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!([]));
    match dispatch(shared, method, &params) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message, data)) => {
            let mut err = json!({"code": code, "message": message});
            if let Some(d) = data {
                err["data"] = d;
            }
            json!({"jsonrpc": "2.0", "id": id, "error": err})
        }
    }
}

type RpcError = (i64, String, Option<Value>);

fn invalid(msg: impl Into<String>) -> RpcError {
    (-32602, msg.into(), None)
}

/// `(from, to, data, value_wei, gas)` of an `eth_call`/`eth_estimateGas` request.
type CallArgs = (Option<[u8; 20]>, Option<[u8; 20]>, Vec<u8>, u128, Option<u64>);

fn call_args(p: &Value) -> Result<CallArgs, RpcError> {
    let from = p.get("from").filter(|v| !v.is_null()).map(parse_addr).transpose().map_err(invalid)?.map(|a| a.0);
    let to = p.get("to").filter(|v| !v.is_null()).map(parse_addr).transpose().map_err(invalid)?.map(|a| a.0);
    let data = parse_data(p.get("input").or_else(|| p.get("data"))).map_err(invalid)?;
    let value = parse_quantity(p.get("value")).map_err(invalid)?;
    let gas = p.get("gas").map(|g| parse_quantity(Some(g)).map(|v| v as u64)).transpose().map_err(invalid)?;
    Ok((from, to, data, value, gas))
}

fn dispatch(shared: &Shared, method: &str, params: &Value) -> Result<Value, RpcError> {
    let p = |i: usize| params.get(i).cloned().unwrap_or(Value::Null);
    match method {
        "eth_chainId" => Ok(hex_u64(hc_evm::CHAIN_ID)),
        "net_version" => Ok(json!(hc_evm::CHAIN_ID.to_string())),
        "web3_clientVersion" => Ok(json!("hc-node/0.1 (HyperEVM-like MVP)")),
        "eth_syncing" => Ok(json!(false)),
        "eth_accounts" => Ok(json!([])),
        "eth_gasPrice" | "eth_maxPriorityFeePerGas" => Ok(hex_u64(0)),
        "eth_blockNumber" => Ok(hex_u64(shared.state.read().expect("state").evm.block_number())),
        "eth_getBalance" => {
            let a = parse_addr(&p(0)).map_err(invalid)?;
            Ok(u256_hex(&shared.state.read().expect("state").evm.balance(a.0)))
        }
        "eth_getTransactionCount" => {
            let a = parse_addr(&p(0)).map_err(invalid)?;
            Ok(hex_u64(shared.state.read().expect("state").evm.nonce(a.0)))
        }
        "eth_getCode" => {
            let a = parse_addr(&p(0)).map_err(invalid)?;
            Ok(hex_bytes(&shared.state.read().expect("state").evm.code(a.0)))
        }
        "eth_getStorageAt" => {
            let a = parse_addr(&p(0)).map_err(invalid)?;
            let slot_v = parse_quantity(Some(&p(1))).map_err(invalid)?;
            let mut slot = [0u8; 32];
            slot[16..].copy_from_slice(&slot_v.to_be_bytes());
            Ok(hex_bytes(&shared.state.read().expect("state").evm.storage(a.0, slot)))
        }
        "eth_call" | "eth_estimateGas" => {
            let (from, to, data, value, gas) = call_args(&p(0))?;
            let st = shared.state.read().expect("state");
            let env = hc_evm::EvmBlockEnv { number: st.evm.block_number() + 1, timestamp_s: st.time_ms / 1000, coinbase: [0; 20], gas_limit: hc_evm::BLOCK_GAS_LIMIT };
            let r = st.evm.call(&env, from, to, &data, value, gas, &st.core_snapshot());
            if !r.success {
                return Err((3, format!("execution reverted{}", r.revert_reason.map(|s| format!(": {s}")).unwrap_or_default()), Some(hex_bytes(&r.output))));
            }
            if method == "eth_call" {
                Ok(hex_bytes(&r.output))
            } else {
                // Headroom for refunds and 63/64 call-gas rules.
                Ok(hex_u64(r.gas_used + r.gas_used / 4 + 10_000))
            }
        }
        "eth_sendRawTransaction" => {
            let raw = parse_data(Some(&p(0))).map_err(invalid)?;
            match shared.submit_evm(raw.clone()).or_else(|| hc_evm::decode_tx(&raw).map(|d| Hash32(d.tx_hash))) {
                Some(h) => Ok(json!(h)),
                None => Err((-32000, "invalid transaction (signature, encoding or chain id)".into(), None)),
            }
        }
        "eth_getTransactionReceipt" => {
            let h = parse_data(Some(&p(0))).map_err(invalid)?;
            let hist = shared.history.read().expect("history");
            let Some((number, block_hash, r)) = h.try_into().ok().and_then(|h: [u8; 32]| hist.evm_receipts.get(&Hash32(h))) else {
                return Ok(Value::Null);
            };
            let index = hist.evm_blocks.get(number).and_then(|(_, _, hs, _)| hs.iter().position(|x| *x == r.tx_hash)).unwrap_or(0);
            Ok(json!({
                "transactionHash": r.tx_hash,
                "transactionIndex": hex_u64(index as u64),
                "blockHash": block_hash,
                "blockNumber": hex_u64(*number),
                "from": r.from,
                "to": r.to,
                "contractAddress": r.contract_address,
                "status": hex_u64(r.success as u64),
                "gasUsed": hex_u64(r.gas_used),
                "cumulativeGasUsed": hex_u64(r.gas_used),
                "effectiveGasPrice": hex_u64(0),
                "type": "0x2",
                "logsBloom": format!("0x{}", "0".repeat(512)),
                "logs": r.logs.iter().enumerate().map(|(i, l)| json!({
                    "address": l.address, "topics": l.topics, "data": l.data,
                    "blockNumber": hex_u64(*number), "blockHash": block_hash, "transactionHash": r.tx_hash,
                    "transactionIndex": hex_u64(index as u64), "logIndex": hex_u64(i as u64), "removed": false,
                })).collect::<Vec<_>>(),
            }))
        }
        "eth_getTransactionByHash" => {
            let h = parse_data(Some(&p(0))).map_err(invalid)?;
            let hist = shared.history.read().expect("history");
            let Some((number, block_hash, r)) = h.try_into().ok().and_then(|h: [u8; 32]| hist.evm_receipts.get(&Hash32(h))) else {
                return Ok(Value::Null);
            };
            Ok(json!({
                "hash": r.tx_hash, "blockHash": block_hash, "blockNumber": hex_u64(*number), "transactionIndex": "0x0",
                "from": r.from, "to": r.to, "nonce": hex_u64(r.nonce), "value": format!("{:#x}", r.value_wei.parse::<u128>().unwrap_or(0)),
                "gas": hex_u64(r.gas_used), "gasPrice": "0x0", "input": "0x", "type": "0x2", "chainId": hex_u64(hc_evm::CHAIN_ID),
                "v": "0x0", "r": "0x0", "s": "0x0",
            }))
        }
        "eth_getBlockByNumber" | "eth_getBlockByHash" => {
            let hist = shared.history.read().expect("history");
            let latest = hist.evm_blocks.keys().next_back().copied().unwrap_or(0);
            let number = if method == "eth_getBlockByHash" {
                let h = parse_data(Some(&p(0))).map_err(invalid)?;
                match hist.evm_blocks.iter().find(|(_, (bh, ..))| bh.0.as_slice() == h.as_slice()) {
                    Some((n, _)) => *n,
                    None => return Ok(Value::Null),
                }
            } else {
                match p(0).as_str() {
                    Some("latest" | "pending" | "safe" | "finalized") | None => latest,
                    Some("earliest") => 0,
                    Some(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| invalid("bad block number"))?,
                }
            };
            let (hash, ts, txs, gas) = match hist.evm_blocks.get(&number) {
                Some((h, ts, txs, gas)) => (*h, *ts, txs.clone(), *gas),
                None if number == 0 => (Hash32::default(), 0, vec![], 0),
                None => return Ok(Value::Null),
            };
            let parent = number.checked_sub(1).and_then(|n| hist.evm_blocks.get(&n)).map(|b| b.0).unwrap_or_default();
            Ok(json!({
                "number": hex_u64(number), "hash": hash, "parentHash": parent, "timestamp": hex_u64(ts),
                "gasLimit": hex_u64(if hist.big_evm_blocks.contains(&number) { hc_evm::BLOCK_GAS_LIMIT } else { hc_evm::SMALL_BLOCK_GAS_LIMIT }), "gasUsed": hex_u64(gas), "baseFeePerGas": "0x0",
                "miner": Address::default(), "difficulty": "0x0", "totalDifficulty": "0x0", "extraData": "0x",
                "nonce": "0x0000000000000000", "mixHash": Hash32::default(), "sha3Uncles": Hash32::default(),
                "logsBloom": format!("0x{}", "0".repeat(512)), "transactionsRoot": Hash32::default(),
                "stateRoot": Hash32::default(), "receiptsRoot": Hash32::default(), "size": "0x0", "uncles": [],
                "transactions": txs,
            }))
        }
        "eth_feeHistory" => Ok(json!({"oldestBlock": "0x0", "baseFeePerGas": ["0x0", "0x0"], "gasUsedRatio": [0.0], "reward": [["0x0"]]})),
        "eth_getLogs" => {
            let f = p(0);
            let hist = shared.history.read().expect("history");
            let latest = hist.evm_blocks.keys().next_back().copied().unwrap_or(0);
            let block_arg = |k: &str, default: u64| match f.get(k).and_then(Value::as_str) {
                None | Some("latest" | "pending" | "safe" | "finalized") => Ok(default),
                Some("earliest") => Ok(0),
                Some(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| invalid("bad block number")),
            };
            let (from, to) = (block_arg("fromBlock", latest)?, block_arg("toBlock", latest)?);
            let addrs: Vec<String> = match f.get("address") {
                Some(Value::String(a)) => vec![a.to_lowercase()],
                Some(Value::Array(v)) => v.iter().filter_map(|a| a.as_str().map(str::to_lowercase)).collect(),
                _ => vec![],
            };
            // Each topic position is null (any), a hash, or a list of alternatives.
            let topics: Vec<Vec<String>> = f
                .get("topics")
                .and_then(Value::as_array)
                .map(|ts| {
                    ts.iter()
                        .map(|t| match t {
                            Value::String(s) => vec![s.to_lowercase()],
                            Value::Array(v) => v.iter().filter_map(|x| x.as_str().map(str::to_lowercase)).collect(),
                            _ => vec![],
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut out = Vec::new();
            for (number, (block_hash, _, tx_hashes, _)) in hist.evm_blocks.range(from..=to) {
                for (ti, h) in tx_hashes.iter().enumerate() {
                    let Some((_, _, r)) = hist.evm_receipts.get(h) else { continue };
                    for (li, l) in r.logs.iter().enumerate() {
                        let addr = l.address.to_hex();
                        if !addrs.is_empty() && !addrs.contains(&addr) {
                            continue;
                        }
                        let log_topics: Vec<String> = l.topics.iter().map(|t| format!("0x{}", hex_encode(&t.0))).collect();
                        let matches = topics.iter().enumerate().all(|(i, alts)| alts.is_empty() || log_topics.get(i).is_some_and(|t| alts.contains(t)));
                        if matches {
                            out.push(json!({
                                "address": l.address, "topics": l.topics, "data": l.data, "blockNumber": hex_u64(*number),
                                "blockHash": block_hash, "transactionHash": h, "transactionIndex": hex_u64(ti as u64),
                                "logIndex": hex_u64(li as u64), "removed": false,
                            }));
                        }
                    }
                }
            }
            Ok(json!(out))
        }
        other => Err((-32601, format!("method {other} not supported"), None)),
    }
}
