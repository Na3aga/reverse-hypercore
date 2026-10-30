//! Public-API JSON views over engine state and events. The field names follow
//! the public info and WebSocket API (`clearinghouseState`, `l2Book`, `WsFill`, ...).

use hc_core::events::{Event, FillEvent, OrderEvent, OrderResponse, TxResponse};
use hc_core::fixed::{format_decimal, ntl, Px, Usd};
use hc_core::types::{Address, Side};
use hc_core::State;
use serde_json::{json, Value};

pub fn px(p: Px) -> String {
    p.to_string()
}

pub fn sz(lots: i64, sz_decimals: u32) -> String {
    format_decimal(lots as i128, sz_decimals)
}

fn usd(u: Usd) -> String {
    u.to_string()
}

pub fn coin_index(state: &State, coin: &str) -> Option<usize> {
    state.metas.iter().position(|m| m.name == coin)
}

pub fn meta(state: &State) -> Value {
    json!({
        "universe": state.metas.iter().map(|m| json!({
            "name": m.name,
            "szDecimals": m.sz_decimals,
            "maxLeverage": m.max_leverage,
            "marginTiers": m.margin_tiers.iter().map(|t| json!({"lowerBound": usd(t.lower_bound), "maxLeverage": t.max_leverage})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "collateralToken": "USDC",
    })
}

pub fn mid(state: &State, a: usize) -> Option<Px> {
    let b = &state.books[a];
    match (b.best_bid(), b.best_ask()) {
        (Some(bid), Some(ask)) => Some(Px((bid.0 + ask.0) / 2)),
        // Public behavior: fall back to the last trade when the book is one-sided/empty.
        _ => state.assets[a].prices.last_trade_px,
    }
}

pub fn all_mids(state: &State) -> Value {
    let mut m = serde_json::Map::new();
    for (a, meta) in state.metas.iter().enumerate() {
        if let Some(p) = mid(state, a) {
            m.insert(meta.name.clone(), json!(px(p)));
        }
    }
    Value::Object(m)
}

pub fn meta_and_asset_ctxs(state: &State) -> Value {
    let ctxs: Vec<Value> = (0..state.metas.len())
        .map(|a| {
            let s = &state.assets[a];
            let f = &s.funding;
            let premium = if f.n_samples == 0 { 0 } else { f.premium_sum / f.n_samples as i128 };
            json!({
                "markPx": px(s.prices.mark_px),
                "oraclePx": px(s.prices.oracle_px),
                "midPx": mid(state, a).map(px),
                "openInterest": sz(s.open_interest, state.metas[a].sz_decimals),
                "premium": format_decimal(premium, 12),
            })
        })
        .collect();
    json!([meta(state), ctxs])
}

pub fn l2_book(state: &State, a: usize, depth: usize) -> Value {
    let szd = state.metas[a].sz_decimals;
    let side = |s: Side| -> Vec<Value> {
        state.books[a].levels(s, depth).into_iter().map(|(p, q, n)| json!({"px": px(p), "sz": sz(q as i64, szd), "n": n})).collect()
    };
    json!({"coin": state.metas[a].name, "time": state.time_ms, "levels": [side(Side::Bid), side(Side::Ask)]})
}

pub fn clearinghouse_state(state: &State, user: Address) -> Value {
    let u = state.users.get(&user).cloned().unwrap_or_default();
    state.with_margin_ctx(|ctx| {
        let s = u.summary(ctx);
        let mut iso_value = Usd::ZERO;
        let mut iso_ntl = Usd::ZERO;
        let positions: Vec<Value> = u
            .positions
            .iter()
            .map(|(a, p)| {
                let meta = &state.metas[*a as usize];
                let mark = ctx.marks[*a as usize];
                let value = Usd(ntl(mark, p.szi, meta.sz_decimals).0.abs());
                let lev = u.leverage_for(*a, ctx);
                let upnl = p.upnl(mark, meta.sz_decimals);
                let (leverage, margin_used, equity_without) = match p.isolated {
                    Some(m) => {
                        let equity = m + upnl;
                        iso_value += equity;
                        iso_ntl += value;
                        (json!({"type": "isolated", "value": lev, "rawUsd": usd(m - Usd(p.entry_ntl))}), equity, (m.0, 0))
                    }
                    // Cross: the rest of the account keeps its value and its maintenance margin.
                    None => (json!({"type": "cross", "value": lev}), Usd(value.0 / lev as i64), ((s.account_value - upnl).0, (s.maintenance_margin - meta.maintenance_margin(value)).0)),
                };
                json!({
                    "type": "oneWay",
                    "position": {
                        "coin": meta.name,
                        "szi": sz(p.szi, meta.sz_decimals),
                        "entryPx": p.entry_px(meta.sz_decimals).map(px),
                        "positionValue": usd(value),
                        "unrealizedPnl": usd(upnl),
                        "returnOnEquity": format!("{:.4}", upnl.0 as f64 / (p.entry_ntl.abs() as f64 / lev as f64).max(1.0)),
                        "leverage": leverage,
                        "liquidationPx": liquidation_px(p, meta, equity_without.0, equity_without.1),
                        "marginUsed": usd(margin_used),
                        "maxLeverage": meta.max_leverage,
                        "cumFunding": {"allTime": usd(p.funding_all_time), "sinceOpen": usd(p.funding_since_open)},
                    }
                })
            })
            .collect();
        let cross = json!({
            "accountValue": usd(s.account_value),
            "totalNtlPos": usd(s.total_ntl_pos),
            "totalRawUsd": usd(s.total_raw_usd),
            "totalMarginUsed": usd(s.total_margin_used),
        });
        let total = json!({
            "accountValue": usd(s.account_value + iso_value),
            "totalNtlPos": usd(s.total_ntl_pos + iso_ntl),
            "totalRawUsd": usd(s.total_raw_usd + u.isolated_margin()),
            "totalMarginUsed": usd(s.total_margin_used + iso_value),
        });
        // Every market's leverage and margin mode, set or default.
        let leverage: Vec<Value> = state
            .metas
            .iter()
            .enumerate()
            .map(|(a, m)| json!({"coin": m.name, "type": if u.isolated.contains(&(a as u32)) { "isolated" } else { "cross" }, "value": u.leverage_for(a as u32, ctx)}))
            .collect();
        json!({
            "assetPositions": positions,
            "assetLeverage": leverage,
            "crossMarginSummary": cross,
            "marginSummary": total,
            "crossMaintenanceMarginUsed": usd(s.maintenance_margin),
            "withdrawable": usd(u.withdrawable(ctx)),
            "time": state.time_ms,
        })
    })
}

/// Mark price at which a position is liquidated, holding everything else
/// fixed: `equity_without` is the equity apart from this position's PnL (its
/// isolated margin, or the rest of the cross account) and `other_mm` the
/// maintenance margin of the cross account's other positions. Uses the first
/// margin tier; for display only.
fn liquidation_px(p: &hc_core::clearinghouse::Position, meta: &hc_core::clearinghouse::AssetMeta, equity_without: i64, other_mm: i64) -> Option<String> {
    let coins = p.szi as f64 / 10f64.powi(meta.sz_decimals as i32);
    let mmr = 1.0 / (2.0 * meta.margin_tiers[0].max_leverage as f64);
    // equity_without + coins * px - entry = other_mm + |coins| * px * mmr
    let px = (p.entry_ntl as f64 + other_mm as f64 - equity_without as f64) / (coins - coins.abs() * mmr);
    (px.is_finite() && px > 0.0).then(|| format!("{:.1}", px / 1e6))
}

/// Open orders, including untriggered TP/SL orders (`isTrigger`).
pub fn open_orders(state: &State, user: Address) -> Value {
    let Some(u) = state.users.get(&user) else { return json!([]) };
    Value::Array(
        u.open_orders
            .iter()
            .filter_map(|(a, oid)| {
                let book = &state.books[*a as usize];
                let szd = state.metas[*a as usize].sz_decimals;
                let coin = &state.metas[*a as usize].name;
                if let Some(o) = book.orders.get(oid) {
                    return Some(json!({
                        "coin": coin, "side": o.side, "limitPx": px(o.px),
                        "sz": sz(o.sz as i64, szd), "origSz": sz(o.orig_sz as i64, szd),
                        "oid": o.oid, "timestamp": o.timestamp, "reduceOnly": o.reduce_only,
                        "tif": o.tif, "cloid": o.cloid, "isTrigger": false, "orderType": "Limit",
                    }));
                }
                let t = book.triggers.get(oid)?;
                let kind = match (t.tpsl, t.is_market) {
                    (hc_core::types::Tpsl::Tp, true) => "Take Profit Market",
                    (hc_core::types::Tpsl::Tp, false) => "Take Profit Limit",
                    (hc_core::types::Tpsl::Sl, true) => "Stop Market",
                    (hc_core::types::Tpsl::Sl, false) => "Stop Limit",
                };
                // Same rule as the engine: sell TP / buy SL fire at or above the trigger.
                let above = matches!((t.side, t.tpsl), (Side::Ask, hc_core::types::Tpsl::Tp) | (Side::Bid, hc_core::types::Tpsl::Sl));
                let waiting = t.parent.is_some_and(|p| book.orders.contains_key(&p));
                Some(json!({
                    "coin": coin, "side": t.side, "limitPx": px(t.limit_px),
                    "sz": sz(t.sz as i64, szd), "origSz": sz(t.sz as i64, szd),
                    "oid": t.oid, "timestamp": t.timestamp, "reduceOnly": t.reduce_only,
                    "tif": null, "cloid": t.cloid, "isTrigger": true, "orderType": kind,
                    "tpsl": t.tpsl, "isMarket": t.is_market, "triggerPx": px(t.trigger_px),
                    "triggerCondition": format!("Mark price {} {}", if above { ">=" } else { "<=" }, px(t.trigger_px)),
                    "waitingForEntry": waiting,
                }))
            })
            .collect(),
    )
}

/// The account's risk limits and today's status, or null.
pub fn risk(state: &State, user: Address) -> Value {
    let Some(r) = state.users.get(&user).and_then(|u| u.risk.as_ref()) else { return Value::Null };
    json!({
        "maxPositionUsd": r.max_position_usd.map(|v| v.to_string()),
        "maxLeverage": r.max_leverage,
        "dailyLossUsd": r.daily_loss_usd.map(|v| v.to_string()),
        "closeAtMarginPct": r.close_at_margin_pct,
        "dayStartValue": r.day_start_value.to_string(),
        "dayPnl": state.risk_day_pnl(user).map(|v| v.to_string()),
        "halted": r.halted,
    })
}

pub fn order_response(state: &State, r: &OrderResponse, asset_szd: Option<u32>) -> Value {
    match r {
        OrderResponse::Resting { oid } => json!({"resting": {"oid": oid}}),
        OrderResponse::Filled { oid, total_sz, avg_px } => {
            json!({"filled": {"oid": oid, "totalSz": sz(*total_sz as i64, asset_szd.unwrap_or(0)), "avgPx": px(*avg_px)}})
        }
        OrderResponse::Success => json!("success"),
        OrderResponse::Error(e) => {
            let _ = state;
            json!({"error": e})
        }
    }
}

/// `{"status":"ok","response":{...}}` in the public exchange-endpoint shape.
pub fn tx_response(state: &State, r: &TxResponse, szds: &[Option<u32>]) -> Value {
    match r {
        TxResponse::Order { statuses } => json!({"status": "ok", "response": {"type": "order", "data": {
            "statuses": statuses.iter().zip(szds.iter().chain(std::iter::repeat(&None))).map(|(s, d)| order_response(state, s, *d)).collect::<Vec<_>>()
        }}}),
        TxResponse::Cancel { statuses } => json!({"status": "ok", "response": {"type": "cancel", "data": {
            "statuses": statuses.iter().map(|s| order_response(state, s, None)).collect::<Vec<_>>()
        }}}),
        TxResponse::Vault { vault_address } => json!({"status": "ok", "response": {"type": "createVault", "data": vault_address}}),
        TxResponse::Twap { twap_id } => json!({"status": "ok", "response": {"type": "twapOrder", "data": {"status": {"running": {"twapId": twap_id}}}}}),
        TxResponse::Default => json!({"status": "ok", "response": {"type": "default"}}),
        TxResponse::Error { error } => json!({"status": "err", "response": error}),
    }
}

pub fn fill(state: &State, f: &FillEvent) -> Value {
    let meta = &state.metas[f.asset as usize];
    let start = f.start_position;
    let dir = match (start.signum(), f.side) {
        (0, Side::Bid) | (1, Side::Bid) => "Open Long",
        (0, Side::Ask) | (-1, Side::Ask) => "Open Short",
        (1, Side::Ask) => "Close Long",
        (_, _) => "Close Short",
    };
    json!({
        "coin": meta.name,
        "px": px(f.px),
        "sz": sz(f.sz as i64, meta.sz_decimals),
        "side": f.side,
        "time": f.time,
        "startPosition": sz(start, meta.sz_decimals),
        "dir": dir,
        "closedPnl": usd(f.closed_pnl),
        "oid": f.oid,
        "tid": f.tid,
        "crossed": f.crossed,
        "fee": usd(f.fee),
        "feeToken": "USDC",
        "liquidation": f.liquidation,
        // Auto-deleveraging fills have no order behind them.
        "adl": f.oid == 0,
        "user": f.user,
    })
}

pub fn order_update(state: &State, o: &OrderEvent) -> Value {
    let meta = &state.metas[o.asset as usize];
    json!({
        "order": {
            "coin": meta.name,
            "side": o.side,
            "limitPx": px(o.limit_px),
            "sz": sz(o.sz as i64, meta.sz_decimals),
            "oid": o.oid,
            "timestamp": o.timestamp,
            "origSz": sz(o.orig_sz as i64, meta.sz_decimals),
            "cloid": o.cloid,
        },
        "status": o.status,
        "statusTimestamp": o.status_timestamp,
    })
}

/// Public trades (one per maker fill, side = taker side).
pub fn trades(state: &State, events: &[Event], asset: usize) -> Vec<Value> {
    let fills: Vec<&FillEvent> = events.iter().filter_map(|e| if let Event::Fill(f) = e { Some(f) } else { None }).collect();
    fills
        .chunks(2)
        .filter_map(|pair| {
            let (taker, maker) = (pair.first()?, pair.get(1)?);
            (taker.asset as usize == asset).then(|| {
                let meta = &state.metas[asset];
                let (buyer, seller) = if taker.side == Side::Bid { (taker.user, maker.user) } else { (maker.user, taker.user) };
                json!({
                    "coin": meta.name,
                    "side": taker.side,
                    "px": px(taker.px),
                    "sz": sz(taker.sz as i64, meta.sz_decimals),
                    "time": taker.time,
                    "tid": taker.tid,
                    "users": [buyer, seller],
                })
            })
        })
        .collect()
}

/// Supply accounting. The exact ledger identity is
/// `Σ balances + Σ isolated margin − Σ position cost basis + on EVM = minted`: every fill moves
/// cash and cost basis between two accounts by the same rounded notional,
/// fees go to the fee collector, and funding residuals too. Unrealized PnL
/// (which marks each position separately, rounding each toward zero) is
/// reported alongside but is not used for the check.
pub fn supply(state: &State) -> Value {
    state.with_margin_ctx(|ctx| {
        let mut balances = Usd::ZERO;
        let mut upnl = Usd::ZERO;
        let mut cost_basis = Usd::ZERO;
        let mut isolated = Usd::ZERO;
        for u in state.users.values() {
            balances += u.balance;
            isolated += u.isolated_margin();
            upnl += Usd(u.total_account_value(ctx).0 - u.balance.0 - u.isolated_margin().0);
            cost_basis += Usd(u.positions.values().map(|p| p.entry_ntl).sum());
        }
        let accounted = balances + isolated - cost_basis + state.net_bridged_to_evm;
        json!({
            "totalMinted": usd(state.total_minted),
            "coreBalances": usd(balances),
            "isolatedMargin": usd(isolated),
            "unrealizedPnlSum": usd(upnl),
            "netCostBasis": usd(cost_basis),
            "onEvm": usd(state.net_bridged_to_evm),
            "feesCollected": usd(state.users.get(&hc_core::exchange::fee_collector()).map(|u| u.balance).unwrap_or_default()),
            "accounted": usd(accounted),
            "discrepancy": usd(accounted - state.total_minted),
        })
    })
}

/// A vault: its account value, shares and followers' equity. With `user`,
/// also that user's share.
pub fn vault(state: &State, addr: Address, user: Option<Address>) -> Option<Value> {
    let v = state.vaults.get(&addr)?;
    let equity = state.vault_equity(addr);
    let per_share = |s: u128| Usd(if v.total_shares == 0 { 0 } else { (s as i128 * equity.0 as i128 / v.total_shares as i128) as i64 });
    let follower = |a: &Address, f: &hc_core::vault::Follower| {
        json!({"user": a, "vaultEquity": usd(per_share(f.shares)), "deposited": usd(Usd(f.cost)), "pnl": usd(per_share(f.shares) - Usd(f.cost)), "lockupUntil": f.locked_until, "isLeader": *a == v.leader})
    };
    let withdrawable = state.with_margin_ctx(|ctx| state.users.get(&addr).map(|u| u.withdrawable(ctx)).unwrap_or_default());
    Some(json!({
        "vaultAddress": addr, "name": v.name, "description": v.description, "leader": v.leader,
        "createdAt": v.created_ms, "tvl": usd(equity), "withdrawable": usd(withdrawable),
        "followers": v.followers.iter().map(|(a, f)| follower(a, f)).collect::<Vec<_>>(),
        "leaderShare": format!("{:.4}", v.followers.get(&v.leader).map_or(0.0, |f| f.shares as f64 / v.total_shares.max(1) as f64)),
        "followerState": user.and_then(|u| v.followers.get(&u).map(|f| follower(&u, f))),
    }))
}

/// A TWAP's progress, in the shape of the public `twapState`.
pub fn twap_event(state: &State, t: &hc_core::events::TwapEvent) -> Value {
    let szd = state.metas[t.asset as usize].sz_decimals;
    json!({
        "twapId": t.id, "user": t.user, "coin": state.metas[t.asset as usize].name, "side": t.side,
        "sz": sz(t.sz as i64, szd), "executedSz": sz(t.filled as i64, szd), "executedNtl": usd(Usd(t.filled_ntl)),
        "avgPx": (t.filled > 0).then(|| px(hc_core::fixed::Px((t.filled_ntl as i128 * hc_core::fixed::pow10(szd) / t.filled as i128) as u64))),
        "minutes": t.minutes, "status": t.status,
    })
}

/// A user's running TWAP orders, with start time and slices sent.
pub fn twaps(state: &State, user: Address) -> Value {
    Value::Array(
        state
            .twaps
            .values()
            .filter(|t| t.user == user)
            .map(|t| {
                let mut v = twap_event(state, &match t.event(hc_core::events::TwapStatus::Running) {
                    Event::Twap(e) => e,
                    _ => unreachable!("twap event"),
                });
                v["timestamp"] = t.start_ms.into();
                v["reduceOnly"] = t.reduce_only.into();
                v["randomize"] = t.randomize.into();
                v["slicesDone"] = t.slices_done.into();
                v["slices"] = t.n_slices().into();
                v
            })
            .collect(),
    )
}

/// One engine event as explorer JSON, tagged with `type`.
pub fn event(state: &State, e: &Event) -> Value {
    let coin = |a: hc_core::types::AssetId| state.metas[a as usize].name.clone();
    let szd = |a: hc_core::types::AssetId| state.metas[a as usize].sz_decimals;
    match e {
        Event::Fill(f) => json!({"type": "fill", "fill": fill(state, f)}),
        Event::Order(o) => json!({"type": "order", "user": o.user, "update": order_update(state, o)}),
        Event::Ledger { user, kind, usd: u } => json!({"type": "ledger", "user": user, "kind": kind, "usdc": usd(*u)}),
        Event::Funding { user, asset, usdc, szi, rate, .. } => {
            json!({"type": "funding", "user": user, "coin": coin(*asset), "usdc": usd(*usdc), "szi": sz(*szi, szd(*asset)), "fundingRate": rate.to_string()})
        }
        Event::Liquidation { user, method, account_value, positions } => json!({"type": "liquidation", "user": user, "method": method, "accountValue": usd(*account_value),
            "positions": positions.iter().map(|(a, s)| json!({"coin": coin(*a), "szi": sz(*s, szd(*a))})).collect::<Vec<_>>()}),
        Event::Risk { user, kind, account_value } => json!({"type": "risk", "user": user, "kind": kind, "accountValue": usd(*account_value)}),
        Event::CoreWriter { user, asset, response, tx_hash } => {
            json!({"type": "coreWriter", "user": user, "coin": asset.map(coin), "response": order_response(state, response, asset.map(szd)), "tx": tx_hash})
        }
        Event::CoreWriterQueued { user, tx_hash, execute_at_ms } => json!({"type": "coreWriterQueued", "user": user, "tx": tx_hash, "executeAt": execute_at_ms}),
        Event::EvmTxQueued { tx_hash, big } => json!({"type": "evmTxQueued", "hash": tx_hash, "big": big}),
        Event::EvmTx(r) => json!({"type": "evmTx", "hash": r.tx_hash, "success": r.success, "gasUsed": r.gas_used}),
        Event::EvmTxSkipped { tx_hash, reason } => json!({"type": "evmTxSkipped", "hash": tx_hash, "reason": reason}),
        Event::EvmBlock { number, hash, n_txs, gas_used, big } => json!({"type": "evmBlock", "number": number, "hash": hash, "nTxs": n_txs, "gasUsed": gas_used, "big": big}),
        Event::Twap(t) => json!({"type": "twap", "twap": twap_event(state, t)}),
        Event::Prices { oracle_pxs, mark_pxs } => json!({"type": "prices", "prices": state.metas.iter().enumerate().map(|(i, m)| json!({
            "coin": m.name, "oraclePx": oracle_pxs.get(i).map(|p| px(*p)), "markPx": mark_pxs.get(i).map(|p| px(*p)),
        })).collect::<Vec<_>>()}),
    }
}

/// Most events and trades an execution view lists; totals cover all of them.
const EXECUTION_LIST_LIMIT: usize = 1_000;

/// Execution of a transaction (or of a block's end-of-block processing):
/// its events, the trades it made (taker with maker), and what changed per
/// account: positions before and after, realized PnL, fees, funding and transfers.
pub fn execution(state: &State, events: &[Event]) -> Value {
    #[derive(Default)]
    struct Change {
        pnl: i64,
        fees: i64,
        funding: i64,
        transfers: i64,
        positions: std::collections::BTreeMap<hc_core::types::AssetId, (i64, i64)>,
        fills: usize,
    }
    let mut changes: std::collections::BTreeMap<Address, Change> = Default::default();
    let mut trades = Vec::new();
    let mut n_trades = 0usize;
    let (mut volume, mut fees) = (0i64, 0i64);
    let fills: Vec<&FillEvent> = events.iter().filter_map(|e| if let Event::Fill(f) = e { Some(f) } else { None }).collect();
    for f in &fills {
        let c = changes.entry(f.user).or_default();
        c.pnl += f.closed_pnl.0;
        c.fees += f.fee.0;
        c.fills += 1;
        fees += f.fee.0;
        let after = f.start_position + f.side.sign() * f.sz as i64;
        c.positions.entry(f.asset).and_modify(|p| p.1 = after).or_insert((f.start_position, after));
    }
    // Fills come in (taker, maker) pairs.
    for pair in fills.chunks(2) {
        let [t, m] = pair else { continue };
        let meta = &state.metas[t.asset as usize];
        n_trades += 1;
        volume += ntl(t.px, t.sz as i64, meta.sz_decimals).0.abs();
        if trades.len() < EXECUTION_LIST_LIMIT {
            trades.push(json!({
                "tid": t.tid, "coin": meta.name, "side": t.side, "px": px(t.px), "sz": sz(t.sz as i64, meta.sz_decimals),
                "notional": usd(ntl(t.px, t.sz as i64, meta.sz_decimals)),
                "taker": t.user, "takerOid": t.oid, "takerFee": usd(t.fee), "takerClosedPnl": usd(t.closed_pnl),
                "maker": m.user, "makerOid": m.oid, "makerFee": usd(m.fee), "makerClosedPnl": usd(m.closed_pnl),
                "liquidation": t.liquidation,
            }));
        }
    }
    for e in events {
        match e {
            Event::Funding { user, usdc, .. } => changes.entry(*user).or_default().funding += usdc.0,
            Event::Ledger { user, kind, usd: u } => {
                let sign = if matches!(kind, hc_core::events::LedgerKind::CoreToEvm) { -1 } else { 1 };
                changes.entry(*user).or_default().transfers += sign * u.0;
            }
            _ => {}
        }
    }
    let n_changes = changes.len();
    let changes: Vec<Value> = changes
        .into_iter()
        .take(EXECUTION_LIST_LIMIT)
        .map(|(user, c)| {
            json!({
                "user": user, "fills": c.fills,
                "closedPnl": usd(Usd(c.pnl)), "fees": usd(Usd(c.fees)), "funding": usd(Usd(c.funding)), "transfers": usd(Usd(c.transfers)),
                "net": usd(Usd(c.pnl - c.fees + c.funding + c.transfers)),
                "positions": c.positions.iter().map(|(a, (b, af))| {
                    let m = &state.metas[*a as usize];
                    json!({"coin": m.name, "before": sz(*b, m.sz_decimals), "after": sz(*af, m.sz_decimals)})
                }).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut by_type: std::collections::BTreeMap<&str, usize> = Default::default();
    for e in events {
        let t = match e {
            Event::Fill(_) => "fill",
            Event::Order(_) => "order",
            Event::Ledger { .. } => "ledger",
            Event::Funding { .. } => "funding",
            Event::Liquidation { .. } => "liquidation",
            Event::Risk { .. } => "risk",
            Event::CoreWriter { .. } => "coreWriter",
            Event::EvmTx(_) => "evmTx",
            Event::EvmTxSkipped { .. } => "evmTxSkipped",
            Event::EvmBlock { .. } => "evmBlock",
            Event::Prices { .. } => "prices",
            Event::Twap(_) => "twap",
            Event::EvmTxQueued { .. } => "evmTxQueued",
            Event::CoreWriterQueued { .. } => "coreWriterQueued",
        };
        *by_type.entry(t).or_default() += 1;
    }
    json!({
        "nEvents": events.len(), "eventCounts": by_type,
        "events": events.iter().take(EXECUTION_LIST_LIMIT).map(|e| event(state, e)).collect::<Vec<_>>(),
        "nTrades": n_trades, "volume": usd(Usd(volume)), "fees": usd(Usd(fees)),
        "trades": trades,
        "nAccounts": n_changes, "changes": changes,
    })
}
