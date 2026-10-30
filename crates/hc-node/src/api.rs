//! HTTP + WebSocket API, shaped after the public Hyperliquid API.
//!
//! - `POST /info` with `{"type": "meta" | "metaAndAssetCtxs" | "allMids" | "l2Book" | "clearinghouseState" | "openOrders" | "candleSnapshot"}`
//! - `POST /exchange` with a signed transaction; replies after execution
//! - `GET /ws`:
//!   - `{"method":"subscribe","subscription":{"type":"l2Book","coin":"BTC"}}`, and likewise
//!     `trades`, `allMids`, `candle{coin, interval}`, `orderUpdates{user}`, `userEvents{user}`, `blocks`
//!   - `{"method":"post","id":1,"request":{"type":"info"|"action","payload":{...}}}`
//!   - `{"method":"ping"}` -> `{"channel":"pong"}`

use crate::node::Shared;
use crate::views;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State as AxState;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use hc_core::events::Event;
use hc_core::types::Address;
use hc_core::{BlockOutput, SignedTx};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};

pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/info", post(http_info))
        .route("/exchange", post(http_exchange))
        .route("/ws", get(ws_upgrade))
        .route("/evm", post(http_evm))
        .route("/", get(trade_page))
        .route("/explorer", get(explorer_page))
        .route("/ui/hcsign.js", get(signer_js))
        .route("/snapshot", get(snapshot_file))
        .with_state(shared)
}

/// This node's latest state snapshot (header line + postcard state), for a
/// peer that is too far behind to catch up from blocks alone.
async fn snapshot_file(AxState(shared): AxState<Arc<Shared>>) -> impl IntoResponse {
    let Some(log) = shared.log_path.get() else { return (axum::http::StatusCode::NOT_FOUND, "no block log").into_response() };
    match tokio::fs::read(crate::node::snapshot_path(log)).await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response(),
        Err(_) => (axum::http::StatusCode::NOT_FOUND, "no snapshot yet").into_response(),
    }
}

async fn http_info(AxState(shared): AxState<Arc<Shared>>, Json(req): Json<Value>) -> impl IntoResponse {
    match info(&shared, &req) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

async fn http_evm(AxState(shared): AxState<Arc<Shared>>, Json(req): Json<Value>) -> impl IntoResponse {
    // JSON-RPC batches are arrays of requests.
    match req {
        Value::Array(reqs) => Json(Value::Array(reqs.iter().map(|r| crate::evm_rpc::handle(&shared, r)).collect())),
        r => Json(crate::evm_rpc::handle(&shared, &r)),
    }
}

async fn explorer_page() -> impl IntoResponse {
    axum::response::Html(include_str!("ui/explorer.html"))
}

async fn trade_page() -> impl IntoResponse {
    axum::response::Html(include_str!("ui/trade.html"))
}

async fn signer_js() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("ui/hcsign.js"))
}

async fn http_exchange(AxState(shared): AxState<Arc<Shared>>, Json(req): Json<Value>) -> impl IntoResponse {
    Json(exchange(&shared, req).await)
}

fn user_param(req: &Value) -> Result<Address, String> {
    req.get("user").and_then(Value::as_str).and_then(Address::from_hex).ok_or_else(|| "missing or invalid user".into())
}

