//! Live oracle input: spot prices from public exchange APIs.
//!
//! Hyperliquid validators each publish the weighted median of spot prices from
//! eight venues (weights 3,2,2,1,1,1,1,1). This feed polls the same kind of
//! venues: Binance, OKX, Bybit, Kraken, KuCoin, Gate and MEXC, plus Coinbase in
//! the slot where Hyperliquid uses its own spot market. Each quote is the
//! venue's best bid/ask mid. The quotes are signed into ordinary `SetOracle`
//! votes, so the block log records them and replay never touches the network.
//!
//! One poller is shared by the in-process validators; on a real network each
//! validator would fetch independently.

use hc_core::fixed::Px;
use serde_json::Value;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Venues in weight order (see `hc_core::oracle::VENUE_WEIGHTS`).
pub const VENUES: [&str; 8] = ["binance", "okx", "bybit", "kraken", "kucoin", "gate", "mexc", "coinbase"];

/// Latest venue quotes per asset (`quotes[asset][venue]`) and when they were fetched.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub quotes: Vec<Vec<Option<Px>>>,
    pub fetched: Option<Instant>,
}

#[derive(Clone)]
pub struct LiveFeed {
    client: reqwest::Client,
    coins: Vec<String>,
    latest: Arc<RwLock<Snapshot>>,
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn mid(bid: &Value, ask: &Value) -> Option<f64> {
    let (b, a) = (num(bid)?, num(ask)?);
    (b > 0.0 && a >= b).then_some((a + b) / 2.0)
}

impl LiveFeed {
    pub fn new(coins: Vec<String>) -> LiveFeed {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(2)).user_agent("hc-devnet-oracle").build().expect("http client");
        LiveFeed { client, coins, latest: Arc::default() }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.latest.read().expect("feed lock").clone()
    }

    async fn get(&self, url: &str) -> Option<Value> {
        self.client.get(url).send().await.ok()?.error_for_status().ok()?.json().await.ok()
    }

    async fn quote(&self, venue: &str, coin: &str) -> Option<f64> {
        let kraken = if coin == "BTC" { "XBT" } else { coin };
        match venue {
            "binance" => {
                let v = self.get(&format!("https://api.binance.com/api/v3/ticker/bookTicker?symbol={coin}USDT")).await?;
                mid(&v["bidPrice"], &v["askPrice"])
            }
            "okx" => {
                let v = self.get(&format!("https://www.okx.com/api/v5/market/ticker?instId={coin}-USDT")).await?;
                mid(&v["data"][0]["bidPx"], &v["data"][0]["askPx"])
            }
            "bybit" => {
                let v = self.get(&format!("https://api.bybit.com/v5/market/tickers?category=spot&symbol={coin}USDT")).await?;
                mid(&v["result"]["list"][0]["bid1Price"], &v["result"]["list"][0]["ask1Price"])
            }
            "kraken" => {
                let v = self.get(&format!("https://api.kraken.com/0/public/Ticker?pair={kraken}USD")).await?;
                let t = v["result"].as_object()?.values().next()?;
                mid(&t["b"][0], &t["a"][0])
            }
            "kucoin" => {
                let v = self.get(&format!("https://api.kucoin.com/api/v1/market/orderbook/level1?symbol={coin}-USDT")).await?;
                mid(&v["data"]["bestBid"], &v["data"]["bestAsk"])
            }
            "gate" => {
                let v = self.get(&format!("https://api.gateio.ws/api/v4/spot/tickers?currency_pair={coin}_USDT")).await?;
                mid(&v[0]["highest_bid"], &v[0]["lowest_ask"])
            }
            "mexc" => {
                let v = self.get(&format!("https://api.mexc.com/api/v3/ticker/bookTicker?symbol={coin}USDT")).await?;
                mid(&v["bidPrice"], &v["askPrice"])
            }
            "coinbase" => {
                let v = self.get(&format!("https://api.exchange.coinbase.com/products/{coin}-USD/ticker")).await?;
                mid(&v["bid"], &v["ask"])
            }
            _ => None,
        }
    }

    /// Fetches every venue for every asset concurrently and stores the result.
    /// Returns the number of quotes received.
    pub async fn poll(&self) -> usize {
        let jobs = self.coins.iter().map(|coin| futures::future::join_all(VENUES.iter().map(move |v| self.quote(v, coin))));
        let quotes: Vec<Vec<Option<Px>>> = futures::future::join_all(jobs)
            .await
            .into_iter()
            .map(|per_venue| per_venue.into_iter().map(|q| q.map(|p| Px((p * 1e6).round() as u64))).collect())
            .collect();
        let n = quotes.iter().flatten().filter(|q| q.is_some()).count();
        if n > 0 {
            *self.latest.write().expect("feed lock") = Snapshot { quotes, fetched: Some(Instant::now()) };
        }
        n
    }

    /// Polls until the feed is stopped.
    pub async fn run(self, period: Duration) {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if self.poll().await == 0 {
                tracing::warn!("oracle feed: no venue answered");
            }
        }
    }

    /// A validator's vote: the weighted median per asset, or `None` if the
    /// snapshot is older than `max_age` or some asset has no quote at all.
    pub fn vote(&self, max_age: Duration) -> Option<Vec<Px>> {
        let s = self.snapshot();
        if s.fetched?.elapsed() > max_age {
            return None;
        }
        s.quotes.iter().map(|q| hc_core::oracle::venue_weighted_median(q)).collect()
    }
}
