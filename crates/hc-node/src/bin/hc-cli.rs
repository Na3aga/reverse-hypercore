//! Minimal client for the devnet WebSocket API, using deterministic dev keys.
//!
//! ```text
//! hc-cli --user alice order BTC buy 100000 0.1 --tif gtc
//! hc-cli --user bob market BTC sell 0.05
//! hc-cli --user alice cancel BTC 12
//! hc-cli --user alice state
//! hc-cli book BTC
//! hc-cli watch --coin BTC --user alice
//! ```

use clap::{Parser, Subcommand};
use futures::{SinkExt, StreamExt};
use hc_core::action::{CancelWire, OrderTypeWire, OrderWire};
use hc_core::types::Tif;
use hc_core::{Action, SignedTx};
use hc_node::dev_user;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "ws://127.0.0.1:8080/ws")]
    url: String,
    /// Dev user name (alice, bob, carol, dave, or any label).
    #[arg(long, default_value = "alice")]
    user: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Limit order.
    Order { coin: String, side: String, px: String, sz: String, #[arg(long, default_value = "gtc")] tif: String, #[arg(long)] reduce_only: bool },
    /// Market order: IOC at mid ± slippage.
    Market { coin: String, side: String, sz: String, #[arg(long, default_value_t = 0.05)] slippage: f64 },
    Cancel { coin: String, oid: u64 },
    /// Leverage of a market, cross or (with --isolated) isolated margin.
    Leverage { coin: String, leverage: u32, #[arg(long)] isolated: bool },
    /// Add (positive) or remove (negative) USDC of an isolated position's margin.
    IsolatedMargin { coin: String, #[arg(allow_hyphen_values = true)] usd: f64 },
    /// TWAP: a market slice every 30 s for `minutes`.
    Twap { coin: String, side: String, sz: String, minutes: u32, #[arg(long)] randomize: bool, #[arg(long)] reduce_only: bool },
    TwapCancel { coin: String, id: u64 },
    /// Send this account's EVM transactions to big blocks (1/min, 30M gas) or back to small ones.
    BigBlocks { #[arg(action = clap::ArgAction::Set)] enable: bool },
    Faucet { usd: String },
    /// Bridge USDC from the core balance to the same address on the EVM.
    SendToEvm { usd: String },
    /// Print the dev user's address and private key (for `cast --private-key`).
    Key,
    /// Explorer view of the user: core state, EVM balance, on-chain activity.
    Address,
    /// USDC supply accounting (conservation check).
    Supply,
    State,
    Orders,
    Book { coin: String },
    Meta,
    /// Stream blocks, trades, book and the user's events.
    Watch { #[arg(long, default_value = "BTC")] coin: String },
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn post(ws: &mut Ws, id: u64, kind: &str, payload: Value) -> anyhow::Result<Value> {
    let msg = json!({"method": "post", "id": id, "request": {"type": kind, "payload": payload}});
    ws.send(Message::Text(msg.to_string().into())).await?;
    while let Some(m) = ws.next().await {
        if let Message::Text(t) = m? {
            let v: Value = serde_json::from_str(&t)?;
            if v["channel"] == "post" && v["data"]["id"] == id {
                return Ok(v["data"]["response"]["payload"].clone());
            }
        }
    }
    anyhow::bail!("connection closed")
}

async fn coin_index(ws: &mut Ws, coin: &str) -> anyhow::Result<u32> {
    let meta = post(ws, 1, "info", json!({"type": "meta"})).await?;
    meta["data"]["universe"]
        .as_array()
        .and_then(|u| u.iter().position(|a| a["name"] == coin))
        .map(|i| i as u32)
        .ok_or_else(|| anyhow::anyhow!("unknown coin {coin}"))
}

fn parse_side(s: &str) -> anyhow::Result<bool> {
    match s {
        "buy" | "b" | "long" => Ok(true),
        "sell" | "s" | "short" => Ok(false),
        _ => anyhow::bail!("side must be buy or sell"),
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_millis() as u64
}

/// Rounds to 5 significant figures (public tick rule), as a decimal string.
fn sig5(px: f64) -> String {
    let digits = px.abs().log10().floor() as i32 + 1;
    let decimals = (5 - digits).max(0) as usize;
    let s = format!("{px:.decimals$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let (key, addr) = dev_user(&args.user);
    if let Cmd::Key = args.cmd {
        println!("{{\"user\": \"{}\", \"address\": \"{}\", \"privateKey\": \"0x{}\"}}", args.user, addr, hc_core::types::hex_encode(&key.to_bytes()));
        return Ok(());
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(&args.url).await?;
    let sign = |action: Action| -> Value { serde_json::to_value(SignedTx::sign(&key, action, now_ms())).expect("tx json") };
    let out = match args.cmd {
        Cmd::Order { coin, side, px, sz, tif, reduce_only } => {
            let a = coin_index(&mut ws, &coin).await?;
            let tif = match tif.as_str() { "alo" => Tif::Alo, "ioc" => Tif::Ioc, _ => Tif::Gtc };
            let o = OrderWire { a, b: parse_side(&side)?, p: px, s: sz, r: reduce_only, t: OrderTypeWire::limit(tif), c: None };
            post(&mut ws, 2, "action", sign(Action::order(vec![o]))).await?
        }
        Cmd::Market { coin, side, sz, slippage } => {
            let a = coin_index(&mut ws, &coin).await?;
            let mids = post(&mut ws, 3, "info", json!({"type": "allMids"})).await?;
            let ctx = post(&mut ws, 4, "info", json!({"type": "metaAndAssetCtxs"})).await?;
            let mid: f64 = mids["data"][&coin].as_str().or(ctx["data"][1][a as usize]["markPx"].as_str()).unwrap_or("0").parse()?;
            let is_buy = parse_side(&side)?;
            let px = sig5(if is_buy { mid * (1.0 + slippage) } else { mid * (1.0 - slippage) });
            let o = OrderWire { a, b: is_buy, p: px, s: sz, r: false, t: OrderTypeWire::limit(Tif::Ioc), c: None };
            post(&mut ws, 5, "action", sign(Action::order(vec![o]))).await?
        }
        Cmd::Cancel { coin, oid } => {
            let a = coin_index(&mut ws, &coin).await?;
            post(&mut ws, 6, "action", sign(Action::Cancel { cancels: vec![CancelWire { a, o: oid }] })).await?
        }
        Cmd::Leverage { coin, leverage, isolated } => {
            let a = coin_index(&mut ws, &coin).await?;
            post(&mut ws, 7, "action", sign(Action::UpdateLeverage { asset: a, is_cross: !isolated, leverage })).await?
        }
        Cmd::IsolatedMargin { coin, usd } => {
            let a = coin_index(&mut ws, &coin).await?;
            post(&mut ws, 16, "action", sign(Action::UpdateIsolatedMargin { asset: a, is_buy: true, ntli: (usd * 1e6).round() as i64 })).await?
        }
        Cmd::Twap { coin, side, sz, minutes, randomize, reduce_only } => {
            let a = coin_index(&mut ws, &coin).await?;
            let twap = hc_core::twap::TwapWire { a, b: parse_side(&side)?, s: sz, r: reduce_only, m: minutes, t: randomize };
            post(&mut ws, 17, "action", sign(Action::TwapOrder { twap })).await?
        }
        Cmd::TwapCancel { coin, id } => {
            let a = coin_index(&mut ws, &coin).await?;
            post(&mut ws, 18, "action", sign(Action::TwapCancel { a, t: id })).await?
        }
        Cmd::BigBlocks { enable } => post(&mut ws, 19, "action", sign(Action::EvmUserModify { using_big_blocks: enable })).await?,
        Cmd::Faucet { usd } => post(&mut ws, 8, "action", sign(Action::Faucet { usd })).await?,
        Cmd::SendToEvm { usd } => post(&mut ws, 13, "action", sign(Action::SendToEvm { usd })).await?,
        Cmd::Address => post(&mut ws, 14, "info", json!({"type": "explorerAddress", "user": addr})).await?,
        Cmd::Supply => post(&mut ws, 15, "info", json!({"type": "supply"})).await?,
        Cmd::Key => unreachable!("handled above"),
        Cmd::State => post(&mut ws, 9, "info", json!({"type": "clearinghouseState", "user": addr})).await?,
        Cmd::Orders => post(&mut ws, 10, "info", json!({"type": "openOrders", "user": addr})).await?,
        Cmd::Book { coin } => post(&mut ws, 11, "info", json!({"type": "l2Book", "coin": coin})).await?,
        Cmd::Meta => post(&mut ws, 12, "info", json!({"type": "metaAndAssetCtxs"})).await?,
        Cmd::Watch { coin } => {
            for sub in [
                json!({"type": "blocks"}),
                json!({"type": "trades", "coin": coin}),
                json!({"type": "l2Book", "coin": coin}),
                json!({"type": "userEvents", "user": addr}),
                json!({"type": "orderUpdates", "user": addr}),
            ] {
                ws.send(Message::Text(json!({"method": "subscribe", "subscription": sub}).to_string().into())).await?;
            }
            while let Some(m) = ws.next().await {
                if let Message::Text(t) = m? {
                    println!("{t}");
                }
            }
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
