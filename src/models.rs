// src/models.rs
//
// Core data structures for the Gembot call backtest.

use serde::{Deserialize, Deserializer};

/// A single row from the exported Gembot call history.
/// Field names map directly to Gembot's real export headers (mint, feed, called_at, call_market_cap), confirmed against an actual export from Gembot's team.
#[derive(Debug, Clone, Deserialize)]
pub struct CallRecord {
    #[serde(rename = "mint")]
    pub mint_address: String,
    #[serde(rename = "feed")]
    pub tier: String,
    #[serde(rename = "called_at", deserialize_with = "deserialize_timestamp")]
    pub call_timestamp: i64,
    #[serde(rename = "call_market_cap")]
    pub market_cap_at_call: f64,
    /// Not present in Gembot's export; defaults to 0.0 when the column is missing entirely. Never used in the price simulation itself, only carried as optional context.
    #[serde(default)]
    pub volume_at_call: f64,
}

/// Accepts a Unix timestamp, a plain date string, or an ISO 8601 timestamp with variable-precision fractional seconds and a
/// trailing Z (Gembot's actual format, e.g. "2026-09-02T20:16:30.212112Z" or "2026-09-02T01:07:49.68403Z", digit count varies row to row).
fn deserialize_timestamp<'de, D>(deserializer: D) -> Result<i64, D::Error>
where D: Deserializer<'de> {
    let raw = String::deserialize(deserializer)?;
    let trimmed = raw.trim();

    if let Ok(unix) = trimmed.parse::<i64>() {
        return Ok(unix);
    }

    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.fZ",   // Gembot's real format, variable fractional seconds
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M:%SZ",
    ] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Ok(dt.and_utc().timestamp());
        }
    }

    Err(serde::de::Error::custom(format!(
        "call_timestamp '{trimmed}' is not a recognised Unix timestamp or date format"
    )))
}

/// A single on-chain price observation for a mint at a given time.
#[derive(Debug, Clone)]
pub struct PricePoint {
    pub timestamp: i64,
    pub price_sol: f64,
}

/// One tranche of a scale-out exit strategy.
#[derive(Debug, Clone, Copy)]
pub struct ExitLeg {
    pub trigger_multiplier: f64,
    pub sell_fraction: f64,
}

/// Outcome of simulating a single call through the exit strategy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Outcome {
    Win,
    Loss,
    NoData,
}

/// Full simulation result for one call.
#[derive(Debug, Clone)]
pub struct SimResult {
    pub mint: String,
    pub tier: String,
    pub entry_price: f64,
    pub entry_timestamp: i64,
    pub legs_filled: Vec<(f64, f64)>,
    pub moonbag_value: f64,
    pub total_pnl_pct: f64,
    pub total_pnl_sol: f64,
    pub outcome: Outcome,
}