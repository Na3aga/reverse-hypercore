//! revm plumbing: read-only database view over [`EvmState`], fee-burning handler and the
//! bridge-transfer tracking inspector.

use std::{convert::Infallible, marker::PhantomData, sync::Arc};

use revm::{
    bytecode::Bytecode,
    context::{
        result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction},
        BlockEnv, CfgEnv, Context, ContextSetters, Evm, FrameStack, Journal, TxEnv,
    },
    context_interface::{ContextTr, JournalTr},
    handler::{instructions::EthInstructions, EthFrame, EvmTr, EvmTrError, FrameTr, Handler},
    inspector::{InspectorEvmTr, InspectorHandler},
    interpreter::{
        interpreter::EthInterpreter, CallInputs, CallOutcome, CreateInputs, CreateOutcome,
    },
    primitives::{hardfork::SpecId, Address, StorageKey, StorageValue, B256, KECCAK_EMPTY, U256},
    state::{AccountInfo, EvmState as RevmState},
    Database, ExecuteEvm, Inspector,
};

use crate::{precompiles::HcPrecompiles, Addr, CoreSnapshot, EvmState, USDC_BRIDGE};

pub(crate) const SPEC: SpecId = SpecId::PRAGUE;

// ---------------------------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------------------------

/// Read-only view of the committed state. Changes come back from revm as a diff and are applied
/// with [`EvmState::commit_changes`].
pub(crate) struct StateDb<'a>(pub &'a EvmState);

