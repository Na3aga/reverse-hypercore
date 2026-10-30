use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolCall, SolValue};
use hc_evm::*;

sol! {
    struct Position {
        int64 szi;
        uint64 entryNtl;
        int64 isolatedRawUsd;
        uint32 leverage;
        bool isIsolated;
    }
    interface IProbe {
        function act(address user, uint32 asset, bytes data) external;
        function lastMark() external view returns (uint64);
        function lastSzi() external view returns (int64);
        function lastEntryNtl() external view returns (uint64);
        function lastAccountValue() external view returns (int64);
        function markPx(uint32 asset) external view returns (uint64);
        function oraclePx(uint32 asset) external view returns (uint64);
        function position(address user, uint32 asset) external view returns (Position memory);
        function coreBalance(address user) external view returns (int64);
    }
    interface IPerpBot {
        function placeLimit(uint32 asset, bool isBuy, uint64 limitPx1e8, uint64 sz1e8, uint8 tif) external;
        function mark(uint32 asset) external view returns (uint64);
        function withdrawToCore() external payable;
    }
}

const GWEI: u128 = 1_000_000_000;
const USDC: u128 = 1_000_000; // micro-USD per USDC
const ONE_USDC_WEI: u128 = 1_000_000_000_000_000_000;

fn signer(n: u8) -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::repeat_byte(n)).unwrap()
}

fn addr(s: &PrivateKeySigner) -> Addr {
    s.address().into_array()
}

struct Tx {
    nonce: u64,
    to: Option<Addr>,
    value: U256,
    input: Vec<u8>,
    gas_limit: u64,
    chain_id: u64,
}

impl Tx {
    fn call(nonce: u64, to: Addr, value: u128, input: Vec<u8>) -> Self {
        Tx {
            nonce,
            to: Some(to),
            value: U256::from(value),
            input,
            gas_limit: 1_000_000,
            chain_id: CHAIN_ID,
        }
    }
    fn create(nonce: u64, code: Vec<u8>) -> Self {
        Tx {
            nonce,
            to: None,
            value: U256::ZERO,
            input: code,
            gas_limit: 3_000_000,
            chain_id: CHAIN_ID,
        }
    }
}

fn kind(to: Option<Addr>) -> TxKind {
    match to {
        Some(a) => TxKind::Call(Address::from(a)),
        None => TxKind::Create,
    }
}

/// EIP-1559 tx: max fee 2 gwei, tip 1 gwei => effective (burned) price 1 gwei.
fn sign_1559(s: &PrivateKeySigner, t: Tx) -> Vec<u8> {
    let tx = TxEip1559 {
        chain_id: t.chain_id,
        nonce: t.nonce,
        gas_limit: t.gas_limit,
        max_fee_per_gas: 2 * GWEI,
        max_priority_fee_per_gas: GWEI,
        to: kind(t.to),
        value: t.value,
        access_list: Default::default(),
        input: Bytes::from(t.input),
    };
    let sig = s.sign_hash_sync(&tx.signature_hash()).unwrap();
    TxEnvelope::from(tx.into_signed(sig)).encoded_2718()
}

/// Legacy EIP-155 tx with gas price 0.
fn sign_legacy(s: &PrivateKeySigner, t: Tx) -> Vec<u8> {
    let tx = TxLegacy {
        chain_id: Some(t.chain_id),
        nonce: t.nonce,
        gas_price: 0,
        gas_limit: t.gas_limit,
        to: kind(t.to),
        value: t.value,
        input: Bytes::from(t.input),
    };
    let sig = s.sign_hash_sync(&tx.signature_hash()).unwrap();
    TxEnvelope::from(tx.into_signed(sig)).encoded_2718()
}

fn env(n: u64) -> EvmBlockEnv {
    EvmBlockEnv {
        gas_limit: hc_evm::BLOCK_GAS_LIMIT,
        number: n,
        timestamp_s: 1_700_000_000 + n,
        coinbase: [0xcc; 20],
    }
}

fn bal(st: &EvmState, a: Addr) -> U256 {
    U256::from_be_bytes(st.balance(a))
}

fn create_address(from: Addr, nonce: u64) -> Addr {
    Address::from(from).create(nonce).into_array()
}

