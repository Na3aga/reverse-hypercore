//! Vaults: an account that a leader trades and others deposit into.
//!
//! Following Hyperliquid's public documentation:
//! - `createVault` opens a vault with the leader's first deposit (at least
//!   100 USDC). The vault is an ordinary trading account at its own address.
//! - The leader trades it by signing actions with `vaultAddress` set (or
//!   through an agent of the leader).
//! - `vaultTransfer` deposits mint shares at the vault's current value per
//!   share; withdrawals burn shares at the current value, once a depositor's
//!   last deposit is a day old, and only from what the vault can withdraw
//!   (its free margin).
//! - On a follower's withdrawal the leader receives 10% of the profit being
//!   withdrawn. The leader must keep at least 5% of the shares.

use crate::events::{Event, LedgerKind, TxResponse};
use crate::exchange::State;
use crate::fixed::Usd;
use crate::types::Address;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MIN_CREATE_USD: Usd = Usd(100_000_000);
pub const LOCKUP_MS: u64 = 86_400_000;
pub const LEADER_MIN_SHARE_PCT: u128 = 5;
pub const PROFIT_SHARE_PCT: i64 = 10;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vault {
    pub name: String,
    pub description: String,
    pub leader: Address,
    pub created_ms: u64,
    pub total_shares: u128,
    pub followers: BTreeMap<Address, Follower>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Follower {
    pub shares: u128,
    /// USDC deposited for the shares still held (for the profit share).
    pub cost: i64,
    /// No withdrawals before this time (a day after the last deposit).
    pub locked_until: u64,
}

/// The address of a vault: keccak256("hc-vault" || leader || nonce), last 20 bytes.
pub fn vault_address(leader: Address, nonce: u64) -> Address {
    let mut b = b"hc-vault".to_vec();
    b.extend_from_slice(&leader.0);
    b.extend_from_slice(&nonce.to_be_bytes());
    let h = crate::action::keccak256(&b);
    let mut a = [0u8; 20];
    a.copy_from_slice(&h[12..]);
    Address(a)
}

impl State {
    /// The vault account's total value (cross and isolated).
    pub fn vault_equity(&self, vault: Address) -> Usd {
        self.with_margin_ctx(|ctx| self.users.get(&vault).map(|u| u.total_account_value(ctx)).unwrap_or_default())
    }

    fn withdrawable_of(&self, user: Address) -> Usd {
        self.with_margin_ctx(|ctx| self.users.get(&user).map(|u| u.withdrawable(ctx)).unwrap_or_default())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_vault(&mut self, leader: Address, tx_nonce: u64, name: &str, description: &str, initial_usd: u64, nonce: u64, events: &mut Vec<Event>) -> TxResponse {
        if nonce != tx_nonce {
            return TxResponse::err("Action nonce does not match the transaction nonce.");
        }
        if name.is_empty() || name.len() > 50 || description.len() > 250 {
            return TxResponse::err("Vault name must be 1 to 50 characters and the description at most 250.");
        }
        let usd = Usd(initial_usd as i64);
        if usd < MIN_CREATE_USD {
            return TxResponse::err(format!("A vault needs at least {MIN_CREATE_USD} USDC to start."));
        }
        if usd > self.withdrawable_of(leader) {
            return TxResponse::err("Insufficient withdrawable balance.");
        }
        let addr = vault_address(leader, nonce);
        if self.vaults.contains_key(&addr) || self.users.contains_key(&addr) {
            return TxResponse::err("Vault address already in use.");
        }
        self.users.entry(leader).or_default().balance -= usd;
        self.users.entry(addr).or_default().balance += usd;
        self.risk_baseline(leader, -usd);
        let mut followers = BTreeMap::new();
        followers.insert(leader, Follower { shares: usd.0 as u128, cost: usd.0, locked_until: 0 });
        self.vaults.insert(addr, Vault { name: name.into(), description: description.into(), leader, created_ms: self.time_ms, total_shares: usd.0 as u128, followers });
        events.push(Event::Ledger { user: leader, kind: LedgerKind::VaultDeposit { vault: addr }, usd: -usd });
        TxResponse::Vault { vault_address: addr }
    }

    pub(crate) fn vault_transfer(&mut self, user: Address, vault: Address, is_deposit: bool, usd: u64, events: &mut Vec<Event>) -> TxResponse {
        let Some(v) = self.vaults.get(&vault) else { return TxResponse::err("Unknown vault.") };
        let usd = Usd(usd as i64);
        if usd <= Usd::ZERO {
            return TxResponse::err("Invalid amount.");
        }
        let equity = self.vault_equity(vault);
        if equity <= Usd::ZERO {
            return TxResponse::err("Vault has no equity.");
        }
        let (total, leader) = (v.total_shares, v.leader);
        if is_deposit {
            if usd > self.withdrawable_of(user) {
                return TxResponse::err("Insufficient withdrawable balance.");
            }
            let shares = usd.0 as u128 * total / equity.0 as u128;
            if shares == 0 {
                return TxResponse::err("Deposit too small.");
            }
            self.users.entry(user).or_default().balance -= usd;
            self.users.entry(vault).or_default().balance += usd;
            self.risk_baseline(user, -usd);
            let now = self.time_ms;
            let v = self.vaults.get_mut(&vault).expect("vault exists");
            v.total_shares += shares;
            let f = v.followers.entry(user).or_default();
            f.shares += shares;
            f.cost += usd.0;
            if user != leader {
                f.locked_until = now + LOCKUP_MS;
            }
            events.push(Event::Ledger { user, kind: LedgerKind::VaultDeposit { vault }, usd: -usd });
            return TxResponse::Default;
        }
        let Some(f) = v.followers.get(&user).cloned() else { return TxResponse::err("No shares in this vault.") };
        if self.time_ms < f.locked_until {
            return TxResponse::err("Deposits stay in a vault for a day before they can be withdrawn.");
        }
        // Shares to burn, rounded up so the vault never pays out more than they are worth.
        let burn = (usd.0 as u128 * total).div_ceil(equity.0 as u128);
        if burn > f.shares {
            return TxResponse::err("Withdrawal is larger than your share of the vault.");
        }
        if user == leader && total > burn && (f.shares - burn) * 100 < LEADER_MIN_SHARE_PCT * (total - burn) {
            return TxResponse::err(format!("The leader must keep at least {LEADER_MIN_SHARE_PCT}% of the vault."));
        }
        if usd > self.withdrawable_of(vault) {
            return TxResponse::err("The vault cannot release that much margin now; its positions use it.");
        }
        let cost = (f.cost as i128 * burn as i128 / f.shares as i128) as i64;
        let profit = usd.0 - cost;
        let fee = if user != leader && profit > 0 { profit * PROFIT_SHARE_PCT / 100 } else { 0 };
        self.users.entry(vault).or_default().balance -= usd;
        self.users.entry(user).or_default().balance += Usd(usd.0 - fee);
        self.risk_baseline(user, Usd(usd.0 - fee));
        if fee > 0 {
            self.users.entry(leader).or_default().balance += Usd(fee);
            self.risk_baseline(leader, Usd(fee));
            events.push(Event::Ledger { user: leader, kind: LedgerKind::VaultProfitShare { vault }, usd: Usd(fee) });
        }
        let v = self.vaults.get_mut(&vault).expect("vault exists");
        v.total_shares -= burn;
        let fo = v.followers.get_mut(&user).expect("follower exists");
        fo.shares -= burn;
        fo.cost -= cost;
        if fo.shares == 0 {
            v.followers.remove(&user);
        }
        events.push(Event::Ledger { user, kind: LedgerKind::VaultWithdraw { vault }, usd: Usd(usd.0 - fee) });
        TxResponse::Default
    }
}
