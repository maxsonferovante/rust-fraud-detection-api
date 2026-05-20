use crate::models::{TransactionRequest, NormalizationConstants};
use std::collections::HashMap;
use chrono::Datelike;
use chrono::Timelike;

pub fn normalize(
    req: &TransactionRequest,
    constants: &NormalizationConstants,
    mcc_risks: &HashMap<String, f32>,
) -> [f32; 14] {
    let mut vector = [0.0; 14];

    // 0: amount
    vector[0] = clamp(req.transaction.amount / constants.max_amount);

    // 1: installments
    vector[1] = clamp(req.transaction.installments as f32 / constants.max_installments);

    // 2: amount_vs_avg
    let amount_vs_avg = if req.customer.avg_amount > 0.0 {
        (req.transaction.amount / req.customer.avg_amount) / constants.amount_vs_avg_ratio
    } else {
        1.0 // If avg is 0, any amount is "infinite" ratio, capped at 1.0
    };
    vector[2] = clamp(amount_vs_avg);

    // 3: hour_of_day (UTC)
    vector[3] = req.transaction.requested_at.hour() as f32 / 23.0;

    // 4: day_of_week (seg=0, dom=6)
    vector[4] = (req.transaction.requested_at.weekday().number_from_monday() - 1) as f32 / 6.0;

    // 5 & 6: minutes and km since last transaction
    if let Some(last) = &req.last_transaction {
        let duration = req.transaction.requested_at.signed_duration_since(last.timestamp);
        let minutes = duration.num_minutes().abs() as f32;
        vector[5] = clamp(minutes / constants.max_minutes);
        vector[6] = clamp(last.km_from_current / constants.max_km);
    } else {
        vector[5] = -1.0;
        vector[6] = -1.0;
    }

    // 7: km_from_home
    vector[7] = clamp(req.terminal.km_from_home / constants.max_km);

    // 8: tx_count_24h
    vector[8] = clamp(req.customer.tx_count_24h as f32 / constants.max_tx_count_24h);

    // 9: is_online
    vector[9] = if req.terminal.is_online { 1.0 } else { 0.0 };

    // 10: card_present
    vector[10] = if req.terminal.card_present { 1.0 } else { 0.0 };

    // 11: unknown_merchant
    let is_known = req.customer.known_merchants.iter().any(|m| m == &req.merchant.id);
    vector[11] = if is_known { 0.0 } else { 1.0 };

    // 12: mcc_risk
    vector[12] = *mcc_risks.get(&req.merchant.mcc).unwrap_or(&0.5);

    // 13: merchant_avg_amount
    vector[13] = clamp(req.merchant.avg_amount / constants.max_merchant_avg_amount);

    vector
}

fn clamp(v: f32) -> f32 {
    if v < 0.0 {
        0.0
    } else if v > 1.0 {
        1.0
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{TransactionData, CustomerData, MerchantData, TerminalData};
    use chrono::TimeZone;
    use chrono::Utc;

    #[test]
    fn test_normalization_example() {
        let req = TransactionRequest {
            id: "tx-1329056812".to_string(),
            transaction: TransactionData {
                amount: 41.12,
                installments: 2,
                requested_at: Utc.with_ymd_and_hms(2026, 3, 11, 18, 45, 53).unwrap(),
            },
            customer: CustomerData {
                avg_amount: 82.24,
                tx_count_24h: 3,
                known_merchants: vec!["MERC-003".to_string(), "MERC-016".to_string()],
            },
            merchant: MerchantData {
                id: "MERC-016".to_string(),
                mcc: "5411".to_string(),
                avg_amount: 60.25,
            },
            terminal: TerminalData {
                is_online: false,
                card_present: true,
                km_from_home: 29.23,
            },
            last_transaction: None,
        };

        let constants = NormalizationConstants {
            max_amount: 10000.0,
            max_installments: 12.0,
            amount_vs_avg_ratio: 10.0,
            max_minutes: 1440.0,
            max_km: 1000.0,
            max_tx_count_24h: 20.0,
            max_merchant_avg_amount: 10000.0,
        };

        let mut mcc_risks = HashMap::new();
        mcc_risks.insert("5411".to_string(), 0.15);

        let vector = normalize(&req, &constants, &mcc_risks);
        let expected = [0.004112, 0.16666667, 0.05, 0.7826087, 0.33333334, -1.0, -1.0, 0.02923, 0.15, 0.0, 1.0, 0.0, 0.15, 0.006025];
        
        for i in 0..14 {
            assert!((vector[i] - expected[i]).abs() < 1e-4, "Dimension {} failed: expected {}, got {}", i, expected[i], vector[i]);
        }
    }
}