fn probe_code() -> Vec<u8> {
    alloy_primitives::hex::decode(include_str!("contracts/Probe.bin").trim()).unwrap()
}

fn perpbot_code() -> Vec<u8> {
    let json: serde_json::Value =
        serde_json::from_str(include_str!("../contracts/out/PerpBot.sol/PerpBot.json")).unwrap();
    alloy_primitives::hex::decode(json["bytecode"]["object"].as_str().unwrap()).unwrap()
}

fn call_ok(st: &EvmState, to: Addr, data: Vec<u8>, core: &CoreSnapshot) -> Vec<u8> {
    let r = st.call(
        &env(st.block_number() + 1),
        None,
        Some(to),
        &data,
        0,
        None,
        core,
    );
    assert!(r.success, "call failed: {:?}", r.revert_reason);
    r.output
}

// -------------------------------------------------------------------------------------------

#[test]
fn transfer_between_eoas_burns_fees() {
    let (a, b) = (signer(1), signer(2));
    let mut st = EvmState::genesis();
    st.credit_micro_usd(addr(&a), 100 * USDC);
    assert_eq!(bal(&st, addr(&a)), U256::from(100 * ONE_USDC_WEI));

    let raw = sign_1559(
        &a,
        Tx {
            gas_limit: 21_000,
            ..Tx::call(0, addr(&b), ONE_USDC_WEI, vec![])
        },
    );
    let decoded = decode_tx(&raw).unwrap();
    assert_eq!(decoded.from, addr(&a));
    assert_eq!(decoded.nonce, 0);
    assert_eq!(decoded.chain_id, CHAIN_ID);

    let res = st.apply_block(&env(1), &[raw], &CoreSnapshot::default());
    assert!(res.skipped.is_empty(), "{:?}", res.skipped);
    assert_eq!(res.receipts.len(), 1);
    let r = &res.receipts[0];
    assert!(r.success);
    assert_eq!(r.gas_used, 21_000);
    assert_eq!(r.tx_hash, decoded.tx_hash);
    assert_eq!(r.effective_gas_price, GWEI);
    assert_eq!(res.gas_used, 21_000);

    let fee = U256::from(21_000u128 * GWEI);
    assert_eq!(bal(&st, addr(&b)), U256::from(ONE_USDC_WEI));
    assert_eq!(bal(&st, addr(&a)), U256::from(99 * ONE_USDC_WEI) - fee);
    assert_eq!(st.nonce(addr(&a)), 1);
    assert_eq!(st.nonce(addr(&b)), 0);
    // fee burned, nothing to coinbase
    assert_eq!(bal(&st, [0xcc; 20]), U256::ZERO);
    assert_eq!(st.block_number(), 1);

    // zero gas price legacy tx is accepted
    let raw = sign_legacy(
        &b,
        Tx {
            gas_limit: 21_000,
            ..Tx::call(0, addr(&a), 1, vec![])
        },
    );
    let res = st.apply_block(&env(2), &[raw], &CoreSnapshot::default());
    assert!(res.skipped.is_empty() && res.receipts[0].success);
    assert_eq!(bal(&st, addr(&b)), U256::from(ONE_USDC_WEI - 1));
    assert_ne!(res.parent_hash, [0; 32]);
}

