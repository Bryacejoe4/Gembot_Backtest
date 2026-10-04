// src/pumpswap_path.rs
//
// Reconstructs on-chain price history for a token after it graduates from the Pump.fun bonding curve to PumpSwap, Pump.fun's own AMM.
//
// Uses the same getTransactionsForAddress forward-scan as price_path.rs, for the same reason: an actively-traded PumpSwap pool can show
// continuous activity from the call date through today, and reading forward from the call timestamp is far faster than walking backward from "now" to find it.

use crate::models::PricePoint;
use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

const PUMP_PROGRAM_ID:     &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMPSWAP_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const WSOL_MINT:           &str = "So11111111111111111111111111111111111111112";
const UNTIL_WINDOW_SECS:   i64  = 48 * 3600;
const TARGET_POINTS:       usize = 100;
const BATCH_SIZE:          usize = 100;
const MAX_BATCHES:         usize = 5;

/// Derives the canonical PumpSwap pool address for a token that migrated from the Pump.fun bonding curve via Pump.fun's own migrate instruction.
/// Verified against real on-chain data earlier in this project — see technical-learnings.md and the permanent unit test asserting this
/// derivation against a known-correct pool address.
pub fn derive_pumpswap_pool(mint: &str) -> Result<Pubkey> {
    let mint_pk = Pubkey::from_str(mint)?;
    let pump_program = Pubkey::from_str(PUMP_PROGRAM_ID)?;
    let pumpswap_program = Pubkey::from_str(PUMPSWAP_PROGRAM_ID)?;
    let wsol = Pubkey::from_str(WSOL_MINT)?;

    let (pool_authority, _) = Pubkey::find_program_address(
        &[b"pool-authority", mint_pk.as_ref()],
        &pump_program,
    );
    let index_bytes: [u8; 2] = 0u16.to_le_bytes();
    let (pool, _) = Pubkey::find_program_address(
        &[b"pool", &index_bytes, pool_authority.as_ref(), mint_pk.as_ref(), wsol.as_ref()],
        &pumpswap_program,
    );
    Ok(pool)
}

pub async fn fetch_pumpswap_price_path(
    http: &Client,
    rpc_url: &str,
    mint: &str,
    from_timestamp: i64,
) -> Result<Vec<PricePoint>> {
    let until_timestamp = from_timestamp + UNTIL_WINDOW_SECS;
    let pool = derive_pumpswap_pool(mint)?;
    let pool_str = pool.to_string();

    let mut points = Vec::new();
    let mut pagination_token: Option<String> = None;
    let mut batches = 0usize;

    loop {
        let mut params = json!({
            "transactionDetails": "full",
            "sortOrder": "asc",
            "limit": BATCH_SIZE,
            "filters": { "blockTime": { "gte": from_timestamp, "lte": until_timestamp } },
        });
        if let Some(t) = &pagination_token {
            params["paginationToken"] = json!(t);
        }

        let body: Value = http
            .post(rpc_url)
            .json(&json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "getTransactionsForAddress",
                "params": [pool_str, params]
            }))
            .send().await?
            .json().await?;

        if let Some(err) = body.get("error") {
            eprintln!("  [RPC ERROR, PumpSwap] {err}");
            break;
        }

        let items = body["result"]["data"].as_array().cloned().unwrap_or_default();
        batches += 1;
        if items.is_empty() { break; }

        let mut past_window = false;
        for item in &items {
            let block_time = item["blockTime"].as_i64().unwrap_or(0);
            if block_time > until_timestamp { past_window = true; break; }
            if !item["meta"]["err"].is_null() { continue; }

            if let Some(point) = compute_pumpswap_point(item, &pool_str, mint, block_time) {
                points.push(point);
            }
            if points.len() >= TARGET_POINTS { break; }
        }

        pagination_token = body["result"]["paginationToken"].as_str().map(String::from);

        if past_window || points.len() >= TARGET_POINTS || pagination_token.is_none() || batches >= MAX_BATCHES {
            break;
        }
    }

    points.sort_by_key(|p| p.timestamp);
    Ok(points)
}

fn compute_pumpswap_point(item: &Value, pool_str: &str, mint: &str, block_time: i64) -> Option<PricePoint> {
    let pre_tok = item["meta"]["preTokenBalances"].as_array().cloned().unwrap_or_default();
    let post_tok = item["meta"]["postTokenBalances"].as_array().cloned().unwrap_or_default();

    let find_amount = |entries: &[Value], target_mint: &str| -> Option<f64> {
        entries.iter().find(|e| {
            e["mint"].as_str() == Some(target_mint) && e["owner"].as_str() == Some(pool_str)
        }).and_then(|e| e["uiTokenAmount"]["amount"].as_str()?.parse::<f64>().ok())
    };

    let pre_mint  = find_amount(&pre_tok, mint);
    let post_mint = find_amount(&post_tok, mint);
    let pre_wsol  = find_amount(&pre_tok, WSOL_MINT);
    let post_wsol = find_amount(&post_tok, WSOL_MINT);

    let (Some(pre_m), Some(post_m), Some(pre_w), Some(post_w)) = (pre_mint, post_mint, pre_wsol, post_wsol)
        else { return None };

    let mint_delta = (post_m - pre_m).abs() / 1e6;
    let wsol_delta = (post_w - pre_w).abs() / 1e9;
    if mint_delta == 0.0 || wsol_delta == 0.0 { return None; }

    Some(PricePoint { timestamp: block_time, price_sol: wsol_delta / mint_delta })
}

