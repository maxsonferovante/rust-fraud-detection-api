#[path = "../models.rs"]
mod models;
#[path = "../normalization.rs"]
mod normalization;
#[path = "../search.rs"]
mod search;

use argmin::core::{CostFunction, Error, Executor, observers::ObserverMode, State};
use argmin_observer_slog::SlogLogger;
use argmin::solver::neldermead::NelderMead;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Instant;
use flate2::read::GzDecoder;
use serde::de::{Visitor, SeqAccess, Deserializer};
use std::fmt;

use crate::models::ReferenceData;
use crate::search::VectorStore;

const VALIDATION_SIZE: usize = 5000;

struct RawVector {
    vector: [f32; 14],
    is_fraud: bool,
}

struct DatasetVisitor<'a> {
    vectors: &'a mut Vec<RawVector>,
}

impl<'de, 'a> Visitor<'de> for DatasetVisitor<'a> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of ReferenceData")
    }
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where A: SeqAccess<'de> {
        while let Some(entry) = seq.next_element::<ReferenceData>()? {
            if self.vectors.len() >= VALIDATION_SIZE { break; }
            self.vectors.push(RawVector {
                vector: entry.vector,
                is_fraud: entry.label == "fraud",
            });
        }
        Ok(())
    }
}

struct SearchOptimizer {
    vector_store: Arc<VectorStore>,
    validation_set: Vec<RawVector>,
}

impl CostFunction for SearchOptimizer {
    type Param = Vec<f32>; // [n_probes, threshold]
    type Output = f32;

    fn cost(&self, p: &Self::Param) -> Result<Self::Output, Error> {
        let n_probes = p[0].round().max(1.0) as usize;
        let threshold = p[1].max(0.0).min(1.0);
        let k = 5;

        let start = Instant::now();
        let mut correct = 0;
        
        for item in &self.validation_set {
            let nearest = self.vector_store.find_k_nearest(&item.vector, k, n_probes);
            let frauds = nearest.iter().filter(|&&f| f).count();
            let is_fraud_predicted = (frauds as f32 / k as f32) >= threshold;
            
            if is_fraud_predicted == item.is_fraud {
                correct += 1;
            }
        }
        
        let duration = start.elapsed();
        let avg_latency_ms = (duration.as_secs_f32() * 1000.0) / self.validation_set.len() as f32;
        let accuracy = correct as f32 / self.validation_set.len() as f32;
        
        let weight_latencia = 100.0;
        let weight_accuracy = 100000.0;
        
        let score = (avg_latency_ms * weight_latencia) + (1.0 - accuracy) * weight_accuracy;
        
        println!("n_probes: {}, threshold: {:.2}, avg_lat: {:.4}ms, acc: {:.4}, score: {:.4}", 
            n_probes, threshold, avg_latency_ms, accuracy, score);
        
        Ok(score)
    }
}

fn main() -> anyhow::Result<()> {
    println!("Loading validation dataset...");
    let ref_path = "resources/references.json.gz";
    let file = File::open(ref_path)?;
    let decoder = GzDecoder::new(file);
    let reader = BufReader::new(decoder);
    
    let mut validation_set = Vec::with_capacity(VALIDATION_SIZE);
    {
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        let _ = deserializer.deserialize_seq(DatasetVisitor { vectors: &mut validation_set });
        // Ignore "trailing characters" error which happens when we stop early
    }
    println!("Loaded {} validation vectors.", validation_set.len());

    let centroid_data = std::fs::read("resources/centroids.bin")?;
    let vec_data = std::fs::read("resources/ivf_vectors.bin")?;
    let label_data = std::fs::read("resources/ivf_labels.bin")?;
    let offset_data = std::fs::read("resources/ivf_offsets.bin")?;

    let vector_store = Arc::new(VectorStore::from_bytes(centroid_data, vec_data, label_data, offset_data));

    let cost = SearchOptimizer {
        vector_store,
        validation_set,
    };

    // Initial guesses for [n_probes, threshold]
    let params = vec![
        vec![32.0, 0.6],
        vec![64.0, 0.5],
        vec![16.0, 0.7],
    ];

    println!("Starting optimization...");
    let solver: NelderMead<Vec<f32>, f32> = NelderMead::new(params);

    let res = Executor::new(cost, solver)
        .configure(|state| state.max_iters(25))
        .add_observer(SlogLogger::term(), ObserverMode::Always)
        .run()?;

    let best_param = res.state().get_best_param().unwrap();
    let best_n_probes = best_param[0].round().max(1.0);
    let best_threshold = best_param[1].max(0.0).min(1.0);
    println!("======================================");
    println!("Optimization Complete!");
    println!("Best n_probes: {}", best_n_probes);
    println!("Best threshold: {:.4}", best_threshold);
    println!("Final Score: {}", res.state().get_best_cost());
    println!("======================================");

    Ok(())
}