#[test]
fn contract_reads_precompiles_and_writes_core() {
    let a = signer(1);
    let user = [0xab; 20];
    let mut st = EvmState::genesis();
    st.credit_micro_usd(addr(&a), 1_000 * USDC);

    let mut core = CoreSnapshot {
        mark_pxs: vec![100, 2_500_000_000],
        oracle_pxs: vec![99, 2_499_000_000],
        sz_decimals: vec![2, 4],
        ..Default::default()
    };
    core.positions.insert((user, 1), (-5, 12_500_000));
    core.account_values.insert(user, -42);
    core.core_balances.insert(user, 7_000_000);

    // deploy Probe
    let res = st.apply_block(
        &env(1),
        &[sign_1559(&a, Tx::create(0, probe_code()))],
        &core,
    );
    assert!(res.receipts[0].success, "{:?}", res);
    let probe = res.receipts[0].contract_address.unwrap();
    assert_eq!(probe, create_address(addr(&a), 0));
    assert!(!st.code(probe).is_empty());

    // act(): reads precompiles, stores results, calls CoreWriter as the contract
    let payload = vec![1u8, 0, 0, 1, 0xde, 0xad, 0xbe, 0xef];
    let input = IProbe::actCall {
        user: user.into(),
        asset: 1,
        data: payload.clone().into(),
    }
    .abi_encode();
    // and an EOA calling CoreWriter directly
    let direct = encode_send_raw_action(b"hello core");
    let res = st.apply_block(
        &env(2),
        &[
            sign_1559(&a, Tx::call(1, probe, 0, input)),
            sign_1559(&a, Tx::call(2, CORE_WRITER, 0, direct)),
        ],
        &core,
    );
    assert!(res.skipped.is_empty(), "{:?}", res.skipped);
    assert!(res.receipts.iter().all(|r| r.success), "{:?}", res.receipts);
    assert_eq!(res.core_writer_actions.len(), 2);
    assert_eq!(res.core_writer_actions[0].user, probe);
    assert_eq!(res.core_writer_actions[0].data, payload);
    assert_eq!(res.core_writer_actions[0].tx_hash, res.receipts[0].tx_hash);
    assert_eq!(res.core_writer_actions[1].user, addr(&a));
    assert_eq!(res.core_writer_actions[1].data, b"hello core".to_vec());

    // stored values (via eth_call of public getters)
    let get = |d: Vec<u8>| call_ok(&st, probe, d, &core);
    assert_eq!(
        IProbe::lastMarkCall::abi_decode_returns(&get(IProbe::lastMarkCall {}.abi_encode()))
            .unwrap(),
        2_500_000_000
    );
    assert_eq!(
        IProbe::lastSziCall::abi_decode_returns(&get(IProbe::lastSziCall {}.abi_encode())).unwrap(),
        -5
    );
    assert_eq!(
        IProbe::lastEntryNtlCall::abi_decode_returns(
            &get(IProbe::lastEntryNtlCall {}.abi_encode())
        )
        .unwrap(),
        12_500_000
    );
    assert_eq!(
        IProbe::lastAccountValueCall::abi_decode_returns(&get(
            IProbe::lastAccountValueCall {}.abi_encode()
        ))
        .unwrap(),
        -42
    );

    // direct views
    let out = get(IProbe::oraclePxCall { asset: 0 }.abi_encode());
    assert_eq!(IProbe::oraclePxCall::abi_decode_returns(&out).unwrap(), 99);
    let out = get(IProbe::coreBalanceCall { user: user.into() }.abi_encode());
    assert_eq!(
        IProbe::coreBalanceCall::abi_decode_returns(&out).unwrap(),
        7_000_000
    );
    let out = get(IProbe::positionCall {
        user: user.into(),
        asset: 1,
    }
    .abi_encode());
    let p = IProbe::positionCall::abi_decode_returns(&out).unwrap();
    assert_eq!(
        (p.szi, p.entryNtl, p.leverage, p.isIsolated),
        (-5, 12_500_000, 0, false)
    );

    // unknown asset -> precompile reverts -> contract reverts
    let r = st.call(
        &env(3),
        None,
        Some(probe),
        &IProbe::markPxCall { asset: 9 }.abi_encode(),
        0,
        None,
        &core,
    );
    assert!(!r.success);
    assert_eq!(r.revert_reason.as_deref(), Some("markPx"));

    // raw precompile call from an EOA via eth_call (ABI input, no selector)
    let r = st.call(
        &env(3),
        None,
        Some(precompiles::MARK_PX_PRECOMPILE),
        &1u32.abi_encode(),
        0,
        None,
        &core,
    );
    assert!(r.success);
    assert_eq!(u64::abi_decode(&r.output).unwrap(), 2_500_000_000);
    assert_eq!(
        r.gas_used,
        21_000 + 16 + 4 * 31 + precompiles::READ_PRECOMPILE_GAS
    );
}

