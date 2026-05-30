#[path = "../models.rs"]
mod models;

use flate2::read::GzDecoder;
use rand::seq::SliceRandom;
use rayon::prelude::*;
use serde::de::{Deserializer, SeqAccess, Visitor};
use std::fmt;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};

const K: usize = 4096; // Número de clusters IVF
const DIM: usize = 14;
const SCALE: f32 = 10_000.0;
const INDEX_MAGIC: u32 = u32::from_le_bytes(*b"RIVF");
const INDEX_VERSION: u32 = 1;
const MAX_ITER: usize = 50; // Máximo de iterações K-means
                            // Threshold de convergência: se o deslocamento médio dos centroids for menor
                            // que isso, encerra cedo sem precisar de todas as MAX_ITER iterações.
const CONVERGENCE_EPS: f32 = 1e-6;

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
    where
        A: SeqAccess<'de>,
    {
        while let Some(entry) = seq.next_element::<models::ReferenceData>()? {
            self.vectors.push(RawVector {
                vector: entry.vector,
                is_fraud: entry.label == "fraud",
            });
        }
        Ok(())
    }
}

/// Distância euclidiana ao quadrado — loop fixo de 14 elementos.
/// Com RUSTFLAGS="-C target-cpu=native", o compilador auto-vetoriza
/// usando YMM (AVX2) ou NEON dependendo da arquitetura de build.
#[inline(always)]
fn dist_sq(a: &[f32; DIM], b: &[f32; DIM]) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..DIM {
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
        deserializer.deserialize_seq(DatasetVisitor {
            vectors: &mut vectors,
        })?;
    }
    println!("Loaded {} vectors.", vectors.len());

    // -----------------------------------------------------------------------
    // Inicialização aleatória dos centroids.
    //
    // Nota: K-means++ seria ideal para qualidade, mas com K=2048 e N=3M
    // o custo de seleção é O(N × K²/2) ≈ 6×10¹² ops — proibitivo mesmo
    // em paralelo. A inicialização aleatória com dataset grande (3M amostras)
    // fornece diversidade suficiente para bons clusters.
    // -----------------------------------------------------------------------
    println!("Initializing centroids (K={}, random sampling)...", K);
    let mut rng = rand::thread_rng();
    let mut centroids: Vec<[f32; 14]> = vectors
        .choose_multiple(&mut rng, K)
        .map(|rv| rv.vector)
        .collect();

    // -----------------------------------------------------------------------
    // K-means com early stopping por convergência.
    // O loop interno (fold/reduce) já usa par_iter() via rayon.
    // Com RUSTFLAGS=native, dist_sq é auto-vetorizado → ganho direto aqui.
    // -----------------------------------------------------------------------
    println!(
        "Running K-means (max {} iterations, eps={})...",
        MAX_ITER, CONVERGENCE_EPS
    );
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
                        for (j, slot) in a[i].0.iter_mut().enumerate() {
                            *slot += b[i].0[j];
                        }
                        a[i].1 += b[i].1;
                    }
                    a
                },
            );

        // Atualiza centroids e mede deslocamento máximo para early stopping
        let mut max_shift_sq = 0.0f32;
        for i in 0..K {
            if new_centroids_data[i].1 > 0 {
                let mut new_c = [0.0f32; 14];
                for (j, slot) in new_c.iter_mut().enumerate() {
                    *slot = new_centroids_data[i].0[j] / new_centroids_data[i].1 as f32;
                }
                let shift = dist_sq(&new_c, &centroids[i]);
                if shift > max_shift_sq {
                    max_shift_sq = shift;
                }
                centroids[i] = new_c;
            }
        }

        println!(
            "Iteration {} complete (max_shift²={:.2e})",
            iter + 1,
            max_shift_sq
        );

        // Early stopping: centroids estabilizaram, iterações extras não mudam resultado
        if max_shift_sq < CONVERGENCE_EPS {
            println!("Converged at iteration {}! Stopping early.", iter + 1);
            break;
        }
    }

    // -----------------------------------------------------------------------
    // Atribuição final dos vetores aos clusters — par_iter já presente.
    // -----------------------------------------------------------------------
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

    // -----------------------------------------------------------------------
    // Escrita do índice compacto:
    //   header + centroids f32 + sizes/offsets + panel_offsets + vetores i16.
    //
    // Em cada cluster, painéis completos de 8 vetores são gravados em SoA
    // (dimensão → 8 lanes), e a cauda fica AoS. Esse layout é o gancho para
    // AVX2: cada dimensão de um painel vira um load de 8 i16.
    // -----------------------------------------------------------------------
    println!("Pre-serializing int16 SoA cluster buffers in parallel...");

    let cluster_buffers: Vec<(Vec<u8>, Vec<u8>, u32)> = clusters
        .par_iter()
        .map(|cluster| {
            let full_panels = cluster.len() / 8;
            let tail = cluster.len() % 8;
            let mut vbuf = Vec::with_capacity(cluster.len() * DIM * std::mem::size_of::<i16>());
            let mut lbuf = Vec::with_capacity(cluster.len());
            for panel in 0..full_panels {
                let panel_start = panel * 8;
                for dim in 0..DIM {
                    for lane in 0..8 {
                        let q = quantize_value(cluster[panel_start + lane].vector[dim]);
                        vbuf.extend_from_slice(&q.to_le_bytes());
                    }
                }
            }
            let tail_start = full_panels * 8;
            for lane in 0..tail {
                for dim in 0..DIM {
                    let q = quantize_value(cluster[tail_start + lane].vector[dim]);
                    vbuf.extend_from_slice(&q.to_le_bytes());
                }
            }
            for rv in cluster {
                lbuf.push(if rv.is_fraud { 1u8 } else { 0u8 });
            }
            let vector_units = (vbuf.len() / std::mem::size_of::<i16>()) as u32;
            (vbuf, lbuf, vector_units)
        })
        .collect();

    println!("Writing resources/specialist.bin...");
    write_specialist_index(&centroids, &clusters, &cluster_buffers)?;

    println!(
        "Preprocessing complete. K={}, total vectors={}, output=resources/specialist.bin",
        K,
        vectors.len()
    );
    Ok(())
}

