// src/price_path.rs
//
// Reconstructs a token's on-chain price path following a Gembot call.
//
// Uses Helius's getTransactionsForAddress rather than the standard getSignaturesForAddress + getTransaction pair. That older combination
// only walks backward from the present, one page at a time, through every transaction newer than the target window before ever reaching
// an old call — for a token still trading heavily today, that walk once cost over 122,000 wasted round trips on a single mint in this project.
// getTransactionsForAddress supports sortOrder=asc plus a blockTime range filter, so a request starts reading forward from the exact call
// timestamp directly, and returns full transaction detail in the same response, removing the separate per-signature fetch entirely. Verified
// directly against Helius before this rewrite (see technical-learnings.md).
//
// Price at each point is still derived from pre/post balance deltas in the transaction's own metadata — the same validator-computed, version-
// independent technique verified earlier in this project. Only the fetching mechanism changed here, not the pricing method.

use crate::models::PricePoint;
use anyhow::{anyhow, Result};
use reqwest::Client;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

const PUMP_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const UNTIL_WINDOW_SECS: i64 = 48 * 3600;

/// Target number of price points per mint. The scale-out simulator only needs enough resolution to catch the 1.25x/1.60x threshold crossings
/// and a reasonably late price for the moonbag, not a full reconstruction.
const TARGET_POINTS: usize = 100;

/// Helius's documented batch size for full transaction detail mode.
const BATCH_SIZE: usize = 100;

/// Hard ceiling on batches fetched per mint, independent of how many points were found, so an unexpected API response can never hang a run indefinitely.
const MAX_BATCHES: usize = 5;

pub async fn fetch_price_path(
    http: &Client,
    rpc_url: &str,
    mint: &str,
    from_timestamp: i64,
) -> Result<Vec<PricePoint>> {
    let until_timestamp = from_timestamp + UNTIL_WINDOW_SECS;
    let bonding_curve = derive_bonding_curve(mint)?;
    let curve_str = bonding_curve.to_string();

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
                "params": [curve_str, params]
            }))
            .send().await?
            .json().await?;

        if let Some(err) = body.get("error") {
            eprintln!("  [RPC ERROR, bonding curve] {err}");
            break;
        }

        let items = body["result"]["data"].as_array().cloned().unwrap_or_default();
        batches += 1;
        if items.is_empty() { break; }

        let mut past_window = false;
        for item in &items {
            let block_time = item["blockTime"].as_i64().unwrap_or(0);
            // Local safety net: honour the window ourselves even though
            // the request already filters server-side.
            if block_time > until_timestamp { past_window = true; break; }
            if !item["meta"]["err"].is_null() { continue; }

            if let Some(point) = compute_bonding_curve_point(item, &curve_str, mint, block_time) {
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

fn derive_bonding_curve(mint: &str) -> Result<Pubkey> {
    let mint_pk = Pubkey::from_str(mint).map_err(|_| anyhow!("invalid mint: {mint}"))?;
    let program = Pubkey::from_str(PUMP_PROGRAM_ID).unwrap();
    let (pda, _) = Pubkey::find_program_address(&[b"bonding-curve", mint_pk.as_ref()], &program);
    Ok(pda)
}

/// Reads accountKeys defensively: the exact encoding returned by getTransactionsForAddress' "full" mode wasn't confirmed field-by-field
/// before this rewrite, so both a plain pubkey string and a {pubkey, signer, writable} object are accepted rather than assuming
/// one shape and risking a silent zero-match if it's the other.
fn compute_bonding_curve_point(item: &Value, curve_str: &str, mint: &str, block_time: i64) -> Option<PricePoint> {
    let account_keys = item["transaction"]["message"]["accountKeys"].as_array().cloned().unwrap_or_default();
    let curve_idx = account_keys.iter().position(|k| {
        k.as_str() == Some(curve_str) || k["pubkey"].as_str() == Some(curve_str)
    })?;

    let pre_balances = item["meta"]["preBalances"].as_array().cloned().unwrap_or_default();
    let post_balances = item["meta"]["postBalances"].as_array().cloned().unwrap_or_default();
    let pre_lamports = pre_balances.get(curve_idx).and_then(|v| v.as_i64()).unwrap_or(0);
    let post_lamports = post_balances.get(curve_idx).and_then(|v| v.as_i64()).unwrap_or(0);
    let sol_delta = (post_lamports - pre_lamports).unsigned_abs() as f64 / 1e9;

    let pre_tok = item["meta"]["preTokenBalances"].as_array().cloned().unwrap_or_default();
    let post_tok = item["meta"]["postTokenBalances"].as_array().cloned().unwrap_or_default();

    let find_curve_amount = |entries: &[Value]| -> Option<(f64, u32)> {
        entries.iter().find(|e| {
            e["mint"].as_str() == Some(mint) && e["owner"].as_str() == Some(curve_str)
        }).and_then(|e| {
            let amount: f64 = e["uiTokenAmount"]["amount"].as_str()?.parse().ok()?;
            let decimals = e["uiTokenAmount"]["decimals"].as_u64()? as u32;
            Some((amount, decimals))
        })
    };

    let (pre_amount, decimals) = find_curve_amount(&pre_tok)?;
    let (post_amount, _) = find_curve_amount(&post_tok)?;

    let token_delta = (post_amount - pre_amount).abs() / 10f64.powi(decimals as i32);
    if token_delta == 0.0 || sol_delta == 0.0 { return None; }

    Some(PricePoint { timestamp: block_time, price_sol: sol_delta / token_delta })
}