#[test]
fn perpbot_example_contract() {
    let a = signer(3);
    let mut st = EvmState::genesis();
    st.credit_micro_usd(addr(&a), 1_000 * USDC);
    let core = CoreSnapshot {
        mark_pxs: vec![50_000_000_000],
        ..Default::default()
    };

    let res = st.apply_block(
        &env(1),
        &[sign_1559(&a, Tx::create(0, perpbot_code()))],
        &core,
    );
    let bot = res.receipts[0].contract_address.expect("deployed");

    let place = IPerpBot::placeLimitCall {
        asset: 0,
        isBuy: true,
        limitPx1e8: 5_000_000_000_000,
        sz1e8: 1_000_000,
        tif: 2,
    }
    .abi_encode();
    let withdraw = IPerpBot::withdrawToCoreCall {}.abi_encode();
    let res = st.apply_block(
        &env(2),
        &[
            sign_1559(&a, Tx::call(1, bot, 0, place)),
            sign_1559(&a, Tx::call(2, bot, 3 * ONE_USDC_WEI + 5, withdraw)),
        ],
        &core,
    );
    assert!(res.receipts.iter().all(|r| r.success), "{:?}", res);

    let mut expected = vec![1u8, 0, 0, 1];
    // abi.encode(uint32 asset, bool isBuy, uint64 limitPx, uint64 sz, bool reduceOnly, uint8 tif, uint128 cloid)
    for w in [0u128, 1, 5_000_000_000_000, 1_000_000, 0, 2, 0] {
        expected.extend(U256::from(w).to_be_bytes::<32>());
    }
    assert_eq!(res.core_writer_actions.len(), 1);
    assert_eq!(res.core_writer_actions[0].user, bot);
    assert_eq!(res.core_writer_actions[0].data, expected);

    // bridged from the contract: core credits the contract; 5 wei dust stays with the contract
    assert_eq!(res.to_core, vec![(bot, 3 * USDC)]);
    assert_eq!(bal(&st, bot), U256::from(5));
    assert_eq!(bal(&st, USDC_BRIDGE), U256::ZERO);

    let out = call_ok(
        &st,
        bot,
        IPerpBot::markCall { asset: 0 }.abi_encode(),
        &core,
    );
    assert_eq!(
        IPerpBot::markCall::abi_decode_returns(&out).unwrap(),
        50_000_000_000
    );
}

#[test]
fn bridge_to_core() {
    let (a, b) = (signer(1), signer(2));
    let mut st = EvmState::genesis();
    st.credit_micro_usd(addr(&a), 10 * USDC);
    let before = bal(&st, addr(&a));

    let value = 5_500_000u128 * WEI_PER_MICRO_USD + 123; // 5.5 USDC + 123 wei dust
    let raw = sign_1559(
        &a,
        Tx {
            gas_limit: 21_000,
            ..Tx::call(0, USDC_BRIDGE, value, vec![])
        },
    );
    // a tx that fails: B has no funds
    let raw_b = sign_legacy(
        &b,
        Tx {
            gas_limit: 21_000,
            ..Tx::call(0, USDC_BRIDGE, ONE_USDC_WEI, vec![])
        },
    );
    let res = st.apply_block(&env(1), &[raw, raw_b], &CoreSnapshot::default());
    assert_eq!(res.receipts.len(), 1);
    assert_eq!(res.skipped.len(), 1);
    assert!(res.receipts[0].success);
    assert_eq!(res.to_core, vec![(addr(&a), 5_500_000)]);
    let fee = U256::from(21_000u128 * GWEI);
    // value minus dust left the EVM
    assert_eq!(
        bal(&st, addr(&a)),
        before - U256::from(5_500_000u128 * WEI_PER_MICRO_USD) - fee
    );
    assert_eq!(bal(&st, USDC_BRIDGE), U256::ZERO);
    assert!(!st.accounts.contains_key(&USDC_BRIDGE));
}

