#[path = "../models.rs"]
mod models;

use std::fs::File;
use std::io::{BufReader, Write};
use flate2::read::GzDecoder;
use serde::de::{Visitor, SeqAccess, Deserializer};
use std::fmt;
use rand::seq::SliceRandom;
use rayon::prelude::*;

const K: usize = 2048; // Increased clusters to reduce search space per probe
const MAX_ITER: usize = 50; // More iterations for better quality with more clusters

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
        while let Some(entry) = seq.next_element::<models::ReferenceData>()? {
            self.vectors.push(RawVector {
                vector: entry.vector,
                is_fraud: entry.label == "fraud",
            });
        }
        Ok(())
    }
}

fn dist_sq(a: &[f32; 14], b: &[f32; 14]) -> f32 {
    let mut sum = 0.0;
    for i in 0..14 {
        let d = a[i] - b[i];
        sum += d * d;
    }
    sum
}

fn main() -> anyhow::Result<()> {
    println!("Loading dataset for IVF clustering...");
    let ref_path = "resources/references.json.gz";
    let file = File::open(ref_path)?;
    let decoder = GzDecoder::new(file);
    let reader = BufReader::new(decoder);
    
    let mut vectors = Vec::with_capacity(3_000_000);
    {
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        deserializer.deserialize_seq(DatasetVisitor { vectors: &mut vectors })?;
    }
    println!("Loaded {} vectors.", vectors.len());

    println!("Initializing centroids (K={})...", K);
    let mut rng = rand::thread_rng();
    let mut centroids: Vec<[f32; 14]> = vectors
        .choose_multiple(&mut rng, K)
        .map(|rv| rv.vector)
        .collect();

    println!("Running K-means ({} iterations)...", MAX_ITER);
    for iter in 0..MAX_ITER {
        let new_centroids_data: Vec<([f32; 14], usize)> = vectors
            .par_iter()
            .fold(
                || vec![([0.0f32; 14], 0usize); K],
                |mut acc, rv| {
                    let mut min_dist = f32::MAX;
                    let mut best_idx = 0;
                    for (i, c) in centroids.iter().enumerate() {
                        let d = dist_sq(&rv.vector, c);
                        if d < min_dist {
                            min_dist = d;
                            best_idx = i;
                        }
                    }
                    for i in 0..14 {
                        acc[best_idx].0[i] += rv.vector[i];
                    }
                    acc[best_idx].1 += 1;
                    acc
                },
            )
            .reduce(
                || vec![([0.0f32; 14], 0usize); K],
                |mut a, b| {
                    for i in 0..K {
                        for j in 0..14 {
                            a[i].0[j] += b[i].0[j];
                        }
                        a[i].1 += b[i].1;
                    }
                    a
                },
            );

        for i in 0..K {
            if new_centroids_data[i].1 > 0 {
                for j in 0..14 {
                    centroids[i][j] = new_centroids_data[i].0[j] / new_centroids_data[i].1 as f32;
                }
            }
        }
        println!("Iteration {} complete", iter + 1);
    }

    println!("Assigning vectors to clusters...");
    let cluster_assignments: Vec<usize> = vectors
        .par_iter()
        .map(|rv| {
            let mut min_dist = f32::MAX;
            let mut best_idx = 0;
            for (i, c) in centroids.iter().enumerate() {
                let d = dist_sq(&rv.vector, c);
                if d < min_dist {
                    min_dist = d;
                    best_idx = i;
                }
            }
            best_idx
        })
        .collect();

    let mut clusters: Vec<Vec<&RawVector>> = vec![Vec::new(); K];
    for (i, &best_idx) in cluster_assignments.iter().enumerate() {
        clusters[best_idx].push(&vectors[i]);
    }

    println!("Writing IVF binary files...");
    let mut centroid_file = File::create("resources/centroids.bin")?;
    for c in &centroids {
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(c.as_ptr() as *const u8, 14 * 4) };
        centroid_file.write_all(bytes)?;
    }

    let mut vector_file = File::create("resources/ivf_vectors.bin")?;
    let mut label_file = File::create("resources/ivf_labels.bin")?;
    let mut offset_file = File::create("resources/ivf_offsets.bin")?;
    
    let mut current_offset = 0u32;
    for cluster in &clusters {
        offset_file.write_all(&current_offset.to_le_bytes())?;
        let cluster_size = cluster.len() as u32;
        offset_file.write_all(&cluster_size.to_le_bytes())?;
        
        for rv in cluster {
            let bytes: &[u8] = unsafe { std::slice::from_raw_parts(rv.vector.as_ptr() as *const u8, 14 * 4) };
            vector_file.write_all(bytes)?;
            
            let label = if rv.is_fraud { 1u8 } else { 0u8 };
            label_file.write_all(&[label])?;
        }
        current_offset += cluster_size;
    }

    println!("Preprocessing complete. K={}, total vectors={}", K, vectors.len());
    Ok(())
}
