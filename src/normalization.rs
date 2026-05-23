use crate::models::{TransactionRequest, NormalizationConstants};

// ---------------------------------------------------------------------------
// Utilitários de parsing ISO 8601 via fatiamento de bytes — sem chrono,
// sem validação complexa, sem alocações.
// Formato esperado: "YYYY-MM-DDTHH:MM:SSZ" ou "YYYY-MM-DDTHH:MM:SS.sssZ"
// ---------------------------------------------------------------------------

/// Extrai 2 dígitos ASCII de um slice de bytes e retorna o valor numérico.
#[inline(always)]
fn parse_u8_2(b: &[u8]) -> u8 {
    (b[0] - b'0') * 10 + (b[1] - b'0')
}

/// Extrai 4 dígitos ASCII de um slice de bytes e retorna o valor numérico.
#[inline(always)]
fn parse_u32_4(b: &[u8]) -> u32 {
    (b[0] - b'0') as u32 * 1000
        + (b[1] - b'0') as u32 * 100
        + (b[2] - b'0') as u32 * 10
        + (b[3] - b'0') as u32
}

/// Hora UTC extraída via offset fixo de bytes (b[11..13]).
/// Custo: 2 subtrações + 1 multiplicação. Zero alocações.
#[inline(always)]
pub fn hour_of_day(ts: &str) -> u8 {
    let b = ts.as_bytes();
    (b[11] - b'0') * 10 + (b[12] - b'0')
}

