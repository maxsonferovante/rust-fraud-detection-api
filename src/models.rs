use serde::{Deserialize, Serialize};

/// Zero-copy deserialization: todos os campos de string são referências
/// ao buffer de bytes da requisição HTTP (&'a str com #[serde(borrow)]).
/// Nenhuma alocação na Heap ocorre para strings durante o parsing.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TransactionRequest<'a> {
    #[serde(borrow)]
    pub id: &'a str,
    pub transaction: TransactionData<'a>,
    pub customer: CustomerData<'a>,
    pub merchant: MerchantData<'a>,
    pub terminal: TerminalData,
    pub last_transaction: Option<LastTransactionData<'a>>,
}

/// requested_at é mantido como &str ISO 8601 bruto.
/// O parsing de hora/dia-da-semana é feito via fatiamento de bytes em normalization.rs.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TransactionData<'a> {
    pub amount: f32,
    pub installments: i32,
    #[serde(borrow)]
    pub requested_at: &'a str,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CustomerData<'a> {
    pub avg_amount: f32,
    pub tx_count_24h: i32,
    #[serde(borrow)]
    pub known_merchants: Vec<&'a str>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct MerchantData<'a> {
    #[serde(borrow)]
    pub id: &'a str,
    #[serde(borrow)]
    pub mcc: &'a str,
    pub avg_amount: f32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct TerminalData {
    pub is_online: bool,
    pub card_present: bool,
    pub km_from_home: f32,
}

/// timestamp mantido como &str ISO 8601 bruto para parsing via slice de bytes.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct LastTransactionData<'a> {
    #[serde(borrow)]
    pub timestamp: &'a str,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_response_serialization() {
        let resp = TransactionResponse {
            approved: true,
            fraud_score: 0.15,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"approved":true,"fraud_score":0.15}"#);
    }
}
