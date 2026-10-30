//! The exchange state machine: `(state, block) -> state'`.
//!
//! Block execution has the same phase structure observed in hl-node
//! (`BeginBlock`, `DeliverSignedActions`, `Commit`):
//! 1. **begin block**: advance height and a monotonic block time;
//! 2. **deliver**: stable-sort transactions by priority category (non-GTC/IOC
//!    actions, then cancels, then GTC/IOC orders), then verify the signature
//!    and nonce of each one and apply it;
//! 3. **end block**: aggregate oracle votes and update mark prices, fire
//!    TP/SL triggers, send due TWAP slices, sample
//!    funding premiums every 5 s, settle funding hourly, run liquidations;
//! 4. **EVM blocks** (HyperEVM-like, via `hc-evm`/revm): the block's raw
//!    Ethereum transactions join the small- or big-block queue; when a small
//!    (every second) or big (every minute) EVM block is due, it runs queued
//!    transactions against a snapshot of core state (read precompiles), then
//!    value sent to the bridge address returns to core balances, CoreWriter
//!    cancels run and CoreWriter orders are queued; delayed CoreWriter orders
//!    whose time has come run as core actions;
//! 5. **commit**: hash the canonical encoding of the state, EVM state included.
//!
//! Steps 4 and 3 run in that order: EVM blocks come before end-block work,
//! so CoreWriter orders are subject to the same liquidation checks.

use crate::action::{priority_category, Action, CancelByCloidWire, CancelWire, OrderWire, SignedTx};
use crate::book::{Book, RestingOrder};
use crate::clearinghouse::{self, apply_fill, AssetMeta, MarginCtx, UserState};
use crate::events::{Event, FillEvent, LedgerKind, LiquidationMethod, OrderEvent, OrderResponse, TxResponse};
use crate::fixed::{self, ntl, parse_decimal, Px, Rate, Usd, RATE_SCALE};
use crate::funding::{self, FundingState};
use crate::oracle::{self, OracleVote, PriceState};
use crate::types::{Address, AssetId, Hash32, Oid, OrderStatus, Side, Tif};
use crate::evm_bridge;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Params {
    pub taker_fee: Rate,
    pub maker_fee: Rate,
    pub min_order_ntl: Usd,
    /// Orders priced more than this percentage away from the oracle are rejected.
    pub oracle_band_pct: u32,
    pub max_open_orders: usize,
    pub nonce_set_size: usize,
    /// Nonces must lie in `(time - back, time + ahead)`.
    pub nonce_window_back_ms: u64,
    pub nonce_window_ahead_ms: u64,
    pub oracle_max_age_ms: u64,
    pub premium_sample_ms: u64,
    pub funding_interval_ms: u64,
    pub funding_interest_8h: Rate,
    pub funding_clamp: Rate,
    pub funding_cap_hourly: Rate,
    /// Positions above this notional are liquidated in chunks.
    pub partial_liq_threshold: Usd,
    /// Fraction of the position sent per partial liquidation (percent).
    pub partial_liq_pct: u32,
    pub partial_liq_cooldown_ms: u64,
    /// Price slippage applied to market liquidation orders.
    pub liq_slippage: Rate,
    /// Max faucet amount per call (0 disables the faucet).
    pub faucet_max: Usd,
}

