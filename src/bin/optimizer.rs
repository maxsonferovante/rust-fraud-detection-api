#[path = "../json.rs"]
mod json;
#[path = "../models.rs"]
mod models;
#[path = "../normalization.rs"]
mod normalization;
#[path = "../search.rs"]
mod search;

use anyhow::{bail, Context};
use models::{NormalizationConstants, TransactionRequest};
use search::VectorStore;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::time::Instant;

const DEFAULT_TEST_DATA: &str = "../rinha-de-backend-2026/test/test-data.json";
const PROBES_TO_EVALUATE: [usize; 10] = [10, 16, 32, 64, 96, 128, 160, 192, 256, 384];
const FRAUD_THRESHOLD: f32 = 0.6;
const EPSILON_MIN: f64 = 0.001;
const BETA: f64 = 300.0;

#[derive(Debug, Deserialize)]
struct TestData<'a> {
    #[serde(borrow)]
    entries: Vec<TestEntry<'a>>,
}

#[derive(Debug, Deserialize)]
struct TestEntry<'a> {
    #[serde(borrow)]
    request: TransactionRequest<'a>,
    expected_approved: bool,
}

#[derive(Debug, Clone)]
struct Metrics {
    n_probes: usize,
    tp: usize,
    tn: usize,
    fp: usize,
    fn_: usize,
    weighted_errors: usize,
    detection_score: f64,
    rate_component: f64,
    absolute_penalty: f64,
    avg_latency_us: f64,
    p99_latency_us: f64,
    score_histogram: [usize; 6],
}

fn main() -> anyhow::Result<()> {
    let test_data_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_TEST_DATA.to_string());

    validate_master_binary_resources()?;

    let test_data_json = std::fs::read_to_string(&test_data_path)
        .with_context(|| format!("failed to read test data at {test_data_path}"))?;
    let test_data: TestData<'_> = serde_json::from_str(&test_data_json)
        .with_context(|| format!("failed to parse test data at {test_data_path}"))?;

    let normalization_constants = load_normalization_constants()?;
    let mcc_table = load_mcc_table()?;
    let vector_store = load_vector_store()?;

    println!(
        "Loaded {} labeled payloads from {}",
        test_data.entries.len(),
        test_data_path
    );
    println!(
        "Threshold fixed at fraud_score >= {:.1} (approved = fraud_score < {:.1})",
        FRAUD_THRESHOLD, FRAUD_THRESHOLD
    );
    println!();
    println!(
        "{:>8} {:>6} {:>6} {:>6} {:>6} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "probes", "TP", "TN", "FP", "FN", "E", "det_score", "rate", "penalty", "avg_us", "p99_us"
    );

    let mut best: Option<Metrics> = None;
    for &n_probes in &PROBES_TO_EVALUATE {
        let metrics = evaluate(
            &vector_store,
            &normalization_constants,
            &mcc_table,
            &test_data.entries,
            n_probes,
        );

        println!(
            "{:>8} {:>6} {:>6} {:>6} {:>6} {:>6} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
            metrics.n_probes,
            metrics.tp,
            metrics.tn,
            metrics.fp,
            metrics.fn_,
            metrics.weighted_errors,
            metrics.detection_score,
            metrics.rate_component,
            metrics.absolute_penalty,
            metrics.avg_latency_us,
            metrics.p99_latency_us,
        );

        if best
            .as_ref()
            .map(|current| is_better(&metrics, current))
            .unwrap_or(true)
        {
            best = Some(metrics);
        }
    }

    if let Some(best) = best {
        println!();
        println!("Best by weighted errors, then p99:");
        println!(
            "N_PROBES={} E={} FP={} FN={} detection_score={:.2} p99_us={:.2}",
            best.n_probes,
            best.weighted_errors,
            best.fp,
            best.fn_,
            best.detection_score,
            best.p99_latency_us,
        );
        println!(
            "fraud_score histogram [0/5..5/5]: {:?}",
            best.score_histogram
        );
    }

    Ok(())
}

