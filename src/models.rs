use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TransactionRequest {
    pub id: String,
    pub transaction: TransactionData,
    pub customer: CustomerData,
    pub merchant: MerchantData,
    pub terminal: TerminalData,
    pub last_transaction: Option<LastTransactionData>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TransactionData {
    pub amount: f32,
    pub installments: i32,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CustomerData {
    pub avg_amount: f32,
    pub tx_count_24h: i32,
    pub known_merchants: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MerchantData {
    pub id: String,
    pub mcc: String,
    pub avg_amount: f32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TerminalData {
    pub is_online: bool,
    pub card_present: bool,
    pub km_from_home: f32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct LastTransactionData {
    pub timestamp: DateTime<Utc>,
    pub km_from_current: f32,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct TransactionResponse {
    pub approved: bool,
    pub fraud_score: f32,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct NormalizationConstants {
    pub max_amount: f32,
    pub max_installments: f32,
    pub amount_vs_avg_ratio: f32,
    pub max_minutes: f32,
    pub max_km: f32,
    pub max_tx_count_24h: f32,
    pub max_merchant_avg_amount: f32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ReferenceData {
    pub vector: [f32; 14],
    pub label: String,
}
