//! `mafold wallet` — the token wallet from the CLI. Currency = model id
//! ("claude-fable-5"); 1 unit = 1 output-equivalent token of that model.
//! Amounts accept 1M/2.5B/1000 shorthand. Account-symmetric: `mint` works for
//! anyone to TYPE but only the @mafold first-party account passes the server
//! check — there is no privileged client path.

use anyhow::{bail, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::Client;

#[derive(Subcommand)]
pub enum WalletCmd {
    /// Show your balances.
    Balance,
    /// Transfer tokens: `mafold wallet transfer @friend 1M claude-fable-5`.
    Transfer {
        to: String,
        amount: String,
        currency: String,
        /// Optional note recorded on both ledgers.
        #[arg(long)]
        memo: Option<String>,
    },
    /// Convert between models at the official-price ratio:
    /// `mafold wallet convert 1M claude-opus-4-8 claude-fable-5`.
    Convert { amount: String, from: String, to: String },
    /// Show the official price table (USD / 1M tokens).
    Rates,
    /// Show your ledger (newest first).
    History {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// List / revoke standing debit authorizations you've given.
    Grants {
        /// Revoke this spender's authorization.
        #[arg(long)]
        revoke: Option<String>,
    },
    /// First-party issuance (server-checked: @mafold only).
    Mint {
        to: String,
        amount: String,
        currency: String,
        #[arg(long)]
        memo: Option<String>,
    },
}

/// "1M" / "2.5B" / "1000" → raw tokens.
fn parse_amount(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_lowercase()) {
        Some('k') => (&s[..s.len() - 1], 1e3),
        Some('m') => (&s[..s.len() - 1], 1e6),
        Some('b') => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    let v: f64 = num.trim().parse().map_err(|_| anyhow::anyhow!("bad amount `{s}` — try 1M / 2.5B / 1000"))?;
    let raw = (v * mult).floor() as i64;
    if raw <= 0 {
        bail!("amount must be positive");
    }
    Ok(raw)
}

fn fmt(n: i64) -> String {
    let a = n.abs() as f64;
    if a >= 1e9 { format!("{:.2}B", n as f64 / 1e9) }
    else if a >= 1e6 { format!("{:.2}M", n as f64 / 1e6) }
    else if a >= 1e3 { format!("{:.1}K", n as f64 / 1e3) }
    else { n.to_string() }
}

fn items(v: &Value) -> Vec<Value> {
    v["items"].as_array().cloned().unwrap_or_default()
}

pub async fn run(cmd: WalletCmd, client: &Client) -> Result<()> {
    match cmd {
        WalletCmd::Balance => {
            let r = client.call("walletBalances", json!({})).await?;
            let rows = items(&r);
            if rows.is_empty() {
                println!("(empty — ask a friend for a transfer, e.g. `mafold wallet transfer @you 1M claude-fable-5`)");
                return Ok(());
            }
            for b in rows {
                println!("  {:>10}  {}", fmt(b["amount"].as_i64().unwrap_or(0)), b["currency"].as_str().unwrap_or("?"));
            }
        }
        WalletCmd::Transfer { to, amount, currency, memo } => {
            let amt = parse_amount(&amount)?;
            client
                .call("walletTransfer", json!({ "to": to, "currency": currency, "amount": amt, "memo": memo }))
                .await?;
            println!("✓ sent {} {currency} → {to}", fmt(amt));
        }
        WalletCmd::Convert { amount, from, to } => {
            let amt = parse_amount(&amount)?;
            let r = client
                .call("walletConvert", json!({ "currency": from, "to_currency": to, "amount": amt }))
                .await?;
            println!(
                "✓ {} {from} → {} {to}  (rate {:.4})",
                fmt(amt),
                fmt(r["to_amount"].as_i64().unwrap_or(0)),
                r["rate"].as_f64().unwrap_or(0.0)
            );
        }
        WalletCmd::Rates => {
            let r = client.call("walletRates", json!({})).await?;
            println!("official prices (USD / 1M tokens):");
            for row in items(&r) {
                println!(
                    "  {:<22} in ${:<7} out ${}",
                    row["currency"].as_str().unwrap_or("?"),
                    row["usd_in"].as_f64().unwrap_or(0.0),
                    row["usd_out"].as_f64().unwrap_or(0.0)
                );
            }
        }
        WalletCmd::History { limit } => {
            let r = client.call("walletHistory", json!({ "limit": limit })).await?;
            let rows = items(&r);
            if rows.is_empty() {
                println!("(no transactions)");
                return Ok(());
            }
            for tx in rows {
                let kind = tx["kind"].as_str().unwrap_or("?");
                let sign = if matches!(kind, "xfer_in" | "mint") { "+" } else { "−" };
                let peer = tx["peer"].as_str().map(|p| format!(" @{p}")).unwrap_or_default();
                let memo = tx["memo"].as_str().map(|m| format!("  ({m})")).unwrap_or_default();
                let conv = match (tx["to_amount"].as_i64(), tx["to_currency"].as_str()) {
                    (Some(a), Some(c)) => format!(" → +{} {c}", fmt(a)),
                    _ => String::new(),
                };
                println!(
                    "  {:<8} {sign}{} {}{conv}{peer}{memo}",
                    kind,
                    fmt(tx["amount"].as_i64().unwrap_or(0)),
                    tx["currency"].as_str().unwrap_or("?")
                );
            }
        }
        WalletCmd::Grants { revoke } => {
            if let Some(spender) = revoke {
                let r = client.call("walletGrantRevoke", json!({ "spender": spender })).await?;
                if r["removed"].as_bool().unwrap_or(false) {
                    println!("✓ revoked @{spender}");
                } else {
                    println!("(no grant for @{spender})");
                }
                return Ok(());
            }
            let r = client.call("walletGrants", json!({})).await?;
            let rows = items(&r);
            if rows.is_empty() {
                println!("(no one is authorized to debit your wallet)");
                return Ok(());
            }
            for g in rows {
                let cap = g["monthly_cap"].as_i64().map(fmt).unwrap_or_else(|| "uncapped".into());
                println!(
                    "  @{:<20} cap {cap:<10} used this month {}",
                    g["spender"].as_str().unwrap_or("?"),
                    fmt(g["used_month"].as_i64().unwrap_or(0))
                );
            }
        }
        WalletCmd::Mint { to, amount, currency, memo } => {
            let amt = parse_amount(&amount)?;
            client
                .call("walletMint", json!({ "to": to, "currency": currency, "amount": amt, "memo": memo }))
                .await?;
            println!("✓ minted {} {currency} → {to}", fmt(amt));
        }
    }
    Ok(())
}