pub fn info(shared: &Shared, req: &Value) -> Result<Value, String> {
    let st = shared.state.read().expect("state lock");
    let coin = || -> Result<usize, String> {
        let c = req.get("coin").and_then(Value::as_str).ok_or("missing coin")?;
        views::coin_index(&st, c).ok_or_else(|| format!("unknown coin {c}"))
    };
    match req.get("type").and_then(Value::as_str) {
        Some("meta") => Ok(views::meta(&st)),
        Some("metaAndAssetCtxs") => Ok(views::meta_and_asset_ctxs(&st)),
        Some("allMids") => Ok(views::all_mids(&st)),
        Some("l2Book") => Ok(views::l2_book(&st, coin()?, 20)),
        Some("clearinghouseState") => Ok(views::clearinghouse_state(&st, user_param(req)?)),
        Some("openOrders") => Ok(views::open_orders(&st, user_param(req)?)),
        Some("exchangeStatus") => Ok(json!({"height": st.height, "time": st.time_ms, "stateHash": st.hash(), "evmBlock": st.evm.block_number()})),
        Some("explorerBlocks") => {
            let limit = req.get("limit").and_then(Value::as_u64).unwrap_or(50).min(500) as usize;
            let before = req.get("before").and_then(Value::as_u64);
            Ok(json!(shared.history.read().expect("history").recent_blocks(limit, before)))
        }
        // {"type":"stateHash","height":h}: this node's state hash after block h
        // (latest without height). Nodes that agree return the same hash.
        Some("stateHash") => {
            let hist = shared.history.read().expect("history");
            let b = match req.get("height").and_then(Value::as_u64) {
                Some(h) => hist.blocks.binary_search_by_key(&h, |b| b.height).ok().map(|i| &hist.blocks[i]),
                None => hist.blocks.last(),
            };
            let b = b.ok_or("unknown block")?;
            Ok(json!({"height": b.height, "stateHash": b.state_hash, "latest": hist.blocks.last().map(|l| l.height)}))
        }
        // {"type":"blockLog","from":h}: finalized blocks from height h, for a peer catching up.
        Some("blockLog") => {
            let from = req.get("from").and_then(Value::as_u64).ok_or("missing from")?;
            let path = shared.log_path.get().ok_or("this node serves no block log")?;
            let entries = crate::node::read_log(path, from, 200).map_err(|e| e.to_string())?;
            Ok(json!({"entries": entries}))
        }
        Some("explorerBlock") => {
            let h = req.get("height").and_then(Value::as_u64).ok_or("missing height")?;
            let hist = shared.history.read().expect("history");
            let b = hist.blocks.iter().find(|b| b.height == h).ok_or("unknown block")?;
            let txs: Vec<Value> = b.tx_hashes.iter().filter_map(|t| hist.txs.get(t)).map(|t| t.to_json(&st)).collect();
            let system = b.system_events.as_ref().map(|ev| views::execution(&st, ev));
            let confirmations = hist.blocks.last().map_or(0, |l| l.height - b.height);
            Ok(json!({"block": b, "txs": txs, "endOfBlock": system, "confirmations": confirmations}))
        }
        Some("explorerTx") => {
            let h = req.get("hash").and_then(Value::as_str).and_then(|s| hc_core::types::hex_decode(s.trim_start_matches("0x"))).ok_or("missing hash")?;
            let h: [u8; 32] = h.try_into().map_err(|_| "bad hash length")?;
            let hist = shared.history.read().expect("history");
            let tx = hist.txs.get(&hc_core::types::Hash32(h)).ok_or("unknown transaction")?;
            let receipt = hist.evm_receipts.get(&hc_core::types::Hash32(h)).map(|(n, bh, r)| json!({"evmBlock": n, "evmBlockHash": bh, "receipt": r}));
            let confirmations = hist.blocks.last().map_or(0, |l| l.height - tx.height);
            Ok(json!({"tx": tx.to_json(&st), "evm": receipt, "execution": tx.execution(&st), "confirmations": confirmations}))
        }
        Some("explorerAddress") => {
            let user = user_param(req)?;
            let hist = shared.history.read().expect("history");
            let activity: Vec<_> = hist.activity.get(&user).map(|a| a.iter().rev().take(200).collect()).unwrap_or_default();
            let evm_balance = hc_core::evm_bridge::wei_string(&st.evm.balance(user.0));
            Ok(json!({
                "address": user,
                "core": views::clearinghouse_state(&st, user),
                "openOrders": views::open_orders(&st, user),
                "twaps": views::twaps(&st, user),
                // Agents (API wallets) this account approved, and the account this address signs for if it is an agent.
                "agents": st.agents.iter().filter(|(_, a)| a.owner == user).map(|(addr, a)| json!({"address": addr, "name": a.name})).collect::<Vec<_>>(),
                "agentOf": st.agents.get(&user).map(|a| a.owner),
                // If this address is a vault, its details; and the vaults this address leads.
                "vault": views::vault(&st, user, None),
                "leads": st.vaults.iter().filter(|(_, v)| v.leader == user).map(|(a, v)| json!({"vaultAddress": a, "name": v.name})).collect::<Vec<_>>(),
                "risk": views::risk(&st, user),
                "evm": {"balanceWei": evm_balance, "nonce": st.evm.nonce(user.0), "isContract": !st.evm.code(user.0).is_empty(), "usingBigBlocks": st.big_block_users.contains(&user)},
                "activity": activity,
            }))
        }
        Some("supply") => Ok(views::supply(&st)),
        // {"type":"vaults","user"?}: every vault, with the user's share if given.
        Some("vaults") => {
            let user = user_param(req).ok();
            Ok(Value::Array(st.vaults.keys().filter_map(|a| views::vault(&st, *a, user)).collect()))
        }
        // {"type":"vaultDetails","vaultAddress","user"?}
        Some("vaultDetails") => {
            let a = req.get("vaultAddress").cloned().ok_or("missing vaultAddress")?;
            let a: hc_core::types::Address = serde_json::from_value(a).map_err(|e| e.to_string())?;
            views::vault(&st, a, user_param(req).ok()).ok_or_else(|| "unknown vault".into())
        }
        Some("twapStates") => Ok(views::twaps(&st, user_param(req)?)),
        Some("candleSnapshot") => {
            // {"type":"candleSnapshot","req":{"coin":"BTC","interval":"1m","startTime":ms,"endTime":ms}}
            let r = req.get("req").ok_or("missing req")?;
            let c = r.get("coin").and_then(Value::as_str).ok_or("missing coin")?;
            let a = views::coin_index(&st, c).ok_or_else(|| format!("unknown coin {c}"))?;
            let name = r.get("interval").and_then(Value::as_str).ok_or("missing interval")?;
            let (i, _) = crate::history::interval_ms(name).ok_or_else(|| format!("unsupported interval {name}"))?;
            let start = r.get("startTime").and_then(Value::as_u64).unwrap_or(0);
            let end = r.get("endTime").and_then(Value::as_u64).unwrap_or(u64::MAX);
            let hist = shared.history.read().expect("history");
            Ok(Value::Array(hist.candles_between(a as u32, i, start, end).into_iter().map(|k| crate::history::candle_json(&st, a, i, k)).collect()))
        }
        other => Err(format!("unsupported info type {other:?}")),
    }
}

