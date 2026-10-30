//! Block outputs: per-transaction responses and events (fills, order updates,
//! funding, liquidations, prices). They are deterministic but are not part of
//! the state hash; the WebSocket API is built from them.

use crate::book::RestingOrder;
use crate::fixed::{Px, Rate, Usd};
use crate::types::{Address, AssetId, Cloid, Hash32, Oid, OrderStatus, Side};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderResponse {
    Resting { oid: Oid },
    Filled { oid: Oid, total_sz: u64, avg_px: Px },
    Success,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum TxResponse {
    Order { statuses: Vec<OrderResponse> },
    Cancel { statuses: Vec<OrderResponse> },
    /// A TWAP order started.
    Twap { twap_id: u64 },
    /// A vault was created at this address.
    Vault { vault_address: Address },
    Default,
    Error { error: String },
}

impl TxResponse {
    pub fn err(msg: impl Into<String>) -> TxResponse {
        TxResponse::Error { error: msg.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillEvent {
    pub user: Address,
    pub asset: AssetId,
    pub px: Px,
    /// Lots.
    pub sz: u64,
    pub side: Side,
    pub time: u64,
    pub start_position: i64,
    pub closed_pnl: Usd,
    pub fee: Usd,
    pub oid: Oid,
    pub tid: u64,
    /// True for the taker.
    pub crossed: bool,
    /// True when this is the liquidated user's taker fill.
    pub liquidation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderEvent {
    pub user: Address,
    pub asset: AssetId,
    pub oid: Oid,
    pub side: Side,
    pub limit_px: Px,
    /// Remaining lots.
    pub sz: u64,
    pub orig_sz: u64,
    pub cloid: Option<Cloid>,
    pub timestamp: u64,
    pub status: OrderStatus,
    pub status_timestamp: u64,
}

impl OrderEvent {
    pub fn from_resting(o: &RestingOrder, remaining: u64, status: OrderStatus, now: u64) -> OrderEvent {
        OrderEvent {
            user: o.user,
            asset: o.asset,
            oid: o.oid,
            side: o.side,
            limit_px: o.px,
            sz: remaining,
            orig_sz: o.orig_sz,
            cloid: o.cloid,
            timestamp: o.timestamp,
            status,
            status_timestamp: now,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LiquidationMethod {
    Market,
    Backstop,
    /// The account was below zero: its positions were closed against the
    /// most profitable, most leveraged opposite positions at the bankruptcy
    /// price, so no one else covers the loss.
    Adl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LedgerKind {
    Faucet,
    /// Core balance -> EVM native balance.
    CoreToEvm,
    /// EVM value sent to the bridge address -> core balance.
    EvmToCore,
    /// Cross balance -> an isolated position's margin (negative: back).
    IsolatedMargin,
    /// A usdSend to (negative) or from (positive) `counterparty`.
    Send { counterparty: Address },
    /// Balance into a vault (negative), out of it (positive), and the
    /// leader's profit share.
    VaultDeposit { vault: Address },
    VaultWithdraw { vault: Address },
    VaultProfitShare { vault: Address },
}

/// Summary of an executed EVM transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvmTxSummary {
    pub tx_hash: Hash32,
    pub from: Address,
    pub to: Option<Address>,
    pub contract_address: Option<Address>,
    pub success: bool,
    pub gas_used: u64,
    pub nonce: u64,
    pub logs: Vec<EvmLog>,
    /// Value in wei, decimal string (native token = USDC with 18 decimals).
    pub value_wei: String,
    /// Return data (or revert data), hex.
    pub output: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvmLog {
    pub address: Address,
    pub topics: Vec<Hash32>,
    /// Hex.
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Movement of funds not caused by trading (faucet, bridge).
    Ledger { user: Address, kind: LedgerKind, usd: Usd },
    /// An EVM block ran: a small (1 s, 2M gas) or big (1 min, 30M gas) one.
    EvmBlock { number: u64, hash: Hash32, n_txs: usize, gas_used: u64, big: bool },
    /// A finalized EVM transaction is waiting for the next small or big block.
    EvmTxQueued { tx_hash: Hash32, big: bool },
    EvmTx(EvmTxSummary),
    EvmTxSkipped { tx_hash: Hash32, reason: String },
    /// A contract-originated action ran on core; `asset` is set when the call
    /// decoded. `tx_hash` is the EVM transaction that sent it.
    CoreWriter { user: Address, asset: Option<AssetId>, response: OrderResponse, tx_hash: Hash32 },
    /// A CoreWriter order waits until `execute_at_ms` before running on core.
    CoreWriterQueued { user: Address, tx_hash: Hash32, execute_at_ms: u64 },
    Fill(FillEvent),
    Order(OrderEvent),
    Funding { user: Address, asset: AssetId, usdc: Usd, szi: i64, rate: Rate, n_samples: u64 },
    Liquidation { user: Address, method: LiquidationMethod, account_value: Usd, positions: Vec<(AssetId, i64)> },
    Prices { oracle_pxs: Vec<Px>, mark_pxs: Vec<Px> },
    /// An account's own risk limit acted.
    Risk { user: Address, kind: RiskKind, account_value: Usd },
    /// A TWAP order started, sent a slice, finished or was canceled.
    Twap(TwapEvent),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TwapEvent {
    pub id: u64,
    pub user: Address,
    pub asset: AssetId,
    pub side: Side,
    /// Total and filled size, in lots.
    pub sz: u64,
    pub filled: u64,
    /// Filled notional (µUSDC), for the average price.
    pub filled_ntl: i64,
    pub minutes: u32,
    pub status: TwapStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TwapStatus {
    Activated,
    Running,
    Finished,
    Canceled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RiskKind {
    /// Daily loss limit reached: only reducing orders until 00:00 UTC.
    DailyLossHalt,
    /// Maintenance margin reached the set share of account value: positions closed at market.
    MarginAutoClose,
}
