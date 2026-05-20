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
use half::f16;

use crate::models::{TransactionRequest, TransactionResponse, NormalizationConstants};
use crate::search::VectorStore;

struct AppState {
    vector_store: VectorStore,
    normalization_constants: NormalizationConstants,
    mcc_risks: HashMap<String, f32>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load normalization constants
    let norm_file = File::open("resources/normalization.json")?;
    let normalization_constants: NormalizationConstants = serde_json::from_reader(norm_file)?;

    // Load MCC risks
    let mcc_file = File::open("resources/mcc_risk.json")?;
    let mcc_risks: HashMap<String, f32> = serde_json::from_reader(mcc_file)?;

    // Load IVF binary data
    let mut centroid_file = File::open("resources/centroids.bin")?;
    let mut vec_file = File::open("resources/ivf_vectors.bin")?;
    let mut label_file = File::open("resources/ivf_labels.bin")?;
    let mut offset_file = File::open("resources/ivf_offsets.bin")?;

    let c_len = centroid_file.metadata()?.len() as usize;
    let v_len = vec_file.metadata()?.len() as usize;
    let l_len = label_file.metadata()?.len() as usize;
    let o_len = offset_file.metadata()?.len() as usize;

    let mut centroids = vec![[0.0f32; 14]; c_len / (14 * 4)];
    let mut vectors = vec![f16::from_f32(0.0); v_len / 2];
    let mut labels = vec![0u8; l_len];
    let mut offsets = vec![(0u32, 0u32); o_len / 8];

    unsafe {
        let c_slice = std::slice::from_raw_parts_mut(centroids.as_mut_ptr() as *mut u8, c_len);
        centroid_file.read_exact(c_slice)?;

        let v_slice = std::slice::from_raw_parts_mut(vectors.as_mut_ptr() as *mut u8, v_len);
        vec_file.read_exact(v_slice)?;

        let l_slice = std::slice::from_raw_parts_mut(labels.as_mut_ptr() as *mut u8, l_len);
        label_file.read_exact(l_slice)?;

        let o_slice = std::slice::from_raw_parts_mut(offsets.as_mut_ptr() as *mut u8, o_len);
        offset_file.read_exact(o_slice)?;
    }

    let vector_store = VectorStore::from_binary(centroids, vectors, labels, offsets);

    let state = Arc::new(AppState {
        vector_store,
        normalization_constants,
        mcc_risks,
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
    
    let nearest_labels = state.vector_store.find_k_nearest(&vector, 5);
    let frauds = nearest_labels.iter().filter(|&&f| f).count();
    let fraud_score = frauds as f32 / 5.0;
    
    Json(TransactionResponse {
        approved: fraud_score < 0.6,
        fraud_score,
    })
}