/// Submits a signed transaction and waits (bounded) for its execution result.
pub async fn exchange(shared: &Shared, req: Value) -> Value {
    match serde_json::from_value(req) {
        Ok(tx) => exchange_tx(shared, tx).await,
        Err(e) => json!({"status": "err", "response": format!("invalid transaction: {e}")}),
    }
}

/// `{"method":"post","id":…,"request":{"type":"action","payload":<tx>}}`,
/// decoded straight into a transaction (the hot path under load).
#[derive(serde::Deserialize)]
struct WsPostAction {
    method: PostMethod,
    id: Value,
    request: WsActionRequest,
}

#[derive(serde::Deserialize)]
enum PostMethod {
    #[serde(rename = "post")]
    Post,
}

#[derive(serde::Deserialize)]
struct WsActionRequest {
    #[serde(rename = "type")]
    kind: ActionKind,
    payload: SignedTx,
}

#[derive(serde::Deserialize)]
enum ActionKind {
    #[serde(rename = "action")]
    Action,
}

pub async fn exchange_tx(shared: &Shared, tx: SignedTx) -> Value {
    let (send, recv) = oneshot::channel();
    let h = tx.hash();
    shared.waiters.add(h, send);
    if !shared.submit_hashed(h, tx) {
        shared.waiters.take(&h);
        return json!({"status": "err", "response": "rejected by mempool (duplicate or bad signature)"});
    }
    match tokio::time::timeout(Duration::from_secs(30), recv).await {
        Ok(Ok(v)) => v,
        _ => json!({"status": "err", "response": "timed out waiting for inclusion"}),
    }
}

async fn ws_upgrade(ws: WebSocketUpgrade, AxState(shared): AxState<Arc<Shared>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_session(socket, shared))
}