#[inline(always)]
fn quantize_value(value: f32) -> i16 {
    let rounded4 = (value * SCALE).round() / SCALE;
    let scaled = (rounded4 * SCALE).round();
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

fn write_specialist_index(
    centroids: &[[f32; DIM]],
    clusters: &[Vec<&RawVector>],
    cluster_buffers: &[(Vec<u8>, Vec<u8>, u32)],
) -> anyhow::Result<()> {
    let total_vectors: u32 = clusters.iter().map(|cluster| cluster.len() as u32).sum();
    let mut cluster_offsets = Vec::with_capacity(K + 1);
    let mut panel_offsets = Vec::with_capacity(K + 1);
    let mut current_cluster_offset = 0u32;
    let mut current_panel_offset = 0u32;

    cluster_offsets.push(current_cluster_offset);
    panel_offsets.push(current_panel_offset);
    for (cluster, (_, _, vector_units)) in clusters.iter().zip(cluster_buffers.iter()) {
        current_cluster_offset += cluster.len() as u32;
        current_panel_offset += *vector_units;
        cluster_offsets.push(current_cluster_offset);
        panel_offsets.push(current_panel_offset);
    }

    let mut file = BufWriter::new(File::create("resources/specialist.bin")?);
    for value in [
        INDEX_MAGIC,
        INDEX_VERSION,
        total_vectors,
        DIM as u32,
        K as u32,
        SCALE as u32,
        0,
        0,
    ] {
        file.write_all(&value.to_le_bytes())?;
    }

    for centroid in centroids {
        for &value in centroid {
            file.write_all(&value.to_le_bytes())?;
        }
    }

    for cluster in clusters {
        file.write_all(&(cluster.len() as u32).to_le_bytes())?;
    }
    for offset in cluster_offsets {
        file.write_all(&offset.to_le_bytes())?;
    }
    for offset in panel_offsets {
        file.write_all(&offset.to_le_bytes())?;
    }

    for (vbuf, _, _) in cluster_buffers {
        file.write_all(vbuf)?;
    }
    for (_, lbuf, _) in cluster_buffers {
        file.write_all(lbuf)?;
    }
    file.flush()?;
    Ok(())
}