/// Dia da semana via algoritmo de Tomohiko Sakamoto (1993).
/// Retorna 0 = Segunda, ..., 6 = Domingo (compatível com number_from_monday() - 1).
/// Custo: parsing de 4+2+2 bytes + 6 operações aritméticas. Zero alocações.
pub fn day_of_week(ts: &str) -> u8 {
    let b = ts.as_bytes();
    let mut y = parse_u32_4(&b[0..4]);
    let m = parse_u8_2(&b[5..7]) as u32;
    let d = parse_u8_2(&b[8..10]) as u32;

    // Tabela de Sakamoto: deslocamentos mensais para o algoritmo modular
    const T: [u32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    if m < 3 {
        y -= 1;
    }
    // dow: 0 = Domingo, 1 = Segunda, ..., 6 = Sábado (convenção Sakamoto)
    let dow = (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d) % 7;
    // Converter para 0 = Segunda, ..., 6 = Domingo
    if dow == 0 { 6 } else { (dow - 1) as u8 }
}

/// Converte um timestamp ISO 8601 em minutos totais desde 2000-01-01T00:00Z.
/// Inclui cálculo completo de ano/mês/dia para diferenças entre datas distintas.
/// Custo: parsing de bytes + ~10 operações aritméticas inteiras. Zero alocações.
fn datetime_to_minutes(ts: &str) -> i64 {
    let b = ts.as_bytes();
    let year  = parse_u32_4(&b[0..4]) as i64;
    let month = parse_u8_2(&b[5..7]) as i64;
    let day   = parse_u8_2(&b[8..10]) as i64;
    let hour  = parse_u8_2(&b[11..13]) as i64;
    let min   = parse_u8_2(&b[14..16]) as i64;

    // Dias acumulados até o início de cada mês em ano não-bissexto
    const MONTH_DAYS: [i64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let leap_offset = if is_leap && month > 2 { 1i64 } else { 0i64 };

    // Anos bissextos de 2000 até (exclusive) o ano corrente
    let y = year - 2000;
    let leap_years = if y >= 1 {
        (y - 1) / 4 - (y - 1) / 100 + (y - 1) / 400 + 1 // inclui o ano 2000
    } else {
        0
    };

    let days_since_2000 = y * 365
        + leap_years
        + MONTH_DAYS[(month - 1) as usize]
        + leap_offset
        + day - 1; // -1: dia 1 = 0 dias decorridos

    days_since_2000 * 1440 + hour * 60 + min
}

/// Diferença absoluta em minutos entre dois timestamps ISO 8601.
/// Cálculo completo incluindo cruzamento de dias e meses.
#[inline]
fn minutes_diff(t1: &str, t2: &str) -> f32 {
    (datetime_to_minutes(t1) - datetime_to_minutes(t2)).abs() as f32
}

// ---------------------------------------------------------------------------
// Função principal de normalização
// mcc_table: vetor estático de 10000 posições — MCC como índice numérico → O(1)
// ---------------------------------------------------------------------------

pub fn normalize(
    req: &TransactionRequest<'_>,
    constants: &NormalizationConstants,
    mcc_table: &[f32],
) -> [f32; 14] {
    let mut vector = [0.0f32; 14];

    // 0: amount
    vector[0] = clamp(req.transaction.amount / constants.max_amount);

    // 1: installments
    vector[1] = clamp(req.transaction.installments as f32 / constants.max_installments);

    // 2: amount_vs_avg
    let amount_vs_avg = if req.customer.avg_amount > 0.0 {
        (req.transaction.amount / req.customer.avg_amount) / constants.amount_vs_avg_ratio
    } else {
        1.0
    };
    vector[2] = clamp(amount_vs_avg);

    // 3: hour_of_day — parsing via offset fixo de bytes b[11..13]
    vector[3] = hour_of_day(req.transaction.requested_at) as f32 / 23.0;

    // 4: day_of_week — algoritmo de Sakamoto, sem biblioteca de datas
    vector[4] = day_of_week(req.transaction.requested_at) as f32 / 6.0;

    // 5 & 6: minutos e km desde a última transação
    if let Some(last) = &req.last_transaction {
        let minutes = minutes_diff(req.transaction.requested_at, last.timestamp);
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

    // 11: unknown_merchant — comparação de &str, sem alocação
    let is_known = req.customer.known_merchants.iter().any(|m| *m == req.merchant.id);
    vector[11] = if is_known { 0.0 } else { 1.0 };

    // 12: mcc_risk — acesso O(1) via índice numérico, sem hash
    let mcc_idx = req.merchant.mcc.parse::<usize>().unwrap_or(usize::MAX);
    vector[12] = if mcc_idx < mcc_table.len() {
        mcc_table[mcc_idx]
    } else {
        0.5 // fallback para MCC desconhecido
    };

    // 13: merchant_avg_amount
    vector[13] = clamp(req.merchant.avg_amount / constants.max_merchant_avg_amount);

    vector
}

#[inline(always)]
fn clamp(v: f32) -> f32 {
    if v < 0.0 { 0.0 } else if v > 1.0 { 1.0 } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{TransactionData, CustomerData, MerchantData, TerminalData};

    fn make_mcc_table_with(mcc: usize, risk: f32) -> Vec<f32> {
        let mut table = vec![0.5f32; 10000];
        table[mcc] = risk;
        table
    }

    #[test]
    fn test_normalization_example() {
        let req = TransactionRequest {
            id: "tx-1329056812",
            transaction: TransactionData {
                amount: 41.12,
                installments: 2,
                // 2026-03-11 = Quarta-feira, hora 18
                requested_at: "2026-03-11T18:45:53Z",
            },
            customer: CustomerData {
                avg_amount: 82.24,
                tx_count_24h: 3,
                known_merchants: vec!["MERC-003", "MERC-016"],
            },
            merchant: MerchantData {
                id: "MERC-016",
                mcc: "5411",
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

        let mcc_table = make_mcc_table_with(5411, 0.15);
        let vector = normalize(&req, &constants, &mcc_table);

        let expected = [
            0.004112,    // 0: amount
            0.16666667,  // 1: installments
            0.05,        // 2: amount_vs_avg
            0.7826087,   // 3: hora 18/23
            0.33333334,  // 4: Quarta (2/6)
            -1.0,        // 5: sem last_transaction
            -1.0,        // 6: sem last_transaction
            0.02923,     // 7: km_from_home
            0.15,        // 8: tx_count_24h
            0.0,         // 9: not online
            1.0,         // 10: card present
            0.0,         // 11: known merchant
            0.15,        // 12: mcc_risk
            0.006025,    // 13: merchant_avg_amount
        ];

        for i in 0..14 {
            assert!(
                (vector[i] - expected[i]).abs() < 1e-4,
                "Dimension {} failed: expected {}, got {}",
                i, expected[i], vector[i]
            );
        }
    }

    #[test]
    fn test_clamp() {
        assert_eq!(clamp(-1.0), 0.0);
        assert_eq!(clamp(0.5), 0.5);
        assert_eq!(clamp(2.0), 1.0);
    }

    #[test]
    fn test_normalization_edge_cases() {
        let req = TransactionRequest {
            id: "tx-edge",
            transaction: TransactionData {
                amount: 100.0,
                installments: 1,
                requested_at: "2026-03-11T12:00:00Z",
            },
            customer: CustomerData {
                avg_amount: 0.0,
                tx_count_24h: 1,
                known_merchants: vec![],
            },
            merchant: MerchantData {
                id: "MERC-NEW",
                mcc: "9999",
                avg_amount: 50.0,
            },
            terminal: TerminalData {
                is_online: true,
                card_present: false,
                km_from_home: 50.0,
            },
            last_transaction: None,
        };

        let constants = NormalizationConstants {
            max_amount: 1000.0,
            max_installments: 10.0,
            amount_vs_avg_ratio: 5.0,
            max_minutes: 100.0,
            max_km: 100.0,
            max_tx_count_24h: 10.0,
            max_merchant_avg_amount: 100.0,
        };

        let mcc_table = vec![0.5f32; 10000];
        let vector = normalize(&req, &constants, &mcc_table);

        assert_eq!(vector[2], 1.0);   // amount_vs_avg com avg=0 cai para 1.0
        assert_eq!(vector[5], -1.0);  // sem last_transaction
        assert_eq!(vector[6], -1.0);  // sem last_transaction
        assert_eq!(vector[9], 1.0);   // is_online = true
        assert_eq!(vector[10], 0.0);  // card_present = false
        assert_eq!(vector[11], 1.0);  // merchant desconhecido
        assert_eq!(vector[12], 0.5);  // MCC 9999 → fallback 0.5
    }

    #[test]
    fn test_hour_of_day_parsing() {
        assert_eq!(hour_of_day("2026-03-11T00:00:00Z"), 0);
        assert_eq!(hour_of_day("2026-03-11T18:45:53Z"), 18);
        assert_eq!(hour_of_day("2026-03-11T23:59:59Z"), 23);
    }

    #[test]
    fn test_day_of_week_sakamoto() {
        // 2026-03-11 = Quarta-feira → índice 2 (0=Seg)
        assert_eq!(day_of_week("2026-03-11T18:45:53Z"), 2);
        // 2026-01-05 = Segunda-feira → índice 0
        assert_eq!(day_of_week("2026-01-05T00:00:00Z"), 0);
        // 2026-01-11 = Domingo → índice 6
        assert_eq!(day_of_week("2026-01-11T00:00:00Z"), 6);
    }

    #[test]
    fn test_minutes_diff_cross_day() {
        // 2026-03-12T01:00Z → 2026-03-11T23:00Z = 120 minutos
        let diff = minutes_diff("2026-03-12T01:00:00Z", "2026-03-11T23:00:00Z");
        assert!((diff - 120.0).abs() < 0.01, "Expected 120 min, got {}", diff);
    }

    #[test]
    fn test_minutes_diff_same_day() {
        // 2026-03-11T18:45Z → 2026-03-11T16:30Z = 135 minutos
        let diff = minutes_diff("2026-03-11T18:45:00Z", "2026-03-11T16:30:00Z");
        assert!((diff - 135.0).abs() < 0.01, "Expected 135 min, got {}", diff);
    }
}