/// Pushes messages for `sub` produced by one block.
fn render(shared: &Shared, sub: &Value, out: &BlockOutput) -> Vec<Value> {
    let st = shared.state.read().expect("state lock");
    let coin = sub.get("coin").and_then(Value::as_str).and_then(|c| views::coin_index(&st, c));
    let user = sub.get("user").and_then(Value::as_str).and_then(Address::from_hex);
    let touches = |a: usize| {
        out.events.iter().any(|e| match e {
            Event::Fill(f) => f.asset as usize == a,
            Event::Order(o) => o.asset as usize == a,
            _ => false,
        })
    };
    match sub.get("type").and_then(Value::as_str) {
        Some("l2Book") => coin.filter(|a| touches(*a)).map(|a| vec![json!({"channel": "l2Book", "data": views::l2_book(&st, a, 20)})]).unwrap_or_default(),
        Some("trades") => {
            let t = coin.map(|a| views::trades(&st, &out.events, a)).unwrap_or_default();
            if t.is_empty() { vec![] } else { vec![json!({"channel": "trades", "data": t})] }
        }
        Some("candle") => {
            let interval = sub.get("interval").and_then(Value::as_str).and_then(crate::history::interval_ms);
            match (coin.filter(|a| touches(*a)), interval) {
                (Some(a), Some((i, _))) => {
                    let hist = shared.history.read().expect("history");
                    hist.candle_at(a as u32, i, out.time_ms).map(|c| vec![json!({"channel": "candle", "data": crate::history::candle_json(&st, a, i, c)})]).unwrap_or_default()
                }
                _ => vec![],
            }
        }
        Some("allMids") if !out.events.is_empty() => vec![json!({"channel": "allMids", "data": {"mids": views::all_mids(&st)}})],
        Some("orderUpdates") => {
            let ups: Vec<Value> = out
                .events
                .iter()
                .filter_map(|e| match e { Event::Order(o) if Some(o.user) == user => Some(views::order_update(&st, o)), _ => None })
                .collect();
            if ups.is_empty() { vec![] } else { vec![json!({"channel": "orderUpdates", "data": ups})] }
        }
        Some("userEvents") => {
            let mut msgs = Vec::new();
            let fills: Vec<Value> = out
                .events
                .iter()
                .filter_map(|e| match e { Event::Fill(f) if Some(f.user) == user => Some(views::fill(&st, f)), _ => None })
                .collect();
            if !fills.is_empty() {
                msgs.push(json!({"channel": "user", "data": {"fills": fills}}));
            }
            for e in &out.events {
                match e {
                    Event::Funding { user: u, asset, usdc, szi, rate, n_samples } if Some(*u) == user => {
                        let meta = &st.metas[*asset as usize];
                        msgs.push(json!({"channel": "user", "data": {"funding": {
                            "time": out.time_ms, "coin": meta.name, "usdc": usdc.to_string(),
                            "szi": hc_core::fixed::format_decimal(*szi as i128, meta.sz_decimals),
                            "fundingRate": rate.to_string(), "nSamples": n_samples,
                        }}}));
                    }
                    Event::Twap(t) if Some(t.user) == user => {
                        msgs.push(json!({"channel": "user", "data": {"twap": views::twap_event(&st, t)}}));
                    }
                    Event::Risk { user: u, kind, account_value } if Some(*u) == user => {
                        msgs.push(json!({"channel": "user", "data": {"risk": {"kind": kind, "accountValue": account_value.to_string()}}}));
                    }
                    Event::Liquidation { user: u, method, account_value, positions } if Some(*u) == user => {
                        msgs.push(json!({"channel": "user", "data": {"liquidation": {
                            "liquidated_user": u, "method": method, "liquidated_account_value": account_value.to_string(),
                            "positions": positions.iter().map(|(a, s)| json!({"coin": st.metas[*a as usize].name, "szi": hc_core::fixed::format_decimal(*s as i128, st.metas[*a as usize].sz_decimals)})).collect::<Vec<_>>(),
                        }}}));
                    }
                    _ => {}
                }
            }
            msgs
        }
        Some("blocks") => vec![json!({"channel": "blocks", "data": {
            "height": out.height, "time": out.time_ms, "stateHash": out.state_hash, "nTxs": out.responses.len(),
        }})],
        _ => vec![],
    }
}

/// Initial snapshot sent right after subscribing.
fn snapshot(shared: &Shared, sub: &Value) -> Option<Value> {
    let st = shared.state.read().expect("state lock");
    match sub.get("type").and_then(Value::as_str) {
        Some("l2Book") => {
            let a = views::coin_index(&st, sub.get("coin")?.as_str()?)?;
            Some(json!({"channel": "l2Book", "data": views::l2_book(&st, a, 20)}))
        }
        Some("allMids") => Some(json!({"channel": "allMids", "data": {"mids": views::all_mids(&st)}})),
        _ => None,
    }
}

