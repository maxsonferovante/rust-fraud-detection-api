mod models;
mod normalization;
mod search;

use axum::{
    routing::{get, post},
    Json, Router,
};
use axum::extract::State;
use axum::response::IntoResponse;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;
use std::fs::File;
use std::io::Read;

use crate::models::{TransactionRequest, TransactionResponse, NormalizationConstants};
use crate::search::VectorStore;

struct AppState {
    vector_store: VectorStore,
    normalization_constants: NormalizationConstants,
    mcc_risks: HashMap<String, f32>,
    n_probes: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load n_probes from environment
    let n_probes = std::env::var("N_PROBES")
        .unwrap_or_else(|_| "32".to_string())
        .parse::<usize>()
        .unwrap_or(32);

    // Load normalization constants
    let norm_file = File::open("resources/normalization.json")?;
    let normalization_constants: NormalizationConstants = serde_json::from_reader(norm_file)?;

    // Load MCC risks
    let mcc_file = File::open("resources/mcc_risk.json")?;
    let mcc_risks: HashMap<String, f32> = serde_json::from_reader(mcc_file)?;

    // Load IVF binary data using memory mapping (mmap)
    let centroid_file = File::open("resources/centroids.bin")?;
    let vec_file = File::open("resources/ivf_vectors.bin")?;
    let label_file = File::open("resources/ivf_labels.bin")?;
    let offset_file = File::open("resources/ivf_offsets.bin")?;

    let centroid_mmap = unsafe { memmap2::MmapOptions::new().map(&centroid_file)? };
    let vec_mmap = unsafe { memmap2::MmapOptions::new().map(&vec_file)? };
    let label_mmap = unsafe { memmap2::MmapOptions::new().map(&label_file)? };
    let offset_mmap = unsafe { memmap2::MmapOptions::new().map(&offset_file)? };

    let vector_store = VectorStore::from_mmaps(centroid_mmap, vec_mmap, label_mmap, offset_mmap);

    let state = Arc::new(AppState {
        vector_store,
        normalization_constants,
        mcc_risks,
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
    axum::http::StatusCode::OK
}

async fn fraud_score(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TransactionRequest>,
) -> Json<TransactionResponse> {
    let vector = normalization::normalize(
        &payload,
        &state.normalization_constants,
        &state.mcc_risks,
    );
    
    let nearest_labels = state.vector_store.find_k_nearest(&vector, 5, state.n_probes);
    let frauds = nearest_labels.iter().filter(|&&f| f).count();
    let fraud_score = frauds as f32 / 5.0;
    
    Json(TransactionResponse {
        approved: fraud_score < 0.6,
        fraud_score,
    })
}
