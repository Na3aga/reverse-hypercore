//! Read-only "L1 read" precompiles exposing the [`CoreSnapshot`] to EVM contracts.
//!
//! Convention (same as HyperEVM's L1Read precompiles): the input is the plain ABI encoding of
//! the arguments (no 4-byte selector) and the output is the ABI encoding of the result.
//! Every call costs a fixed [`READ_PRECOMPILE_GAS`]. Malformed input or an unknown asset reverts
//! with empty data. Output depends only on the snapshot, so execution is deterministic.

use std::sync::Arc;

use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{precompile_output_to_interpreter_result, EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::{PrecompileHalt, PrecompileOutput},
    primitives::{hardfork::SpecId, Address, AddressSet, Bytes},
};

use crate::{Addr, CoreSnapshot};

/// Fixed gas charged by every core read precompile.
pub const READ_PRECOMPILE_GAS: u64 = 2_000;

/// `position(address user, uint32 asset)` ->
/// `(int64 szi, uint64 entryNtl, int64 isolatedRawUsd, uint32 leverage, bool isIsolated)`.
pub const POSITION_PRECOMPILE: Addr = precompile_addr(0x0800);
/// `markPx(uint32 asset)` -> `uint64` (micro-USD per coin).
pub const MARK_PX_PRECOMPILE: Addr = precompile_addr(0x0806);
/// `oraclePx(uint32 asset)` -> `uint64` (micro-USD per coin).
pub const ORACLE_PX_PRECOMPILE: Addr = precompile_addr(0x0807);
/// `accountValue(address user)` -> `int64` (micro-USD).
pub const ACCOUNT_VALUE_PRECOMPILE: Addr = precompile_addr(0x080F);
/// `coreBalance(address user)` -> `int64` (micro-USD, raw cross balance).
pub const CORE_BALANCE_PRECOMPILE: Addr = precompile_addr(0x0810);

const fn precompile_addr(n: u16) -> Addr {
    let mut a = [0u8; 20];
    a[18] = (n >> 8) as u8;
    a[19] = n as u8;
    a
}

/// Precompile provider = Ethereum precompiles for the active spec + the core read precompiles.
#[derive(Debug, Clone)]
pub struct HcPrecompiles {
    inner: EthPrecompiles,
    warm: AddressSet,
    core: Arc<CoreSnapshot>,
}

impl HcPrecompiles {
    pub fn new(spec: SpecId, core: Arc<CoreSnapshot>) -> Self {
        let inner = EthPrecompiles::new(spec);
        let mut s = Self {
            inner,
            warm: AddressSet::default(),
            core,
        };
        s.rebuild_warm();
        s
    }

    fn rebuild_warm(&mut self) {
        self.warm.clone_from(self.inner.warm_addresses());
        for a in [
            POSITION_PRECOMPILE,
            MARK_PX_PRECOMPILE,
            ORACLE_PX_PRECOMPILE,
            ACCOUNT_VALUE_PRECOMPILE,
            CORE_BALANCE_PRECOMPILE,
        ] {
            self.warm.insert(Address::from(a));
        }
    }
}

