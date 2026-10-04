// src/main.rs
//
// Entry point for the Gembot call backtest.
//
// Reads a CSV of historical Gembot calls, reconstructs each token's on-chain price path from the call timestamp onward, simulates the
// scale-out exit strategy against that path, and reports aggregate results broken down by tier.
//
// Each call's result prints immediately once computed, not only in the final summary, so a long run remains useful even if interrupted partway.

mod models;
mod price_path;
mod pumpswap_path;
mod simulator;

use anyhow::{Context, Result};
use models::{CallRecord, Outcome};
use reqwest::Client;
use std::collections::HashMap;

const ENTRY_SOL: f64 = 0.1;

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url = std::env::var("RPC_URL")
        .context("RPC_URL must be set, e.g. your Helius endpoint")?;
    let csv_path = std::env::args().nth(1)
        .context("usage: gembot-backtest <path-to-calls.csv>")?;

    let calls = read_calls(&csv_path)?;
    println!("Loaded {} calls from {}\n", calls.len(), csv_path);

    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("failed to build HTTP client");
    let exit_legs = simulator::default_exit_legs();

    let mut results = Vec::with_capacity(calls.len());
    for (i, call) in calls.iter().enumerate() {
        eprintln!("--- [{}/{}] {} ({}) ---", i + 1, calls.len(), &call.mint_address[..8.min(call.mint_address.len())], call.tier);

        let mut path = price_path::fetch_price_path(&http, &rpc_url, &call.mint_address, call.call_timestamp)
            .await
            .unwrap_or_default();

        let continue_from = path.last().map(|p| p.timestamp).unwrap_or(call.call_timestamp);
        if let Ok(pumpswap_points) = pumpswap_path::fetch_pumpswap_price_path(
            &http, &rpc_url, &call.mint_address, continue_from
        ).await {
            path.extend(pumpswap_points);
        }

        let result = simulator::simulate_call(
            &call.mint_address,
            &call.tier,
            &path,
            ENTRY_SOL,
            &exit_legs,
        );

        // Stream this result immediately, real-time visibility on a long run
        match result.outcome {
            Outcome::NoData => println!("[{}/{}] {} [{}] -- NO PRICE DATA", i + 1, calls.len(), call.mint_address, call.tier),
            _ => println!(
                "[{}/{}] {} [{}] pnl={:+.4} SOL ({:+.1}%)",
                i + 1, calls.len(), call.mint_address, call.tier, result.total_pnl_sol, result.total_pnl_pct
            ),
        }

        results.push(result);
    }

    print_report(&results);
    Ok(())
}

fn read_calls(path: &str) -> Result<Vec<CallRecord>> {
    let mut reader = csv::Reader::from_path(path)
        .with_context(|| format!("could not open {path}"))?;
    let mut calls = Vec::new();
    for record in reader.deserialize() {
        let call: CallRecord = record.context("malformed row in calls CSV")?;
        calls.push(call);
    }
    Ok(calls)
}

fn print_report(results: &[models::SimResult]) {
    println!("\n\n=== OVERALL ===");
    let total = results.len();
    let no_data = results.iter().filter(|r| r.outcome == Outcome::NoData).count();
    let wins = results.iter().filter(|r| r.outcome == Outcome::Win).count();
    let losses = results.iter().filter(|r| r.outcome == Outcome::Loss).count();
    let total_pnl: f64 = results.iter().map(|r| r.total_pnl_sol).sum();

    println!("Total calls:     {total}");
    println!("No price data:   {no_data}");
    println!("Wins:            {wins}");
    println!("Losses:          {losses}");
    println!("Total P&L (SOL): {total_pnl:.4}");

    println!("\n=== BY TIER ===");
    let mut by_tier: HashMap<String, Vec<&models::SimResult>> = HashMap::new();
    for r in results {
        by_tier.entry(r.tier.clone()).or_default().push(r);
    }
    for (tier, rows) in &by_tier {
        let tier_pnl: f64 = rows.iter().map(|r| r.total_pnl_sol).sum();
        let tier_wins = rows.iter().filter(|r| r.outcome == Outcome::Win).count();
        println!("{tier}: {} calls, {} wins, {:.4} SOL total", rows.len(), tier_wins, tier_pnl);
    }

    println!("\n=== PER-CALL DETAIL ===");
    for r in results {
        match r.outcome {
            Outcome::NoData => println!("{} [{}] -- NO PRICE DATA", r.mint, r.tier),
            _ => {
                println!(
                    "{} [{}] ts={}  entry={:.9} SOL  pnl={:+.4} SOL ({:+.1}%)  moonbag={:.4} SOL",
                    r.mint, r.tier, r.entry_timestamp, r.entry_price, r.total_pnl_sol, r.total_pnl_pct, r.moonbag_value
                );
                for (i, (multiplier, sol_returned)) in r.legs_filled.iter().enumerate() {
                    println!("    leg {}: fired at {:.2}x, returned {:.4} SOL", i + 1, multiplier, sol_returned);
                }
            }
        }
    }
}