impl Default for Params {
    fn default() -> Params {
        let r = RATE_SCALE as i64;
        Params {
            taker_fee: funding::rate_bps(450),
            maker_fee: funding::rate_bps(150),
            min_order_ntl: Usd::from_whole(10),
            oracle_band_pct: 80,
            max_open_orders: 1000,
            nonce_set_size: 100,
            nonce_window_back_ms: 2 * 86_400_000,
            nonce_window_ahead_ms: 86_400_000,
            oracle_max_age_ms: 10_000,
            premium_sample_ms: 5_000,
            funding_interval_ms: 3_600_000,
            funding_interest_8h: Rate(r / 10_000),
            funding_clamp: Rate(5 * r / 10_000),
            funding_cap_hourly: Rate(4 * r / 100),
            partial_liq_threshold: Usd::from_whole(10_000),
            partial_liq_pct: 20,
            partial_liq_cooldown_ms: 30_000,
            liq_slippage: Rate(r / 100),
            faucet_max: Usd::from_whole(100_000),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validator {
    /// Address of the validator's oracle-signing key.
    pub address: Address,
    pub stake: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Genesis {
    pub time_ms: u64,
    pub assets: Vec<AssetMeta>,
    pub oracle_pxs: Vec<Px>,
    pub validators: Vec<Validator>,
    pub balances: Vec<(Address, Usd)>,
    pub liquidator_balance: Usd,
    pub params: Params,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetState {
    pub prices: PriceState,
    pub funding: FundingState,
    /// Sum of long position sizes in lots.
    pub open_interest: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub height: u64,
    pub time_ms: u64,
    pub params: Params,
    pub metas: Vec<AssetMeta>,
    pub assets: Vec<AssetState>,
    pub books: Vec<Book>,
    pub users: BTreeMap<Address, UserState>,
    pub validators: Vec<Validator>,
    pub oracle_votes: BTreeMap<u32, OracleVote>,
    pub next_oid: Oid,
    pub last_premium_sample: u64,
    pub last_funding_period: u64,
    /// HyperEVM-like world state (accounts, code, storage), part of the state hash.
    pub evm: hc_evm::EvmState,
    /// USDC created (genesis balances + faucet), for supply accounting.
    pub total_minted: Usd,
    /// USDC currently on the EVM side (bridged in minus bridged out).
    pub net_bridged_to_evm: Usd,
    /// Accounts with risk limits, checked at the end of every block.
    pub risk_users: std::collections::BTreeSet<Address>,
    /// Running TWAP orders by id.
    pub twaps: BTreeMap<u64, crate::twap::Twap>,
    pub next_twap_id: u64,
    /// Finalized EVM transactions waiting for a small or a big EVM block.
    pub evm_queue_small: std::collections::VecDeque<evm_bridge::QueuedEvmTx>,
    pub evm_queue_big: std::collections::VecDeque<evm_bridge::QueuedEvmTx>,
    /// Second and minute of the last small and big EVM block slot.
    pub last_small_evm_slot: u64,
    pub last_big_evm_slot: u64,
    /// Accounts whose EVM transactions go to big blocks.
    pub big_block_users: std::collections::BTreeSet<Address>,
    /// CoreWriter orders waiting out their delay, in the order they were sent.
    pub core_writer_queue: std::collections::VecDeque<evm_bridge::DelayedCoreWriter>,
    /// Approved agent (API wallet) -> the account it signs for.
    pub agents: BTreeMap<Address, Agent>,
}

/// An agent key approved by an account (`approveAgent`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    pub owner: Address,
    pub name: String,
}

/// Most agents one account may have approved.
pub const MAX_AGENTS: usize = 4;

pub fn fee_collector() -> Address {
    Address::system("fee-collector")
}

pub fn liquidator() -> Address {
    Address::system("liquidator")
}

/// Everything a block produces besides the new state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BlockOutput {
    pub height: u64,
    pub time_ms: u64,
    pub responses: Vec<(Hash32, Address, TxResponse)>,
    pub events: Vec<Event>,
    /// Events caused by each transaction: (tx hash, start, end) into `events`,
    /// core transactions in delivery order, then EVM transactions and their
    /// CoreWriter actions. Events outside every span come from end-of-block
    /// processing (oracle update, triggers, funding, risk, liquidations).
    pub spans: Vec<(Hash32, u32, u32)>,
    pub state_hash: Hash32,
}

/// Order origin. Liquidation orders skip user-facing checks; triggered TP/SL
/// orders and risk auto-closes get the same checks as user orders.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    User,
    Liquidation,
    Trigger,
}

/// Validated order parameters.
pub(crate) struct OrderReq {
    pub(crate) asset: AssetId,
    pub(crate) side: Side,
    pub(crate) px: Px,
    pub(crate) lots: u64,
    pub(crate) reduce_only: bool,
    pub(crate) tif: Tif,
    pub(crate) cloid: Option<crate::types::Cloid>,
}

impl State {
    pub fn genesis(g: &Genesis) -> State {
        assert_eq!(g.assets.len(), g.oracle_pxs.len());
        let mut users: BTreeMap<Address, UserState> = BTreeMap::new();
        for (addr, usd) in &g.balances {
            users.entry(*addr).or_default().balance += *usd;
        }
        users.entry(liquidator()).or_default().balance += g.liquidator_balance;
        users.entry(fee_collector()).or_default();
        State {
            height: 0,
            time_ms: g.time_ms,
            params: g.params.clone(),
            metas: g.assets.clone(),
            assets: g
                .oracle_pxs
                .iter()
                .map(|px| AssetState { prices: PriceState::new(*px), funding: FundingState::default(), open_interest: 0 })
                .collect(),
            books: vec![Book::default(); g.assets.len()],
            users,
            validators: g.validators.clone(),
            oracle_votes: BTreeMap::new(),
            next_oid: 1,
            last_premium_sample: g.time_ms / g.params.premium_sample_ms,
            last_funding_period: g.time_ms / g.params.funding_interval_ms,
            evm: hc_evm::EvmState::genesis(),
            total_minted: g.balances.iter().fold(g.liquidator_balance, |acc, (_, u)| acc + *u),
            net_bridged_to_evm: Usd::ZERO,
            risk_users: Default::default(),
            twaps: BTreeMap::new(),
            next_twap_id: 1,
            evm_queue_small: Default::default(),
            evm_queue_big: Default::default(),
            last_small_evm_slot: g.time_ms / 1_000,
            last_big_evm_slot: g.time_ms / 60_000,
            big_block_users: Default::default(),
            core_writer_queue: Default::default(),
            agents: BTreeMap::new(),
        }
    }

    pub fn hash(&self) -> Hash32 {
        Hash32(*blake3::hash(&postcard::to_stdvec(self).expect("state encodes")).as_bytes())
    }

    fn marks(&self) -> Vec<Px> {
        self.assets.iter().map(|a| a.prices.mark_px).collect()
    }

    fn default_leverage(&self) -> Vec<u32> {
        self.metas.iter().map(|m| m.max_leverage.min(20)).collect()
    }

    /// Runs `f` with a margin context over current marks.
    pub fn with_margin_ctx<R>(&self, f: impl FnOnce(&MarginCtx) -> R) -> R {
        let marks = self.marks();
        let lev = self.default_leverage();
        f(&MarginCtx { metas: &self.metas, marks: &marks, default_leverage: &lev })
    }

    /// Executes one block of core actions only (see [`State::apply_block_with_evm`]).
    pub fn apply_block(&mut self, height: u64, proposer_time_ms: u64, txs: &[SignedTx]) -> BlockOutput {
        self.apply_block_with_evm(height, proposer_time_ms, txs, &[])
    }

    /// Executes one block, recovering signatures in parallel first.
    pub fn apply_block_with_evm(&mut self, height: u64, proposer_time_ms: u64, txs: &[SignedTx], evm_txs: &[Vec<u8>]) -> BlockOutput {
        let signers = crate::action::recover_signers(txs);
        self.apply_block_with_signers(height, proposer_time_ms, txs, &signers, evm_txs)
    }

    /// Executes one block. `proposer_time_ms` is clamped to be monotonic.
    /// `signers[i]` must be `txs[i].signer()`; callers pass it so that recovery
    /// can run in parallel or come from a cache (it is a pure function of the
    /// transaction, so caching cannot change the result).
    pub fn apply_block_with_signers(
        &mut self,
        height: u64,
        proposer_time_ms: u64,
        txs: &[SignedTx],
        signers: &[Option<Address>],
        evm_txs: &[Vec<u8>],
    ) -> BlockOutput {
        assert_eq!(signers.len(), txs.len(), "one signer per transaction");
        // Begin block.
        self.height = height;
        self.time_ms = self.time_ms.max(proposer_time_ms);
        let mut out = BlockOutput { height, time_ms: self.time_ms, ..Default::default() };

        // Deliver: stable sort by priority category keeps proposer order within a category.
        let mut order: Vec<(&SignedTx, Option<Address>)> = txs.iter().zip(signers.iter().copied()).collect();
        order.sort_by_key(|(tx, _)| priority_category(&tx.action));
        let mut oracle_voted = false;
        for (tx, signer) in order {
            let start = out.events.len() as u32;
            let (user, resp) = match signer {
                Some(signer) => self.deliver(signer, tx, &mut out.events, &mut oracle_voted),
                None => (Address::default(), TxResponse::err("Invalid signature.")),
            };
            let hash = tx.hash();
            out.spans.push((hash, start, out.events.len() as u32));
            out.responses.push((hash, user, resp));
        }

        // EVM transactions join their queue; due EVM blocks run, then delayed CoreWriter orders.
        self.queue_evm_txs(evm_txs, &mut out.events, &mut out.spans);
        self.run_evm_blocks(&mut out.events, &mut out.spans);
        self.run_delayed_core_writer(&mut out.events);

        // End block.
        if oracle_voted {
            self.update_oracle(&mut out.events);
        }
        self.run_triggers(&mut out.events);
        self.run_twaps(&mut out.events);
        self.sample_premiums();
        self.settle_funding(&mut out.events);
        self.run_risk(&mut out.events);
        self.run_liquidations(&mut out.events);

        // Commit.
        out.state_hash = self.hash();
        out
    }

    /// Read-only view of core state for EVM precompiles.
    pub fn core_snapshot(&self) -> hc_evm::CoreSnapshot {
        self.with_margin_ctx(|ctx| {
            let mut snap = hc_evm::CoreSnapshot {
                mark_pxs: ctx.marks.iter().map(|p| p.0).collect(),
                oracle_pxs: self.assets.iter().map(|a| a.prices.oracle_px.0).collect(),
                sz_decimals: self.metas.iter().map(|m| m.sz_decimals).collect(),
                ..Default::default()
            };
            for (addr, u) in &self.users {
                for (asset, p) in &u.positions {
                    snap.positions.insert((addr.0, *asset), (p.szi, p.entry_ntl));
                }
                snap.account_values.insert(addr.0, u.total_account_value(ctx).0);
                snap.core_balances.insert(addr.0, u.balance.0);
            }
            snap
        })
    }

    /// Puts finalized EVM transactions in their sender's queue. Ones that
    /// cannot run (undecodable, wrong chain, over the block's gas limit) are
    /// skipped here.
    fn queue_evm_txs(&mut self, evm_txs: &[Vec<u8>], events: &mut Vec<Event>, spans: &mut Vec<(Hash32, u32, u32)>) {
        for raw in evm_txs {
            let start = events.len() as u32;
            let decoded = hc_evm::decode_tx(raw);
            let hash = Hash32(decoded.as_ref().map_or_else(|| crate::action::keccak256(raw), |d| d.tx_hash));
            match decoded {
                None => events.push(Event::EvmTxSkipped { tx_hash: hash, reason: "undecodable transaction".into() }),
                Some(d) if d.chain_id != hc_evm::CHAIN_ID => events.push(Event::EvmTxSkipped { tx_hash: hash, reason: format!("wrong chain id {}", d.chain_id) }),
                Some(d) => {
                    let big = self.big_block_users.contains(&Address(d.from));
                    let limit = if big { hc_evm::BLOCK_GAS_LIMIT } else { hc_evm::SMALL_BLOCK_GAS_LIMIT };
                    if d.gas_limit > limit {
                        let reason = if big {
                            format!("gas limit {} is above the big block limit {limit}", d.gas_limit)
                        } else {
                            format!("gas limit {} is above the small block limit {limit}; send evmUserModify {{usingBigBlocks: true}} to use big blocks", d.gas_limit)
                        };
                        events.push(Event::EvmTxSkipped { tx_hash: hash, reason });
                    } else {
                        let q = evm_bridge::QueuedEvmTx { hash, raw: crate::action::RawEvmTx(raw.clone()), gas_limit: d.gas_limit };
                        if big { self.evm_queue_big.push_back(q) } else { self.evm_queue_small.push_back(q) }
                        events.push(Event::EvmTxQueued { tx_hash: hash, big });
                    }
                }
            }
            spans.push((hash, start, events.len() as u32));
        }
    }

    /// Runs the small EVM block when a new second starts and the big one when
    /// a new minute starts, if their queues hold transactions.
    fn run_evm_blocks(&mut self, events: &mut Vec<Event>, spans: &mut Vec<(Hash32, u32, u32)>) {
        let (second, minute) = (self.time_ms / 1_000, self.time_ms / 60_000);
        if second > self.last_small_evm_slot {
            self.last_small_evm_slot = second;
            if !self.evm_queue_small.is_empty() {
                self.run_evm_block(false, events, spans);
            }
        }
        if minute > self.last_big_evm_slot {
            self.last_big_evm_slot = minute;
            if !self.evm_queue_big.is_empty() {
                self.run_evm_block(true, events, spans);
            }
        }
    }

    fn run_evm_block(&mut self, big: bool, events: &mut Vec<Event>, spans: &mut Vec<(Hash32, u32, u32)>) {
        let gas_limit = if big { hc_evm::BLOCK_GAS_LIMIT } else { hc_evm::SMALL_BLOCK_GAS_LIMIT };
        // Oldest first, while their gas limits fit; the rest waits for the next block.
        let queue = if big { &mut self.evm_queue_big } else { &mut self.evm_queue_small };
        let mut txs = Vec::new();
        let mut gas = 0;
        while let Some(q) = queue.front() {
            if gas + q.gas_limit > gas_limit {
                break;
            }
            gas += q.gas_limit;
            txs.push(queue.pop_front().expect("front exists").raw.0);
        }
        let snapshot = self.core_snapshot();
        let env = hc_evm::EvmBlockEnv { number: self.evm.block_number() + 1, timestamp_s: self.time_ms / 1000, coinbase: [0u8; 20], gas_limit };
        let result = self.evm.apply_block(&env, &txs, &snapshot);
        for r in &result.receipts {
            spans.push((Hash32(r.tx_hash), events.len() as u32, events.len() as u32 + 1));
            events.push(Event::EvmTx(evm_bridge::receipt_summary(r)));
        }
        for (hash, reason) in &result.skipped {
            spans.push((Hash32(*hash), events.len() as u32, events.len() as u32 + 1));
            events.push(Event::EvmTxSkipped { tx_hash: Hash32(*hash), reason: reason.clone() });
        }
        // Bridge out of the EVM: value sent to the bridge address returns to core.
        for (addr, micro_usd) in &result.to_core {
            let usd = Usd(*micro_usd as i64);
            self.users.entry(Address(*addr)).or_default().balance += usd;
            self.net_bridged_to_evm -= usd;
            self.risk_baseline(Address(*addr), usd);
            events.push(Event::Ledger { user: Address(*addr), kind: LedgerKind::EvmToCore, usd });
        }
        // CoreWriter: cancels run now; orders wait out the delay.
        for a in &result.core_writer_actions {
            let start = events.len() as u32;
            let (user, tx_hash) = (Address(a.user), Hash32(a.tx_hash));
            match evm_bridge::decode_core_writer(&a.data, &self.metas) {
                Ok(evm_bridge::CoreWriterCall::Order(_)) => {
                    let execute_at_ms = self.time_ms + evm_bridge::CORE_WRITER_DELAY_MS;
                    self.core_writer_queue.push_back(evm_bridge::DelayedCoreWriter { execute_at_ms, user, data: a.data.clone(), tx_hash });
                    events.push(Event::CoreWriterQueued { user, tx_hash, execute_at_ms });
                }
                _ => self.run_core_writer(user, &a.data, tx_hash, events),
            }
            spans.push((tx_hash, start, events.len() as u32));
        }
        events.push(Event::EvmBlock { number: result.number, hash: Hash32(result.block_hash), n_txs: result.receipts.len(), gas_used: result.gas_used, big });
    }

    /// Runs one CoreWriter action on core as its sender (the EVM account).
    fn run_core_writer(&mut self, user: Address, data: &[u8], tx_hash: Hash32, events: &mut Vec<Event>) {
        let (asset, response) = match evm_bridge::decode_core_writer(data, &self.metas) {
            Ok(evm_bridge::CoreWriterCall::Order(o)) => {
                let req = OrderReq { asset: o.asset, side: o.side, px: o.px, lots: o.lots, reduce_only: o.reduce_only, tif: o.tif, cloid: None };
                (Some(o.asset), self.place(user, req, Origin::User, events))
            }
            Ok(evm_bridge::CoreWriterCall::Cancel { asset, oid }) => (Some(asset), self.cancel(user, &CancelWire { a: asset, o: oid }, events)),
            Err(e) => (None, OrderResponse::Error(e)),
        };
        events.push(Event::CoreWriter { user, asset, response, tx_hash });
    }

    /// CoreWriter orders whose delay has passed, in the order they were sent.
    fn run_delayed_core_writer(&mut self, events: &mut Vec<Event>) {
        while self.core_writer_queue.front().is_some_and(|d| d.execute_at_ms <= self.time_ms) {
            let d = self.core_writer_queue.pop_front().expect("front exists");
            self.run_core_writer(d.user, &d.data, d.tx_hash, events);
        }
    }

    /// Applies one transaction. Nonces are tracked per signer; an approved
    /// agent's actions apply to its owner's account. Returns the account the
    /// transaction acted for, and its response.
    fn deliver(&mut self, signer: Address, tx: &SignedTx, events: &mut Vec<Event>, oracle_voted: &mut bool) -> (Address, TxResponse) {
        if let Err(e) = self.check_nonce(signer, tx.nonce) {
            return (signer, TxResponse::err(e));
        }
        let user = self.agents.get(&signer).map_or(signer, |a| a.owner);
        let resp = match &tx.action {
            Action::ApproveAgent { .. } if user != signer => TxResponse::err("Agents cannot approve agents."),
            Action::UsdSend { .. } if user != signer => TxResponse::err("Agents cannot send funds; sign transfers with the account's own key."),
            Action::SetOracle { .. } if user != signer => TxResponse::err("Agents cannot vote for validators."),
            action => self.apply_action(user, tx.nonce, action, events, oracle_voted),
        };
        (user, resp)
    }

    fn apply_action(&mut self, user: Address, nonce: u64, action: &Action, events: &mut Vec<Event>, oracle_voted: &mut bool) -> TxResponse {
        match action {
            Action::Order { orders, grouping } => TxResponse::Order { statuses: self.order_action(user, orders, *grouping, events) },
            Action::Cancel { cancels } => {
                let statuses = cancels.iter().map(|c| self.cancel(user, c, events)).collect();
                TxResponse::Cancel { statuses }
            }
            Action::CancelByCloid { cancels } => {
                let statuses = cancels.iter().map(|c| self.cancel_by_cloid(user, c, events)).collect();
                TxResponse::Cancel { statuses }
            }
            Action::UpdateLeverage { asset, is_cross, leverage } => self.update_leverage(user, *asset, *is_cross, *leverage),
            Action::UpdateIsolatedMargin { asset, is_buy: _, ntli } => self.update_isolated_margin(user, *asset, *ntli, events),
            Action::TwapOrder { twap } => self.twap_order(user, twap, events),
            Action::TwapCancel { a, t } => self.twap_cancel(user, *a, *t, events),
            Action::ApproveAgent { hyperliquid_chain, agent_address, agent_name, nonce: action_nonce, .. } => {
                self.approve_agent(user, nonce, hyperliquid_chain, *agent_address, agent_name, *action_nonce)
            }
            Action::UsdSend { hyperliquid_chain, destination, amount, time, .. } => self.usd_send(user, nonce, hyperliquid_chain, destination, amount, *time, events),
            Action::EvmUserModify { using_big_blocks } => {
                if *using_big_blocks {
                    self.big_block_users.insert(user);
                } else {
                    self.big_block_users.remove(&user);
                }
                TxResponse::Default
            }
            Action::Faucet { usd } => self.faucet(user, usd, events),
            Action::SendToEvm { usd } => self.send_to_evm(user, usd, events),
            Action::SetRiskLimits { max_position_usd, max_leverage, daily_loss_usd, close_at_margin_pct } => {
                self.set_risk_limits(user, max_position_usd.as_deref(), *max_leverage, daily_loss_usd.as_deref(), *close_at_margin_pct)
            }
            Action::SetOracle { pxs } => {
                let r = self.oracle_vote(user, pxs);
                if matches!(r, TxResponse::Default) {
                    *oracle_voted = true;
                }
                r
            }
        }
    }

    fn approve_agent(&mut self, user: Address, tx_nonce: u64, chain: &str, agent: Address, name: &str, action_nonce: u64) -> TxResponse {
        if chain != crate::action::HYPERLIQUID_CHAIN {
            return TxResponse::err(format!("Wrong hyperliquidChain {chain:?}; this network is {:?}.", crate::action::HYPERLIQUID_CHAIN));
        }
        if action_nonce != tx_nonce {
            return TxResponse::err("Action nonce does not match the transaction nonce.");
        }
        if name.len() > 16 {
            return TxResponse::err("Agent name is longer than 16 characters.");
        }
        // An address that holds funds, positions or orders is an account of its own.
        let in_use = self.users.get(&agent).is_some_and(|u| u.balance != Usd::ZERO || !u.positions.is_empty() || !u.open_orders.is_empty());
        if agent == user || self.agents.get(&agent).is_some_and(|a| a.owner != user) || in_use {
            return TxResponse::err("Agent address is already an account or another account's agent.");
        }
        // The same name replaces the earlier agent.
        self.agents.retain(|_, a| !(a.owner == user && a.name == name));
        if self.agents.values().filter(|a| a.owner == user).count() >= MAX_AGENTS {
            return TxResponse::err(format!("At most {MAX_AGENTS} agents per account."));
        }
        self.agents.insert(agent, Agent { owner: user, name: name.to_string() });
        TxResponse::Default
    }

    /// Moves USDC from `user`'s withdrawable balance to `destination`'s balance.
    #[allow(clippy::too_many_arguments)]
    fn usd_send(&mut self, user: Address, tx_nonce: u64, chain: &str, destination: &str, amount: &str, time: u64, events: &mut Vec<Event>) -> TxResponse {
        if chain != crate::action::HYPERLIQUID_CHAIN {
            return TxResponse::err(format!("Wrong hyperliquidChain {chain:?}; this network is {:?}.", crate::action::HYPERLIQUID_CHAIN));
        }
        if time != tx_nonce {
            return TxResponse::err("Action time does not match the transaction nonce.");
        }
        let Some(to) = destination.strip_prefix("0x").and_then(crate::types::hex_decode).and_then(|b| <[u8; 20]>::try_from(b).ok()).map(Address) else {
            return TxResponse::err("Invalid destination address.");
        };
        let Some(v) = parse_decimal(amount, 6).filter(|v| *v > 0).and_then(|v| i64::try_from(v).ok()) else {
            return TxResponse::err("Invalid amount.");
        };
        if to == user {
            return TxResponse::err("Cannot send to yourself.");
        }
        let withdrawable = self.with_margin_ctx(|ctx| self.users.get(&user).map(|u| u.withdrawable(ctx)).unwrap_or_default());
        if Usd(v) > withdrawable {
            return TxResponse::err("Insufficient withdrawable balance.");
        }
        self.users.entry(user).or_default().balance -= Usd(v);
        self.users.entry(to).or_default().balance += Usd(v);
        self.risk_baseline(user, Usd(-v));
        self.risk_baseline(to, Usd(v));
        events.push(Event::Ledger { user, kind: LedgerKind::Send { counterparty: to }, usd: Usd(-v) });
        events.push(Event::Ledger { user: to, kind: LedgerKind::Send { counterparty: user }, usd: Usd(v) });
        TxResponse::Default
    }

    /// Bridge into the EVM: debit withdrawable core balance, credit native EVM balance.
    fn send_to_evm(&mut self, user: Address, usd: &str, events: &mut Vec<Event>) -> TxResponse {
        let Some(v) = parse_decimal(usd, 6).filter(|v| *v > 0).and_then(|v| i64::try_from(v).ok()) else {
            return TxResponse::err("Invalid amount.");
        };
        let withdrawable = self.with_margin_ctx(|ctx| self.users.get(&user).map(|u| u.withdrawable(ctx)).unwrap_or_default());
        if Usd(v) > withdrawable {
            return TxResponse::err("Insufficient withdrawable balance.");
        }
        self.users.entry(user).or_default().balance -= Usd(v);
        self.evm.credit_micro_usd(user.0, v as u128);
        self.net_bridged_to_evm += Usd(v);
        self.risk_baseline(user, Usd(-v));
        events.push(Event::Ledger { user, kind: LedgerKind::CoreToEvm, usd: Usd(v) });
        TxResponse::Default
    }

    /// Keeps the `nonce_set_size` highest nonces per signer. A new nonce must
    /// be unused, larger than the smallest kept nonce once the set is full,
    /// and within the time window around block time.
    fn check_nonce(&mut self, user: Address, nonce: u64) -> Result<(), String> {
        let p = &self.params;
        let lo = self.time_ms.saturating_sub(p.nonce_window_back_ms);
        let hi = self.time_ms + p.nonce_window_ahead_ms;
        if nonce <= lo || nonce >= hi {
            return Err(format!("Invalid nonce: {nonce} outside time window."));
        }
        let size = p.nonce_set_size;
        let u = self.users.entry(user).or_default();
        if u.nonces.contains(&nonce) {
            return Err("Invalid nonce: duplicate nonce.".into());
        }
        if u.nonces.len() >= size && nonce <= *u.nonces.first().expect("non-empty") {
            return Err("Invalid nonce: nonce too low.".into());
        }
        u.nonces.insert(nonce);
        while u.nonces.len() > size {
            u.nonces.pop_first();
        }
        Ok(())
    }

    // ---- Orders ----------------------------------------------------------

    pub(crate) fn parse_order(&self, o: &OrderWire) -> Result<OrderReq, (OrderStatus, String)> {
        let meta = self.metas.get(o.a as usize).ok_or((OrderStatus::TickRejected, "Unknown asset.".into()))?;
        let invalid_px = || (OrderStatus::TickRejected, "Order has invalid price.".to_string());
        let px = parse_decimal(&o.p, 6).filter(|v| *v > 0).ok_or_else(invalid_px)?;
        let px = Px(u64::try_from(px).map_err(|_| invalid_px())?);
        let lots = parse_decimal(&o.s, meta.sz_decimals)
            .filter(|v| *v > 0)
            .and_then(|v| u64::try_from(v).ok())
            .ok_or((OrderStatus::TickRejected, "Order has invalid size.".into()))?;
        if !fixed::is_valid_px(px, meta.sz_decimals) {
            return Err((OrderStatus::TickRejected, "Price must be divisible by tick size.".into()));
        }
        Ok(OrderReq { asset: o.a, side: Side::from_is_buy(o.b), px, lots, reduce_only: o.r, tif: o.t.tif().unwrap_or(Tif::Gtc), cloid: o.c })
    }

    /// The orders of one `order` action, in order. With `normalTpsl`
    /// grouping, reduce-only triggers that follow a limit order are its TP/SL:
    /// they wait for that entry to fill, and are rejected if the entry was
    /// rejected. Otherwise every order stands alone.
    fn order_action(&mut self, user: Address, orders: &[OrderWire], grouping: crate::action::Grouping, events: &mut Vec<Event>) -> Vec<OrderResponse> {
        let linked = grouping == crate::action::Grouping::NormalTpsl;
        let mut entry: Option<Result<Option<Oid>, ()>> = None;
        let mut out = Vec::with_capacity(orders.len());
        for o in orders {
            let r = match &o.t {
                crate::action::OrderTypeWire::Limit(_) => {
                    let r = match self.parse_order(o) {
                        Ok(req) => self.place(user, req, Origin::User, events),
                        Err((_, msg)) => OrderResponse::Error(msg),
                    };
                    if entry.is_none() {
                        entry = Some(match &r {
                            OrderResponse::Resting { oid } => Ok(Some(*oid)),
                            OrderResponse::Error(_) => Err(()),
                            _ => Ok(None),
                        });
                    }
                    r
                }
                crate::action::OrderTypeWire::Trigger(t) => match (o.r && linked, entry) {
                    (true, Some(Err(()))) => OrderResponse::Error("Entry order was rejected, so its TP/SL was not placed.".into()),
                    (true, Some(Ok(parent))) => self.place_trigger(user, o, t, parent, events),
                    _ => self.place_trigger(user, o, t, None, events),
                },
            };
            out.push(r);
        }
        out
    }

    fn open_order_margin(&self, user: &UserState) -> Usd {
        self.with_margin_ctx(|ctx| {
            user.open_orders
                .iter()
                .filter_map(|(a, oid)| self.books[*a as usize].orders.get(oid))
                .filter(|o| !o.reduce_only)
                .map(|o| Usd(ntl(o.px, o.sz as i64, self.metas[o.asset as usize].sz_decimals).0 / user.leverage_for(o.asset, ctx) as i64))
                .fold(Usd::ZERO, |a, b| a + b)
        })
    }

    /// Initial-margin check for a new resting/taking exposure of `lots` on `side`.
    fn has_margin_for(&self, user: Address, asset: AssetId, side: Side, px: Px, lots: u64) -> Result<(), (OrderStatus, String)> {
        let empty = UserState::default();
        let u = self.users.get(&user).unwrap_or(&empty);
        let meta = &self.metas[asset as usize];
        let (summary, lev) = self.with_margin_ctx(|ctx| (u.summary(ctx), u.leverage_for(asset, ctx)));
        let cur = u.positions.get(&asset).map(|p| p.szi).unwrap_or(0);
        let new_szi = cur + side.sign() * lots as i64;
        let new_value = Usd(ntl(px.max(self.assets[asset as usize].prices.mark_px), new_szi, meta.sz_decimals).0.abs());
        if lev > meta.max_leverage_at(new_value) {
            return Err((
                OrderStatus::PerpMarginRejected,
                "Order would exceed maximum position size for current leverage. Reduce leverage to increase maximum position size.".into(),
            ));
        }
        let order_im = Usd(ntl(px, lots as i64, meta.sz_decimals).0 / lev as i64);
        let required = summary.total_margin_used + self.open_order_margin(u) + order_im;
        if required > summary.account_value {
            return Err((OrderStatus::PerpMarginRejected, "Insufficient margin to place order.".into()));
        }
        Ok(())
    }

    pub(crate) fn place(&mut self, user: Address, mut req: OrderReq, origin: Origin, events: &mut Vec<Event>) -> OrderResponse {
        let asset = req.asset;
        let meta = self.metas[asset as usize].clone();
        let szd = meta.sz_decimals;
        let pos_szi = self.users.get(&user).and_then(|u| u.positions.get(&asset)).map(|p| p.szi).unwrap_or(0);

        if origin != Origin::Liquidation {
            let oracle = self.assets[asset as usize].prices.oracle_px.0 as i128;
            if (req.px.0 as i128 - oracle).abs() * 100 > oracle * self.params.oracle_band_pct as i128 {
                return OrderResponse::Error(format!(
                    "Order price cannot be more than {}% away from the reference price",
                    self.params.oracle_band_pct
                ));
            }
            if req.reduce_only {
                // Must reduce: position on the opposite side; clip to its size.
                if pos_szi == 0 || (pos_szi > 0) == (req.side == Side::Bid) {
                    return OrderResponse::Error("Reduce only order would increase position.".into());
                }
                req.lots = req.lots.min(pos_szi.unsigned_abs());
            } else if ntl(req.px, req.lots as i64, szd) < self.params.min_order_ntl {
                return OrderResponse::Error(format!("Order must have minimum value of ${}.", self.params.min_order_ntl));
            }
            if req.tif == Tif::Alo && self.books[asset as usize].crosses(req.side, req.px) {
                return OrderResponse::Error("Post only order would have immediately matched.".into());
            }
            let n_open = self.users.get(&user).map(|u| u.open_orders.len()).unwrap_or(0);
            if n_open >= self.params.max_open_orders && req.tif != Tif::Ioc {
                return OrderResponse::Error("Too many open orders".into());
            }
            if !req.reduce_only {
                if let Err((_, msg)) = self.has_margin_for(user, asset, req.side, req.px, req.lots) {
                    return OrderResponse::Error(msg);
                }
                if let Err(msg) = self.risk_check(user, asset, req.side, req.px, req.lots) {
                    return OrderResponse::Error(msg);
                }
            }
        }

        let oid = self.next_oid;
        self.next_oid += 1;
        let mut rem = req.lots;
        let mut filled: u64 = 0;
        let mut filled_ntl: i128 = 0;

        // Match against the opposite side, best price first, FIFO within a level.
        let maker_side = req.side.opposite();
        while rem > 0 && self.books[asset as usize].crosses(req.side, req.px) {
            let maker_oid = self.books[asset as usize].front(maker_side).expect("crossing level exists");
            let maker = self.books[asset as usize].orders[&maker_oid].clone();
            if maker.user == user {
                // Self-trade prevention: expire the resting (maker) order.
                self.remove_resting(asset, maker_oid, OrderStatus::SelfTradeCanceled, events);
                continue;
            }
            let mut q = rem.min(maker.sz);
            let maker_szi = self.users.get(&maker.user).and_then(|u| u.positions.get(&asset)).map(|p| p.szi).unwrap_or(0);
            if maker.reduce_only {
                let reducible = if (maker_szi > 0) != (maker.side == Side::Bid) { maker_szi.unsigned_abs() } else { 0 };
                if reducible == 0 {
                    self.remove_resting(asset, maker_oid, OrderStatus::ReduceOnlyCanceled, events);
                    continue;
                }
                q = q.min(reducible);
            } else {
                // Resting side is re-checked at match time.
                let increases = maker_szi == 0 || (maker_szi > 0) == (maker.side == Side::Bid);
                if increases && self.maker_lacks_margin(&maker, q) {
                    self.remove_resting(asset, maker_oid, OrderStatus::MarginCanceled, events);
                    continue;
                }
            }
            self.settle_fill(asset, &req, oid, user, &maker, q, origin, events);
            rem -= q;
            filled += q;
            filled_ntl += maker.px.0 as i128 * q as i128;
            self.books[asset as usize].reduce(maker_oid, q);
            if maker.sz == q {
                if let Some(u) = self.users.get_mut(&maker.user) {
                    u.open_orders.remove(&(asset, maker_oid));
                }
                events.push(Event::Order(OrderEvent::from_resting(&maker, 0, OrderStatus::Filled, self.time_ms)));
            }
        }

        let avg_px = (filled > 0).then(|| Px((filled_ntl / filled as i128) as u64));
        if rem > 0 && req.tif != Tif::Ioc {
            let resting = RestingOrder {
                oid,
                user,
                asset,
                side: req.side,
                px: req.px,
                sz: rem,
                orig_sz: req.lots,
                reduce_only: req.reduce_only,
                tif: req.tif,
                cloid: req.cloid,
                timestamp: self.time_ms,
            };
            events.push(Event::Order(OrderEvent::from_resting(&resting, rem, OrderStatus::Open, self.time_ms)));
            self.books[asset as usize].insert(resting);
            self.users.entry(user).or_default().open_orders.insert((asset, oid));
            return OrderResponse::Resting { oid };
        }
        if filled == 0 {
            return OrderResponse::Error(if origin == Origin::Liquidation {
                "No liquidity available for market order.".into()
            } else {
                "Order could not immediately match against any resting orders.".into()
            });
        }
        OrderResponse::Filled { oid, total_sz: filled, avg_px: avg_px.expect("filled > 0") }
    }

    fn maker_lacks_margin(&self, maker: &RestingOrder, q: u64) -> bool {
        let empty = UserState::default();
        let u = self.users.get(&maker.user).unwrap_or(&empty);
        let meta = &self.metas[maker.asset as usize];
        self.with_margin_ctx(|ctx| {
            let s = u.summary(ctx);
            let im = Usd(ntl(maker.px, q as i64, meta.sz_decimals).0 / u.leverage_for(maker.asset, ctx) as i64);
            s.total_margin_used + im > s.account_value
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn settle_fill(
        &mut self,
        asset: AssetId,
        taker: &OrderReq,
        taker_oid: Oid,
        taker_user: Address,
        maker: &RestingOrder,
        q: u64,
        origin: Origin,
        events: &mut Vec<Event>,
    ) {
        let szd = self.metas[asset as usize].sz_decimals;
        let px = maker.px;
        let abs_ntl = ntl(px, q as i64, szd);
        let taker_fee = clearinghouse::fee(abs_ntl, self.params.taker_fee);
        let maker_fee = clearinghouse::fee(abs_ntl, self.params.maker_fee);
        let d_taker = taker.side.sign() * q as i64;
        let tid = trade_id(taker_oid, maker.oid);
        let time = self.time_ms;

        let legs = [(taker_user, d_taker, taker_fee, taker_oid, true), (maker.user, -d_taker, maker_fee, maker.oid, false)];
        for (user, d, fee, oid, crossed) in legs {
            let eff = self.settle_leg(user, asset, d, px, fee);
            events.push(Event::Fill(FillEvent {
                user,
                asset,
                px,
                sz: q,
                side: Side::from_is_buy(d > 0),
                time,
                start_position: eff.start_szi,
                closed_pnl: eff.closed_pnl,
                fee,
                oid,
                tid,
                crossed,
                liquidation: crossed && origin == Origin::Liquidation,
            }));
        }
        self.users.entry(fee_collector()).or_default().balance += taker_fee + maker_fee;
        self.assets[asset as usize].prices.last_trade_px = Some(px);
    }

    /// Applies one side of a trade (`d` lots at `px`, paying `fee`) to a
    /// user's position. Cross: realized PnL and the fee go to the balance.
    /// Isolated: opening moves initial margin out of the cross balance into
    /// the position; closing adds realized PnL to the position's margin and
    /// returns the closed fraction of it to the cross balance.
    pub(crate) fn settle_leg(&mut self, user: Address, asset: AssetId, d: i64, px: Px, fee: Usd) -> clearinghouse::FillEffect {
        let szd = self.metas[asset as usize].sz_decimals;
        let default_lev = self.default_leverage()[asset as usize];
        let u = self.users.entry(user).or_default();
        let lev = u.leverage.get(&asset).copied().unwrap_or(default_lev) as i64;
        let iso_mode = u.isolated.contains(&asset);
        let pos = u.positions.entry(asset).or_default();
        if pos.szi == 0 {
            pos.isolated = iso_mode.then_some(Usd::ZERO);
        }
        let old_long = pos.szi.max(0);
        let eff = apply_fill(pos, d, px, szd);
        let closed = if eff.start_szi != 0 && (eff.start_szi > 0) != (d > 0) { d.abs().min(eff.start_szi.abs()) } else { 0 };
        let opened = d.abs() - closed;
        let mut to_balance = -fee;
        match pos.isolated.as_mut() {
            Some(m) => {
                *m += eff.closed_pnl;
                if closed > 0 {
                    let release = if closed == eff.start_szi.abs() { *m } else { Usd((m.0 as i128 * closed as i128 / eff.start_szi.abs() as i128) as i64) };
                    *m -= release;
                    to_balance += release;
                }
                if opened > 0 {
                    let im = Usd(ntl(px, opened, szd).0 / lev);
                    *m += im;
                    to_balance -= im;
                }
            }
            None => to_balance += eff.closed_pnl,
        }
        let new_long = pos.szi.max(0);
        if pos.szi == 0 {
            u.positions.remove(&asset);
        }
        u.balance += to_balance;
        self.assets[asset as usize].open_interest += new_long - old_long;
        eff
    }

    /// Removes a resting order, or an untriggered TP/SL with the same oid.
    pub(crate) fn remove_resting(&mut self, asset: AssetId, oid: Oid, status: OrderStatus, events: &mut Vec<Event>) -> Option<RestingOrder> {
        let Some(o) = self.books[asset as usize].remove(oid) else {
            self.remove_trigger(asset, oid, status, events);
            return None;
        };
        if let Some(u) = self.users.get_mut(&o.user) {
            u.open_orders.remove(&(asset, oid));
        }
        events.push(Event::Order(OrderEvent::from_resting(&o, o.sz, status, self.time_ms)));
        Some(o)
    }

    fn cancel(&mut self, user: Address, c: &CancelWire, events: &mut Vec<Event>) -> OrderResponse {
        let owned = self
            .books
            .get(c.a as usize)
            .is_some_and(|b| b.orders.get(&c.o).map(|o| o.user).or_else(|| b.triggers.get(&c.o).map(|t| t.user)) == Some(user));
        if !owned {
            return OrderResponse::Error("Order was never placed, already canceled, or filled.".into());
        }
        self.remove_resting(c.a, c.o, OrderStatus::Canceled, events);
        OrderResponse::Success
    }

    fn cancel_by_cloid(&mut self, user: Address, c: &CancelByCloidWire, events: &mut Vec<Event>) -> OrderResponse {
        let oid = self.books.get(c.asset as usize).and_then(|b| {
            b.find_cloid(user, c.cloid).or_else(|| b.triggers.values().find(|t| t.user == user && t.cloid == Some(c.cloid)).map(|t| t.oid))
        });
        match oid {
            Some(o) => self.cancel(user, &CancelWire { a: c.asset, o }, events),
            None => OrderResponse::Error("Order was never placed, already canceled, or filled.".into()),
        }
    }

    // ---- Account actions -------------------------------------------------

    /// Sets an asset's leverage and margin mode. The mode cannot change while
    /// a position is open; a lower leverage must still be covered by margin.
    fn update_leverage(&mut self, user: Address, asset: AssetId, is_cross: bool, leverage: u32) -> TxResponse {
        let Some(meta) = self.metas.get(asset as usize) else {
            return TxResponse::err("Unknown asset.");
        };
        if leverage == 0 || leverage > meta.max_leverage {
            return TxResponse::err("Invalid leverage value.");
        }
        if let Some(cap) = self.users.get(&user).and_then(|u| u.risk.as_ref()).and_then(|r| r.max_leverage) {
            if leverage > cap {
                return TxResponse::err(format!("Leverage is above your risk limit of {cap}x."));
            }
        }
        let mut u = self.users.get(&user).cloned().unwrap_or_default();
        if u.isolated.contains(&asset) == is_cross && u.positions.contains_key(&asset) {
            return TxResponse::err("Cannot switch leverage type with open position.");
        }
        if is_cross {
            u.isolated.remove(&asset);
        } else {
            u.isolated.insert(asset);
        }
        u.leverage.insert(asset, leverage);
        let ok = self.with_margin_ctx(|ctx| {
            let s = u.summary(ctx);
            let iso_ok = u.isolated_summary(asset, ctx).is_none_or(|(equity, _, im)| equity >= im);
            s.total_margin_used <= s.account_value && iso_ok
        });
        if !ok {
            return TxResponse::err("Insufficient margin to update leverage.");
        }
        self.users.insert(user, u);
        TxResponse::Default
    }

    /// Adds margin to an isolated position from the cross balance
    /// (`ntli` > 0, µUSDC), or takes it back (`ntli` < 0) while the position
    /// keeps its initial margin.
    fn update_isolated_margin(&mut self, user: Address, asset: AssetId, ntli: i64, events: &mut Vec<Event>) -> TxResponse {
        let Some(u) = self.users.get(&user) else { return TxResponse::err("No isolated position for this asset.") };
        if u.positions.get(&asset).is_none_or(|p| p.isolated.is_none()) {
            return TxResponse::err("No isolated position for this asset.");
        }
        if ntli == 0 {
            return TxResponse::err("Invalid amount.");
        }
        let (withdrawable, (equity, _, im)) =
            self.with_margin_ctx(|ctx| (u.withdrawable(ctx), u.isolated_summary(asset, ctx).expect("isolated position")));
        if ntli > 0 && Usd(ntli) > withdrawable {
            return TxResponse::err("Insufficient margin to add.");
        }
        if ntli < 0 && equity + Usd(ntli) < im {
            return TxResponse::err("Insufficient isolated margin to remove.");
        }
        let u = self.users.get_mut(&user).expect("user exists");
        u.balance -= Usd(ntli);
        *u.positions.get_mut(&asset).and_then(|p| p.isolated.as_mut()).expect("isolated position") += Usd(ntli);
        events.push(Event::Ledger { user, kind: LedgerKind::IsolatedMargin, usd: Usd(ntli) });
        TxResponse::Default
    }

    fn faucet(&mut self, user: Address, usd: &str, events: &mut Vec<Event>) -> TxResponse {
        let Some(v) = parse_decimal(usd, 6).filter(|v| *v > 0) else {
            return TxResponse::err("Invalid amount.");
        };
        if self.params.faucet_max.0 == 0 || v > self.params.faucet_max.0 as i128 {
            return TxResponse::err("Faucet disabled or amount too large.");
        }
        self.users.entry(user).or_default().balance += Usd(v as i64);
        self.total_minted += Usd(v as i64);
        self.risk_baseline(user, Usd(v as i64));
        events.push(Event::Ledger { user, kind: LedgerKind::Faucet, usd: Usd(v as i64) });
        TxResponse::Default
    }

    // ---- Oracle ------------------------------------------------------------

    fn oracle_vote(&mut self, signer: Address, pxs: &[String]) -> TxResponse {
        let Some(idx) = self.validators.iter().position(|v| v.address == signer) else {
            return TxResponse::err("Signer is not a validator.");
        };
        if pxs.len() != self.metas.len() {
            return TxResponse::err("Wrong number of oracle prices.");
        }
        let parsed: Option<Vec<Px>> =
            pxs.iter().map(|p| parse_decimal(p, 6).filter(|v| *v > 0).and_then(|v| u64::try_from(v).ok()).map(Px)).collect();
        let Some(parsed) = parsed else {
            return TxResponse::err("Invalid oracle price.");
        };
        self.oracle_votes.insert(idx as u32, OracleVote { time_ms: self.time_ms, pxs: parsed });
        TxResponse::Default
    }

    fn update_oracle(&mut self, events: &mut Vec<Event>) {
        let stakes: Vec<u64> = self.validators.iter().map(|v| v.stake).collect();
        let Some(pxs) = oracle::aggregate(&self.oracle_votes, &stakes, self.metas.len(), self.time_ms, self.params.oracle_max_age_ms)
        else {
            return;
        };
        for (a, px) in pxs.iter().enumerate() {
            let (bid, ask) = (self.books[a].best_bid(), self.books[a].best_ask());
            let p = &mut self.assets[a].prices;
            p.oracle_px = *px;
            p.update_mark(bid, ask, self.time_ms);
        }
        events.push(Event::Prices {
            oracle_pxs: pxs,
            mark_pxs: self.marks(),
        });
    }

    // ---- Funding -----------------------------------------------------------

    fn sample_premiums(&mut self) {
        let k = self.time_ms / self.params.premium_sample_ms;
        if k <= self.last_premium_sample {
            return;
        }
        self.last_premium_sample = k;
        for a in 0..self.metas.len() {
            let meta = &self.metas[a];
            let prem = funding::premium(&self.books[a], self.assets[a].prices.oracle_px, meta.impact_ntl.0 as i128, meta.sz_decimals);
            self.assets[a].funding.sample(prem);
        }
    }

    fn settle_funding(&mut self, events: &mut Vec<Event>) {
        let period = self.time_ms / self.params.funding_interval_ms;
        if period <= self.last_funding_period {
            return;
        }
        self.last_funding_period = period;
        for a in 0..self.metas.len() {
            let (avg, n) = self.assets[a].funding.take_average();
            let p = &self.params;
            let rate = funding::hourly_rate(avg, p.funding_interest_8h, p.funding_clamp, p.funding_cap_hourly);
            let oracle = self.assets[a].prices.oracle_px;
            let szd = self.metas[a].sz_decimals;
            let mut net_paid = Usd::ZERO;
            for (addr, u) in self.users.iter_mut() {
                let Some(pos) = u.positions.get_mut(&(a as AssetId)) else { continue };
                // Longs pay when the rate is positive.
                let pay = fixed::mul_rate(ntl(oracle, pos.szi, szd), rate);
                pos.funding_since_open += pay;
                pos.funding_all_time += pay;
                match pos.isolated.as_mut() {
                    Some(m) => *m -= pay,
                    None => u.balance -= pay,
                }
                net_paid += pay;
                events.push(Event::Funding { user: *addr, asset: a as AssetId, usdc: -pay, szi: pos.szi, rate, n_samples: n });
            }
            // Payments round per position, so payers and receivers can differ
            // by a few µUSDC; the fee collector absorbs the residual so no
            // USDC is created or destroyed.
            if net_paid != Usd::ZERO {
                self.users.entry(fee_collector()).or_default().balance += net_paid;
            }
        }
    }

    // ---- Liquidations ------------------------------------------------------

    /// Liquidates every isolated position below its maintenance margin, then
    /// every cross account below its maintenance margin.
    fn run_liquidations(&mut self, events: &mut Vec<Event>) {
        let candidates: Vec<Address> = self
            .users
            .iter()
            .filter(|(a, u)| !u.positions.is_empty() && **a != liquidator())
            .map(|(a, _)| *a)
            .collect();
        for user in candidates {
            let isolated: Vec<AssetId> = self.users[&user].positions.iter().filter(|(_, p)| p.isolated.is_some()).map(|(a, _)| *a).collect();
            for asset in isolated {
                let Some((equity, mm, _)) = self.with_margin_ctx(|ctx| self.users[&user].isolated_summary(asset, ctx)) else { continue };
                if equity >= mm {
                    continue;
                }
                let open: Vec<(AssetId, Oid)> = self.users[&user].open_orders.iter().filter(|(a, _)| *a == asset).copied().collect();
                for (a, oid) in open {
                    self.remove_resting(a, oid, OrderStatus::LiquidatedCanceled, events);
                }
                self.liquidate(user, &[asset], equity, mm, events);
            }
            let cross: Vec<AssetId> = self.users[&user].positions.iter().filter(|(_, p)| p.isolated.is_none()).map(|(a, _)| *a).collect();
            if cross.is_empty() {
                continue;
            }
            let s = self.with_margin_ctx(|ctx| self.users[&user].summary(ctx));
            if s.account_value >= s.maintenance_margin {
                continue;
            }
            // Free margin held by resting orders first.
            let open: Vec<(AssetId, Oid)> = self.users[&user].open_orders.iter().copied().collect();
            for (a, oid) in open {
                self.remove_resting(a, oid, OrderStatus::LiquidatedCanceled, events);
            }
            self.liquidate(user, &cross, s.account_value, s.maintenance_margin, events);
        }
    }

    /// Liquidates `assets` (the cross account's positions, or one isolated
    /// position) with equity `equity` against maintenance margin `mm`:
    /// below zero by auto-deleveraging, below 2/3 of `mm` through the backstop
    /// liquidator, otherwise with market orders.
    fn liquidate(&mut self, user: Address, assets: &[AssetId], equity: Usd, mm: Usd, events: &mut Vec<Event>) {
        if equity < Usd::ZERO {
            self.auto_deleverage(user, assets, equity, events);
        } else if equity.0 as i128 * 3 < mm.0 as i128 * 2 {
            self.backstop_liquidate(user, assets, equity, events);
        } else {
            self.market_liquidate(user, assets, equity, events);
        }
    }

    /// Below 2/3 of maintenance margin: the positions and their remaining
    /// equity move to the liquidator vault at the mark price.
    fn backstop_liquidate(&mut self, user: Address, assets: &[AssetId], equity: Usd, events: &mut Vec<Event>) {
        let marks = self.marks();
        let u = self.users.get_mut(&user).expect("user exists");
        let positions: Vec<(AssetId, clearinghouse::Position)> = assets.iter().filter_map(|a| u.positions.remove(a).map(|p| (*a, p))).collect();
        if positions.iter().all(|(_, p)| p.isolated.is_none()) {
            // Cross: the whole balance is part of the equity that moves.
            u.balance = Usd::ZERO;
        }
        let liq = self.users.entry(liquidator()).or_default();
        let mut liquidated = Vec::new();
        for (asset, p) in positions {
            let szd = self.metas[asset as usize].sz_decimals;
            let lp = liq.positions.entry(asset).or_default();
            let old_long = lp.szi.max(0);
            let eff = apply_fill(lp, p.szi, marks[asset as usize], szd);
            let new_long = lp.szi.max(0);
            liq.balance += eff.closed_pnl;
            if lp.szi == 0 {
                liq.positions.remove(&asset);
            }
            // The user's longs leave; the liquidator's may net against its own shorts.
            self.assets[asset as usize].open_interest += new_long - old_long - p.szi.max(0);
            liquidated.push((asset, p.szi));
        }
        liq.balance += equity;
        events.push(Event::Liquidation { user, method: LiquidationMethod::Backstop, account_value: equity, positions: liquidated });
    }

    /// Auto-deleveraging of an account below zero. Its deficit is shared
    /// across its positions by notional, which sets each position's
    /// bankruptcy price: the price at which closing it leaves its share of
    /// equity at zero. Each position is then closed at that price against the
    /// opposite positions ranked by profit and leverage
    /// (`upnl / cost basis × notional / account value`, highest first), the
    /// order Hyperliquid documents. The winners give up the deficit, so no
    /// bad debt is left and no vault pays for it.
    fn auto_deleverage(&mut self, user: Address, assets: &[AssetId], equity: Usd, events: &mut Vec<Event>) {
        let marks = self.marks();
        let positions: Vec<(AssetId, i64, i128)> = assets
            .iter()
            .filter_map(|a| self.users[&user].positions.get(a).map(|p| (*a, p.szi, ntl(marks[*a as usize], p.szi, self.metas[*a as usize].sz_decimals).0.abs() as i128)))
            .collect();
        let deficit = -(equity.0 as i128);
        let total: i128 = positions.iter().map(|p| p.2).sum::<i128>().max(1);
        let isolated = positions.iter().any(|(a, _, _)| self.users[&user].positions[a].isolated.is_some());
        let mut left = deficit;
        let mut liquidated = Vec::new();
        for (k, &(asset, szi, value)) in positions.iter().enumerate() {
            let share = if k + 1 == positions.len() { left } else { deficit * value / total };
            left -= share;
            let szd = self.metas[asset as usize].sz_decimals;
            // A long goes bankrupt above the mark, a short below it.
            let bankrupt_px = Px((marks[asset as usize].0 as i128 + share * fixed::pow10(szd) / szi as i128).max(1) as u64);
            let mut rem = szi.unsigned_abs();
            for (cp, cp_szi) in self.adl_ranking(user, asset, szi > 0) {
                if rem == 0 {
                    break;
                }
                let q = rem.min(cp_szi.unsigned_abs());
                rem -= q;
                let d = -szi.signum() * q as i64;
                let tid = trade_id(self.height, (asset as u64) << 32 | liquidated.len() as u64 | (rem << 8));
                for (who, dd, crossed) in [(user, d, true), (cp, -d, false)] {
                    let eff = self.settle_leg(who, asset, dd, bankrupt_px, Usd::ZERO);
                    events.push(Event::Fill(FillEvent {
                        user: who,
                        asset,
                        px: bankrupt_px,
                        sz: q,
                        side: Side::from_is_buy(dd > 0),
                        time: self.time_ms,
                        start_position: eff.start_szi,
                        closed_pnl: eff.closed_pnl,
                        fee: Usd::ZERO,
                        oid: 0,
                        tid,
                        crossed,
                        liquidation: crossed,
                    }));
                }
            }
            liquidated.push((asset, szi));
        }
        if !isolated {
            // Rounding leaves a few µUSDC either way; the fee collector takes it so the account ends at zero.
            let u = self.users.get_mut(&user).expect("user exists");
            let rest = std::mem::take(&mut u.balance);
            self.users.entry(fee_collector()).or_default().balance += rest;
        }
        events.push(Event::Liquidation { user, method: LiquidationMethod::Adl, account_value: equity, positions: liquidated });
    }

    /// Opposite positions to `asset` (shorts if `long`), most profitable and
    /// most leveraged first; ties by address so every validator agrees.
    fn adl_ranking(&self, user: Address, asset: AssetId, long: bool) -> Vec<(Address, i64)> {
        let mark = self.assets[asset as usize].prices.mark_px;
        let szd = self.metas[asset as usize].sz_decimals;
        let mut ranked: Vec<(i128, Address, i64)> = self.with_margin_ctx(|ctx| {
            self.users
                .iter()
                .filter(|(a, _)| **a != user)
                .filter_map(|(a, u)| {
                    let p = u.positions.get(&asset).filter(|p| p.szi != 0 && (p.szi > 0) != long)?;
                    let value = ntl(mark, p.szi, szd).0.abs() as i128;
                    let upnl = p.upnl(mark, szd).0 as i128;
                    let equity = match p.isolated_equity(mark, szd) {
                        Some(e) => e.0 as i128,
                        None => u.summary(ctx).account_value.0 as i128,
                    };
                    let cost = (p.entry_ntl as i128).abs().max(1);
                    let score = if equity <= 0 { i128::MAX } else { upnl * value / cost * 1_000_000 / equity };
                    Some((score, *a, p.szi))
                })
                .collect()
        });
        ranked.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)));
        ranked.into_iter().map(|(_, a, s)| (a, s)).collect()
    }

    /// Sends IOC market orders for each position; positions above the
    /// threshold are cut by `partial_liq_pct` unless the user is in cooldown.
    fn market_liquidate(&mut self, user: Address, assets: &[AssetId], equity: Usd, events: &mut Vec<Event>) {
        let now = self.time_ms;
        let positions: Vec<(AssetId, i64)> = assets.iter().filter_map(|a| self.users[&user].positions.get(a).map(|p| (*a, p.szi))).collect();
        let in_cooldown = now < self.users[&user].liq_cooldown_until;
        let mut partial = false;
        let mut liquidated = Vec::new();
        for (asset, szi) in positions {
            let meta = self.metas[asset as usize].clone();
            let mark = self.assets[asset as usize].prices.mark_px;
            let value = Usd(ntl(mark, szi, meta.sz_decimals).0.abs());
            let mut lots = szi.unsigned_abs();
            if value > self.params.partial_liq_threshold && !in_cooldown {
                lots = (lots * self.params.partial_liq_pct as u64).div_ceil(100);
                partial = true;
            }
            // Closing order with slippage around mark (sell below, buy above).
            let side = if szi > 0 { Side::Ask } else { Side::Bid };
            let slip = fixed::px_mul_rate(mark, self.params.liq_slippage);
            let px_raw = if side == Side::Ask { mark.0 as i128 - slip } else { mark.0 as i128 + slip };
            let px = fixed::round_px(px_raw.max(1), meta.sz_decimals, side == Side::Bid);
            let req = OrderReq { asset, side, px, lots, reduce_only: true, tif: Tif::Ioc, cloid: None };
            self.place(user, req, Origin::Liquidation, events);
            liquidated.push((asset, szi));
        }
        if partial {
            self.users.get_mut(&user).expect("user exists").liq_cooldown_until = now + self.params.partial_liq_cooldown_ms;
        }
        events.push(Event::Liquidation { user, method: LiquidationMethod::Market, account_value: equity, positions: liquidated });
    }
}

/// 50-bit trade id derived from (taker oid, maker oid).
pub fn trade_id(a: Oid, b: Oid) -> u64 {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&a.to_be_bytes());
    buf[8..].copy_from_slice(&b.to_be_bytes());
    let h = blake3::hash(&buf);
    u64::from_be_bytes(h.as_bytes()[..8].try_into().expect("8 bytes")) >> 14
}
