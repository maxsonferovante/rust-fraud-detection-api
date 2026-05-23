mod models;
mod normalization;
mod search;

use axum::{
    body::Bytes,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use axum::extract::State;
use axum::response::IntoResponse;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use std::fs::File;

use crate::models::{TransactionRequest, TransactionResponse, NormalizationConstants};
use crate::search::VectorStore;

struct AppState {
    vector_store: VectorStore,
    normalization_constants: NormalizationConstants,
    /// Tabela O(1): mcc_table[mcc_code as usize] = risco (0.0..1.0).
    /// Elimina completamente o custo de hash lookup na rota crítica.
    mcc_table: Vec<f32>,
    n_probes: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Carrega n_probes do ambiente
    let n_probes = std::env::var("N_PROBES")
        .unwrap_or_else(|_| "32".to_string())
        .parse::<usize>()
        .unwrap_or(32);

    // Carrega constantes de normalização
    let norm_file = File::open("resources/normalization.json")?;
    let normalization_constants: NormalizationConstants = serde_json::from_reader(norm_file)?;

    // Constrói tabela MCC O(1): Vec<f32> com 10_000 posições.
    // MCC é sempre 4 dígitos numéricos (0000–9999); parse direto → índice.
    let mcc_file = File::open("resources/mcc_risk.json")?;
    let mcc_json: HashMap<String, f32> = serde_json::from_reader(mcc_file)?;
    let mut mcc_table = vec![0.5f32; 10_000]; // 0.5 = risco padrão para MCC desconhecido
    for (key, val) in &mcc_json {
        if let Ok(idx) = key.parse::<usize>() {
            if idx < 10_000 {
                mcc_table[idx] = *val;
            }
        }
    }

    // Carrega índice IVF completamente na memória RAM
    let centroid_data = std::fs::read("resources/centroids.bin")?;
    let vec_data = std::fs::read("resources/ivf_vectors.bin")?;
    let label_data = std::fs::read("resources/ivf_labels.bin")?;
    let offset_data = std::fs::read("resources/ivf_offsets.bin")?;

    let vector_store = VectorStore::from_bytes(centroid_data, vec_data, label_data, offset_data);

    let state = Arc::new(AppState {
        vector_store,
        normalization_constants,
        mcc_table,
        n_probes,
    });

    let app = Router::new()
        .route("/ready", get(ready))
        .route("/fraud-score", post(fraud_score))
        .with_state(state);

    let listener = TcpListener::bind("0.0.0.0:9999").await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn ready() -> impl IntoResponse {
    StatusCode::OK
}

/// Tarefa 3.1: handler recebe Bytes brutos (buffer da rede) e deserializa via
/// serde_json::from_slice — zero-copy, referências apontam diretamente para `body`.
/// Nenhuma String é alocada para campos de texto durante a requisição.
async fn fraud_score(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> impl IntoResponse {
    let payload: TransactionRequest<'_> = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
        }
    };

    let vector = normalization::normalize(
        &payload,
        &state.normalization_constants,
        &state.mcc_table,
    );

    let nearest_labels = state.vector_store.find_k_nearest(&vector, 5, state.n_probes);
    let frauds = nearest_labels.iter().filter(|&&f| f).count();
    let fraud_score = frauds as f32 / 5.0;

    Json(TransactionResponse {
        approved: fraud_score < 0.6,
        fraud_score,
    })
    .into_response()
}
