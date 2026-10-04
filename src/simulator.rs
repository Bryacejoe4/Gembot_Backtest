// src/simulator.rs
//
// Applies a scale-out exit strategy to a reconstructed price path and produces the resulting simulated outcome for a single call.

use crate::models::{ExitLeg, Outcome, PricePoint, SimResult};

/// Default scale-out strategy: 30% of the position exits at +25%, a further 30% at +60%, and the remaining 40% is carried as a moonbag, valued at the last available price in the series.
pub fn default_exit_legs() -> Vec<ExitLeg> {
    vec![
        ExitLeg { trigger_multiplier: 1.25, sell_fraction: 0.30 },
        ExitLeg { trigger_multiplier: 1.60, sell_fraction: 0.30 },
    ]
}

/// Slippage applied per leg, expressed as basis points against the fill price. 
/// Reflects realistic execution on thin, early-stage liquidity rather than idealised fill prices.
const ENTRY_SLIPPAGE_BPS: f64 = 300.0;
const EXIT_SLIPPAGE_BPS:  f64 = 300.0;

/// Approximate priority fee per transaction, in SOL. Applied once on entry and once per exit leg to reflect real network cost.
const NETWORK_FEE_LAMPORTS: f64 = 0.00001;

/// Simulates one call through the given exit strategy against its reconstructed price path. `entry_sol` is the fixed position size used for every call, so results
/// across calls are directly comparable rather than skewed by varying position sizes. Returns `Outcome::NoData` if the price path is empty, which happens when no on-chain trades could be found after the call.
pub fn simulate_call(
    mint: &str,
    tier: &str,
    price_path: &[PricePoint],
    entry_sol: f64,
    exit_legs: &[ExitLeg],
) -> SimResult {
    // Guard clause: no reconstructed price data means we cannot simulate this call at all. Reported separately from a loss, since it reflects a data gap rather than a real trading outcome.
    let Some(first) = price_path.first() else {
        return no_data_result(mint, tier);
    };

    // Entry price and timestamp are taken from the first reconstructed price point, with buy-side slippage applied to reflect a realistic
    // fill rather than the idealised on-chain price.
    let entry_price = apply_slippage(first.price_sol, ENTRY_SLIPPAGE_BPS, true);
    let entry_timestamp = first.timestamp;

    // Token quantity purchased at entry, net of the entry network fee.
    let tokens_bought = (entry_sol - NETWORK_FEE_LAMPORTS) / entry_price;

    // One flag per exit leg, indexed directly rather than inferred from position or count. This is what guarantees a leg fires at most once,
    // regardless of the order price points arrive in or how many legs are configured.
    let mut fired = vec![false; exit_legs.len()];
    let mut legs_filled: Vec<(f64, f64)> = Vec::new();
    let mut sol_returned = 0.0_f64;

    // Walk the price path chronologically, checking every unfired leg at every point. This correctly handles a price path that jumps past
    // multiple trigger levels between two consecutive observations, both legs fire in the same iteration instead of one being skipped.
    for point in price_path.iter().skip(1) {
        let multiplier = point.price_sol / first.price_sol;

        for (i, leg) in exit_legs.iter().enumerate() {
            if fired[i] { continue; }
            if multiplier < leg.trigger_multiplier { continue; }

            let sell_price = apply_slippage(point.price_sol, EXIT_SLIPPAGE_BPS, false);
            let tokens_this_leg = tokens_bought * leg.sell_fraction;
            let sol_this_leg = (tokens_this_leg * sell_price) - NETWORK_FEE_LAMPORTS;

            legs_filled.push((multiplier, sol_this_leg));
            sol_returned += sol_this_leg;
            fired[i] = true;
        }
    }

    // Remaining fraction is computed directly from which legs never fired, plus whatever fraction was never assigned to any leg in the first place (the moonbag portion of the strategy, e.g. 40% in the default
    // two-leg configuration). Valued at the last observed price rather than assumed worthless.
    let remaining_fraction: f64 = exit_legs.iter()
        .zip(fired.iter())
        .filter(|(_, &f)| !f)
        .map(|(leg, _)| leg.sell_fraction)
        .sum::<f64>()
        + (1.0 - exit_legs.iter().map(|l| l.sell_fraction).sum::<f64>());

    let last_price = price_path.last().unwrap().price_sol;
    let moonbag_tokens = tokens_bought * remaining_fraction;
    let moonbag_value = moonbag_tokens * apply_slippage(last_price, EXIT_SLIPPAGE_BPS, false);

    let total_returned = sol_returned + moonbag_value;
    let total_pnl_sol = total_returned - entry_sol;
    let total_pnl_pct = (total_returned / entry_sol - 1.0) * 100.0;

    SimResult {
        mint: mint.to_string(),
        tier: tier.to_string(),
        entry_price,
        entry_timestamp,
        legs_filled,
        moonbag_value,
        total_pnl_pct,
        total_pnl_sol,
        outcome: if total_pnl_sol > 0.0 { Outcome::Win } else { Outcome::Loss },
    }
}

/// Applies slippage against a raw on-chain price. Buys fill slightly worse (higher) than quoted; sells fill slightly worse (lower) than quoted, matching how slippage actually erodes execution in both directions.
fn apply_slippage(price: f64, bps: f64, is_buy: bool) -> f64 {
    let factor = bps / 10_000.0;
    if is_buy { price * (1.0 + factor) } else { price * (1.0 - factor) }
}

/// Constructs a zeroed result for a call with no reconstructable price data, kept separate from `Outcome::Loss` so it can be reported and excluded from win-rate math distinctly.
fn no_data_result(mint: &str, tier: &str) -> SimResult {
    SimResult {
        mint: mint.to_string(),
        tier: tier.to_string(),
        entry_price: 0.0,
        entry_timestamp: 0,
        legs_filled: Vec::new(),
        moonbag_value: 0.0,
        total_pnl_pct: 0.0,
        total_pnl_sol: 0.0,
        outcome: Outcome::NoData,
    }
}