const SUBSCRIPTION_TYPES: [&str; 7] = ["l2Book", "trades", "allMids", "candle", "orderUpdates", "userEvents", "blocks"];

async fn ws_session(socket: WebSocket, shared: Arc<Shared>) {
    let (mut sink, mut stream) = socket.split();
    let mut blocks = shared.outputs.subscribe();
    let mut subs: Vec<Value> = Vec::new();
    // Post replies arrive asynchronously (actions wait for inclusion).
    let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    loop {
        let outgoing: Vec<Value> = tokio::select! {
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(text) = msg else { continue };
                // Orders: decode once, straight into the transaction type.
                if let Ok(WsPostAction { method: PostMethod::Post, id, request: WsActionRequest { kind: ActionKind::Action, payload } }) =
                    serde_json::from_str::<WsPostAction>(&text)
                {
                    let (shared, reply_tx) = (shared.clone(), reply_tx.clone());
                    tokio::spawn(async move {
                        let r = exchange_tx(&shared, payload).await;
                        let _ = reply_tx.send(json!({"channel": "post", "data": {"id": id, "response": {"type": "action", "payload": r}}}));
                    });
                    continue;
                }
                match serde_json::from_str::<Value>(&text) {
                    Ok(req) => handle_ws_request(&shared, req, &mut subs, &reply_tx),
                    Err(_) => vec![json!({"channel": "error", "data": "invalid json"})],
                }
            }
            block = blocks.recv() => match block {
                Ok(out) => subs.iter().flat_map(|s| render(&shared, s, &out)).collect(),
                Err(broadcast::error::RecvError::Lagged(n)) => vec![json!({"channel": "error", "data": format!("lagged {n} blocks")})],
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(reply) = reply_rx.recv() => vec![reply],
        };
        for v in outgoing {
            if sink.send(Message::Text(v.to_string().into())).await.is_err() {
                return;
            }
        }
    }
}

fn handle_ws_request(
    shared: &Arc<Shared>,
    req: Value,
    subs: &mut Vec<Value>,
    reply_tx: &tokio::sync::mpsc::UnboundedSender<Value>,
) -> Vec<Value> {
    match req.get("method").and_then(Value::as_str) {
        Some("ping") => vec![json!({"channel": "pong"})],
        Some(m @ ("subscribe" | "unsubscribe")) => {
            let Some(sub) = req.get("subscription").cloned() else {
                return vec![json!({"channel": "error", "data": "missing subscription"})];
            };
            if !sub.get("type").and_then(Value::as_str).is_some_and(|t| SUBSCRIPTION_TYPES.contains(&t)) {
                return vec![json!({"channel": "error", "data": format!("unsupported subscription {sub}")})];
            }
            let mut out = vec![json!({"channel": "subscriptionResponse", "data": {"method": m, "subscription": sub}})];
            if m == "subscribe" {
                out.extend(snapshot(shared, &sub));
                subs.push(sub);
            } else {
                subs.retain(|s| *s != sub);
            }
            out
        }
        Some("post") => {
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let request = req.get("request").cloned().unwrap_or(Value::Null);
            let payload = request.get("payload").cloned().unwrap_or(Value::Null);
            match request.get("type").and_then(Value::as_str) {
                Some("info") => {
                    let response = match info(shared, &payload) {
                        Ok(data) => json!({"type": "info", "payload": {"type": payload.get("type"), "data": data}}),
                        Err(e) => json!({"type": "error", "payload": e}),
                    };
                    vec![json!({"channel": "post", "data": {"id": id, "response": response}})]
                }
                Some("action") => {
                    let shared = shared.clone();
                    let reply_tx = reply_tx.clone();
                    tokio::spawn(async move {
                        let r = exchange(&shared, payload).await;
                        let _ = reply_tx.send(json!({"channel": "post", "data": {"id": id, "response": {"type": "action", "payload": r}}}));
                    });
                    vec![]
                }
                _ => vec![json!({"channel": "post", "data": {"id": id, "response": {"type": "error", "payload": "unknown request type"}}})],
            }
        }
        _ => vec![json!({"channel": "error", "data": "unknown method"})],
    }
}