impl Database for StateDb<'_> {
    type Error = Infallible;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Infallible> {
        let Some(acc) = self.0.accounts.get(address.as_ref() as &[u8; 20]) else {
            return Ok(None);
        };
        let code_hash = B256::from(acc.code_hash);
        let code = if code_hash == KECCAK_EMPTY {
            Bytecode::default()
        } else {
            self.0
                .code
                .get(&acc.code_hash)
                .map(|c| Bytecode::new_raw(c.clone().into()))
                .unwrap_or_default()
        };
        Ok(Some(AccountInfo::new(
            U256::from_be_bytes(acc.balance),
            acc.nonce,
            code_hash,
            code,
        )))
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Infallible> {
        Ok(self
            .0
            .code
            .get(&code_hash.0)
            .map(|c| Bytecode::new_raw(c.clone().into()))
            .unwrap_or_default())
    }

    fn storage(&mut self, address: Address, index: StorageKey) -> Result<StorageValue, Infallible> {
        let key = (address.into_array(), index.to_be_bytes::<32>());
        Ok(self
            .0
            .storage
            .get(&key)
            .map(|v| U256::from_be_bytes(*v))
            .unwrap_or_default())
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Infallible> {
        Ok(self
            .0
            .block_hashes
            .get(&number)
            .copied()
            .map(B256::from)
            .unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------------------------
// Bridge inspector
// ---------------------------------------------------------------------------------------------

/// Tracks native-value transfers into [`USDC_BRIDGE`] (top-level, from contracts via CALL, and
/// SELFDESTRUCT beneficiaries). Transfers made inside a frame that later reverts are dropped,
/// so `completed` only holds transfers that actually took effect.
#[derive(Default, Debug)]
pub(crate) struct BridgeInspector {
    stack: Vec<Vec<(Addr, U256)>>,
    pub completed: Vec<(Addr, U256)>,
}

impl BridgeInspector {
    fn push_frame(&mut self) {
        self.stack.push(Vec::new());
    }

    fn pop_frame(&mut self, ok: bool) {
        let frame = self.stack.pop().unwrap_or_default();
        if !ok {
            return;
        }
        match self.stack.last_mut() {
            Some(parent) => parent.extend(frame),
            None => self.completed.extend(frame),
        }
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for BridgeInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.push_frame();
        if let Some(value) = inputs.transfer_value() {
            if !value.is_zero() && inputs.transfer_to() == Address::from(USDC_BRIDGE) {
                let from = inputs.transfer_from().into_array();
                self.stack
                    .last_mut()
                    .expect("just pushed")
                    .push((from, value));
            }
        }
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        self.pop_frame(outcome.instruction_result().is_ok());
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.push_frame();
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        self.pop_frame(outcome.instruction_result().is_ok());
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        if !value.is_zero() && target == Address::from(USDC_BRIDGE) {
            if let Some(top) = self.stack.last_mut() {
                top.push((contract.into_array(), value));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Handler: mainnet semantics, but transaction fees are burned (no beneficiary reward).
// ---------------------------------------------------------------------------------------------

pub(crate) struct BurnFeesHandler<EVM> {
    _p: PhantomData<EVM>,
}

impl<EVM> Default for BurnFeesHandler<EVM> {
    fn default() -> Self {
        Self { _p: PhantomData }
    }
}

type HcError = EVMError<Infallible, InvalidTransaction>;

impl<EVM> Handler for BurnFeesHandler<EVM>
where
    EVM: EvmTr<
        Context: ContextTr<Journal: JournalTr<State = RevmState>>,
        Frame = EthFrame<EthInterpreter>,
    >,
    HcError: EvmTrError<EVM>,
{
    type Evm = EVM;
    type Error = HcError;
    type HaltReason = HaltReason;

    fn reward_beneficiary(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        // HyperEVM: base fee and priority fee are both burned. The caller was already charged
        // gas_limit * price upfront and refunded the unused part; nothing goes to coinbase.
        Ok(())
    }
}

impl<EVM> InspectorHandler for BurnFeesHandler<EVM>
where
    EVM: InspectorEvmTr<
        Context: ContextTr<Journal: JournalTr<State = RevmState>>,
        Frame = EthFrame<EthInterpreter>,
        Inspector: Inspector<<EVM as EvmTr>::Context, EthInterpreter>,
    >,
    HcError: EvmTrError<EVM>,
{
    type IT = EthInterpreter;
}

// ---------------------------------------------------------------------------------------------
// Execution entry point
// ---------------------------------------------------------------------------------------------

type HcCtx<'a> = Context<BlockEnv, TxEnv, CfgEnv, StateDb<'a>, Journal<StateDb<'a>>, ()>;
type HcEvm<'a> = Evm<
    HcCtx<'a>,
    BridgeInspector,
    EthInstructions<EthInterpreter, HcCtx<'a>>,
    HcPrecompiles,
    EthFrame<EthInterpreter>,
>;

pub(crate) struct TxOutcome {
    pub result: ExecutionResult<HaltReason>,
    pub changes: RevmState,
    pub bridge_transfers: Vec<(Addr, U256)>,
}

/// Execute one transaction against `state` without mutating it. Returns the result + state diff.
pub(crate) fn execute_tx(
    state: &EvmState,
    block: BlockEnv,
    tx: TxEnv,
    core: Arc<CoreSnapshot>,
    simulate: bool,
) -> Result<TxOutcome, HcError> {
    let ctx: HcCtx<'_> = HcCtx::new(StateDb(state), SPEC)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = crate::CHAIN_ID;
            if simulate {
                // eth_call / estimateGas: no nonce/balance requirements, no chain id needed.
                cfg.disable_nonce_check = true;
                cfg.disable_balance_check = true;
                cfg.disable_block_gas_limit = true;
                cfg.disable_eip3607 = true;
                cfg.tx_chain_id_check = false;
            } else {
                cfg.tx_chain_id_check = true;
            }
        })
        .with_block(block);
    let mut evm: HcEvm<'_> = Evm {
        ctx,
        inspector: BridgeInspector::default(),
        instruction: EthInstructions::new_mainnet_with_spec(SPEC),
        precompiles: HcPrecompiles::new(SPEC, core),
        frame_stack: FrameStack::new_prealloc(8),
    };
    evm.ctx.set_tx(tx);
    let res = BurnFeesHandler::<HcEvm<'_>>::default().inspect_run(&mut evm);
    let changes = evm.finalize();
    let result = res?;
    let bridge_transfers = std::mem::take(&mut evm.inspector.completed);
    Ok(TxOutcome {
        result,
        changes,
        bridge_transfers,
    })
}