#[test]
fn determinism_same_txs_same_root() {
    let (a, b) = (signer(1), signer(2));
    let core = CoreSnapshot {
        mark_pxs: vec![1, 2, 3],
        ..Default::default()
    };
    let blocks: Vec<Vec<Vec<u8>>> = vec![
        vec![
            sign_1559(&a, Tx::create(0, probe_code())),
            sign_1559(&a, Tx::call(1, addr(&b), ONE_USDC_WEI, vec![])),
        ],
        vec![
            sign_1559(
                &a,
                Tx::call(
                    2,
                    create_address(addr(&a), 0),
                    0,
                    IProbe::actCall {
                        user: addr(&b).into(),
                        asset: 2,
                        data: vec![9; 40].into(),
                    }
                    .abi_encode(),
                ),
            ),
            sign_legacy(&b, Tx::call(0, USDC_BRIDGE, ONE_USDC_WEI / 2, vec![])),
        ],
    ];
    let run = || {
        let mut st = EvmState::genesis();
        st.credit_micro_usd(addr(&a), 100 * USDC);
        let results: Vec<_> = blocks
            .iter()
            .enumerate()
            .map(|(i, txs)| st.apply_block(&env(i as u64 + 1), txs, &core))
            .collect();
        (st, results)
    };
    let (s1, r1) = run();
    let (s2, r2) = run();
    assert!(
        r1.iter()
            .all(|r| r.skipped.is_empty() && r.receipts.iter().all(|x| x.success)),
        "{:?}",
        r1
    );
    assert_eq!(s1.state_root(), s2.state_root());
    assert_eq!(s1.encode(), s2.encode());
    assert_eq!(s1, s2);
    assert_eq!(r1, r2);
    assert_eq!(r1[1].core_writer_actions.len(), 1);
    assert_eq!(r1[1].to_core, vec![(addr(&b), 500_000)]);
    // state root actually depends on state
    assert_ne!(s1.state_root(), EvmState::genesis().state_root());
    let decoded = EvmState::decode(&s1.encode()).unwrap();
    assert_eq!(decoded.state_root(), s1.state_root());
}

#[test]
fn invalid_txs_are_skipped() {
    let (a, b) = (signer(1), signer(2));
    let mut st = EvmState::genesis();
    st.credit_micro_usd(addr(&a), 10 * USDC);
    let root0 = st.state_root();

    let bad_nonce = sign_1559(&a, Tx::call(5, addr(&b), 1, vec![]));
    let wrong_chain = sign_1559(
        &a,
        Tx {
            chain_id: 1,
            ..Tx::call(0, addr(&b), 1, vec![])
        },
    );
    let no_funds = sign_1559(&b, Tx::call(0, addr(&a), 1, vec![]));
    let too_much_gas = sign_1559(
        &a,
        Tx {
            gas_limit: BLOCK_GAS_LIMIT + 1,
            ..Tx::call(0, addr(&b), 1, vec![])
        },
    );
    let garbage = vec![0x02, 0xc0, 0x01];
    let mut bad_sig = sign_legacy(&a, Tx::call(0, addr(&b), 1, vec![]));
    let n = bad_sig.len();
    bad_sig[n - 3] ^= 0xff; // corrupt s
    assert!(decode_tx(&garbage).is_none());

    let res = st.apply_block(
        &env(1),
        &[
            bad_nonce,
            wrong_chain,
            no_funds,
            too_much_gas,
            garbage,
            bad_sig.clone(),
        ],
        &CoreSnapshot::default(),
    );
    assert!(res.receipts.is_empty(), "{:?}", res.receipts);
    // bad_sig may still decode to *some* signer (with no funds) -> skipped either way
    assert_eq!(res.skipped.len(), 6, "{:?}", res.skipped);
    for (_, why) in &res.skipped {
        assert!(!why.is_empty());
    }
    assert_eq!(st.state_root(), root0);
    assert_eq!(st.nonce(addr(&a)), 0);

    // a valid tx after an invalid one in the same block still executes
    let res = st.apply_block(
        &env(2),
        &[
            sign_1559(&a, Tx::call(1, addr(&b), 1, vec![])),
            sign_1559(&a, Tx::call(0, addr(&b), 1, vec![])),
        ],
        &CoreSnapshot::default(),
    );
    assert_eq!(res.skipped.len(), 1);
    assert_eq!(res.receipts.len(), 1);
    assert_eq!(st.nonce(addr(&a)), 1);
    assert_eq!(bal(&st, addr(&b)), U256::from(1));
}