fn evaluate(
    vector_store: &VectorStore,
    normalization_constants: &NormalizationConstants,
    mcc_table: &[f32],
    entries: &[TestEntry<'_>],
    n_probes: usize,
) -> Metrics {
    let mut tp = 0usize;
    let mut tn = 0usize;
    let mut fp = 0usize;
    let mut fn_ = 0usize;
    let mut score_histogram = [0usize; 6];
    let mut latencies = Vec::with_capacity(entries.len());

    for entry in entries {
        let vector =
            normalization::normalize_i16(&entry.request, normalization_constants, mcc_table);

        let start = Instant::now();
        let frauds = vector_store.fraud_count_nearest_i16(&vector, n_probes);
        let elapsed = start.elapsed();
        latencies.push(elapsed.as_secs_f64() * 1_000_000.0);

        score_histogram[frauds] += 1;
        let fraud_score = frauds as f32 / 5.0;
        let approved = fraud_score < FRAUD_THRESHOLD;

        match (entry.expected_approved, approved) {
            (false, false) => tp += 1,
            (true, true) => tn += 1,
            (true, false) => fp += 1,
            (false, true) => fn_ += 1,
        }
    }

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg_latency_us = latencies.iter().sum::<f64>() / latencies.len() as f64;
    let p99_idx = ((latencies.len() as f64) * 0.99).ceil() as usize - 1;
    let p99_latency_us = latencies[p99_idx.min(latencies.len() - 1)];
    let weighted_errors = fp + (3 * fn_);
    let (detection_score, rate_component, absolute_penalty) =
        detection_score(weighted_errors, entries.len());

    Metrics {
        n_probes,
        tp,
        tn,
        fp,
        fn_,
        weighted_errors,
        detection_score,
        rate_component,
        absolute_penalty,
        avg_latency_us,
        p99_latency_us,
        score_histogram,
    }
}

fn detection_score(weighted_errors: usize, total: usize) -> (f64, f64, f64) {
    let epsilon = if total == 0 {
        0.0
    } else {
        weighted_errors as f64 / total as f64
    };
    let rate_component = 1000.0 * (1.0 / epsilon.max(EPSILON_MIN)).log10();
    let absolute_penalty = -BETA * (1.0 + weighted_errors as f64).log10();
    (
        rate_component + absolute_penalty,
        rate_component,
        absolute_penalty,
    )
}

fn is_better(candidate: &Metrics, current: &Metrics) -> bool {
    candidate
        .weighted_errors
        .cmp(&current.weighted_errors)
        .then_with(|| candidate.p99_latency_us.total_cmp(&current.p99_latency_us))
        .is_lt()
}

fn load_normalization_constants() -> anyhow::Result<NormalizationConstants> {
    let file = File::open("resources/normalization.json")
        .context("failed to open resources/normalization.json")?;
    serde_json::from_reader(file).context("failed to parse resources/normalization.json")
}

fn load_mcc_table() -> anyhow::Result<Vec<f32>> {
    let file =
        File::open("resources/mcc_risk.json").context("failed to open resources/mcc_risk.json")?;
    let mcc_json: HashMap<String, f32> =
        serde_json::from_reader(file).context("failed to parse resources/mcc_risk.json")?;
    let mut mcc_table = vec![0.5f32; 10_000];
    for (key, val) in &mcc_json {
        if let Ok(idx) = key.parse::<usize>() {
            if idx < mcc_table.len() {
                mcc_table[idx] = *val;
            }
        }
    }
    Ok(mcc_table)
}

fn load_vector_store() -> anyhow::Result<VectorStore> {
    VectorStore::load("resources/specialist.bin")
}

fn validate_master_binary_resources() -> anyhow::Result<()> {
    let index_bytes = std::fs::metadata("resources/specialist.bin")
        .context("failed to stat resources/specialist.bin; regenerate with `cargo run --release --bin preprocessor`")?
        .len();

    if index_bytes < 1024 {
        bail!("resources/specialist.bin is too small to be a valid compact IVF index");
    }
    Ok(())
}