impl<CTX> PrecompileProvider<CTX> for HcPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = SpecId>>,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: SpecId) -> bool {
        let changed = <EthPrecompiles as PrecompileProvider<CTX>>::set_spec(&mut self.inner, spec);
        self.rebuild_warm();
        changed
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let addr: Addr = inputs.bytecode_address.into_array();
        let handler: fn(&CoreSnapshot, &[u8]) -> Option<Vec<u8>> = match addr {
            POSITION_PRECOMPILE => read_position,
            MARK_PX_PRECOMPILE => read_mark_px,
            ORACLE_PX_PRECOMPILE => read_oracle_px,
            ACCOUNT_VALUE_PRECOMPILE => read_account_value,
            CORE_BALANCE_PRECOMPILE => read_core_balance,
            _ => {
                return <EthPrecompiles as PrecompileProvider<CTX>>::run(
                    &mut self.inner,
                    context,
                    inputs,
                )
            }
        };
        let output = if inputs.gas_limit < READ_PRECOMPILE_GAS {
            PrecompileOutput::halt(PrecompileHalt::OutOfGas, inputs.reservoir)
        } else {
            let input = inputs.input.bytes(context);
            match handler(&self.core, &input) {
                Some(out) => {
                    PrecompileOutput::new(READ_PRECOMPILE_GAS, Bytes::from(out), inputs.reservoir)
                }
                None => {
                    PrecompileOutput::revert(READ_PRECOMPILE_GAS, Bytes::new(), inputs.reservoir)
                }
            }
        };
        Ok(Some(precompile_output_to_interpreter_result(
            output,
            inputs.gas_limit,
        )))
    }

    fn warm_addresses(&self) -> &AddressSet {
        &self.warm
    }

    fn contains(&self, address: &Address) -> bool {
        self.warm.contains(address)
    }
}

// ---------------------------------------------------------------------------------------------
// ABI helpers
// ---------------------------------------------------------------------------------------------

fn word(input: &[u8], i: usize) -> Option<&[u8; 32]> {
    input.get(i * 32..i * 32 + 32)?.try_into().ok()
}

fn arg_u32(input: &[u8], i: usize) -> Option<u32> {
    let w = word(input, i)?;
    if w[..28].iter().any(|b| *b != 0) {
        return None;
    }
    Some(u32::from_be_bytes(w[28..].try_into().unwrap()))
}

fn arg_addr(input: &[u8], i: usize) -> Option<Addr> {
    let w = word(input, i)?;
    if w[..12].iter().any(|b| *b != 0) {
        return None;
    }
    Some(w[12..].try_into().unwrap())
}

fn enc_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&[0u8; 24]);
    out.extend_from_slice(&v.to_be_bytes());
}

fn enc_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&[if v < 0 { 0xff } else { 0 }; 24]);
    out.extend_from_slice(&v.to_be_bytes());
}

fn read_position(core: &CoreSnapshot, input: &[u8]) -> Option<Vec<u8>> {
    let user = arg_addr(input, 0)?;
    let asset = arg_u32(input, 1)?;
    let (szi, entry_ntl) = core
        .positions
        .get(&(user, asset))
        .copied()
        .unwrap_or((0, 0));
    let mut out = Vec::with_capacity(5 * 32);
    enc_i64(&mut out, szi);
    enc_u64(&mut out, entry_ntl.unsigned_abs());
    enc_i64(&mut out, 0); // isolatedRawUsd
    enc_u64(&mut out, 0); // leverage (uint32)
    enc_u64(&mut out, 0); // isIsolated (bool)
    Some(out)
}

fn read_px(pxs: &[u64], input: &[u8]) -> Option<Vec<u8>> {
    let asset = arg_u32(input, 0)?;
    let px = *pxs.get(asset as usize)?;
    let mut out = Vec::with_capacity(32);
    enc_u64(&mut out, px);
    Some(out)
}

fn read_mark_px(core: &CoreSnapshot, input: &[u8]) -> Option<Vec<u8>> {
    read_px(&core.mark_pxs, input)
}

fn read_oracle_px(core: &CoreSnapshot, input: &[u8]) -> Option<Vec<u8>> {
    read_px(&core.oracle_pxs, input)
}

fn read_account_value(core: &CoreSnapshot, input: &[u8]) -> Option<Vec<u8>> {
    let user = arg_addr(input, 0)?;
    let mut out = Vec::with_capacity(32);
    enc_i64(
        &mut out,
        core.account_values.get(&user).copied().unwrap_or(0),
    );
    Some(out)
}

fn read_core_balance(core: &CoreSnapshot, input: &[u8]) -> Option<Vec<u8>> {
    let user = arg_addr(input, 0)?;
    let mut out = Vec::with_capacity(32);
    enc_i64(
        &mut out,
        core.core_balances.get(&user).copied().unwrap_or(0),
    );
    Some(out)
}
