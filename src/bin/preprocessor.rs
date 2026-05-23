#[path = "../models.rs"]
mod models;

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use flate2::read::GzDecoder;
use serde::de::{Visitor, SeqAccess, Deserializer};
use std::fmt;
use rand::seq::SliceRandom;
use rayon::prelude::*;

const K: usize = 2048;     // Número de clusters IVF
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

/// Distância euclidiana ao quadrado — loop fixo de 14 elementos.
/// Com RUSTFLAGS="-C target-cpu=native", o compilador auto-vetoriza
/// usando YMM (AVX2) ou NEON dependendo da arquitetura de build.
#[inline(always)]
fn dist_sq(a: &[f32; 14], b: &[f32; 14]) -> f32 {
    let mut sum = 0.0f32;
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
    println!("Running K-means (max {} iterations, eps={})...", MAX_ITER, CONVERGENCE_EPS);
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

        // Atualiza centroids e mede deslocamento máximo para early stopping
        let mut max_shift_sq = 0.0f32;
        for i in 0..K {
            if new_centroids_data[i].1 > 0 {
                let mut new_c = [0.0f32; 14];
                for j in 0..14 {
                    new_c[j] = new_centroids_data[i].0[j] / new_centroids_data[i].1 as f32;
                }
                let shift = dist_sq(&new_c, &centroids[i]);
                if shift > max_shift_sq {
                    max_shift_sq = shift;
                }
                centroids[i] = new_c;
            }
        }

        println!("Iteration {} complete (max_shift²={:.2e})", iter + 1, max_shift_sq);

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
    // Escrita dos arquivos binários.
    //
    // Estratégia de paralelismo:
    //   1. Pré-serialização paralela: cada cluster converte seus vetores em
    //      bytes via par_iter() → Vec<u8> por cluster. Zero I/O nesta fase.
    //   2. Escrita sequencial: os buffers pré-computados são escritos em ordem
    //      (obrigatório — FileSystem não é thread-safe para writes ordenados).
    //   3. BufWriter: agrupa syscalls write() em chunks de 64KB, eliminando
    //      o overhead de uma syscall por vetor (era ~3M syscalls por arquivo).
    // -----------------------------------------------------------------------
    println!("Pre-serializing cluster buffers in parallel...");

    // Pré-computa os buffers de vetor e label de cada cluster em paralelo
    let cluster_buffers: Vec<(Vec<u8>, Vec<u8>)> = clusters
        .par_iter()
        .map(|cluster| {
            let mut vbuf = Vec::with_capacity(cluster.len() * 14 * 4);
            let mut lbuf = Vec::with_capacity(cluster.len());
            for rv in cluster {
                let bytes = unsafe {
                    std::slice::from_raw_parts(rv.vector.as_ptr() as *const u8, 14 * 4)
                };
                vbuf.extend_from_slice(bytes);
                lbuf.push(if rv.is_fraud { 1u8 } else { 0u8 });
            }
            (vbuf, lbuf)
        })
        .collect();

    println!("Writing IVF binary files...");

    // BufWriter(64KB) elimina syscall por vetor — eram ~3M writes individuais
    let mut centroid_file = BufWriter::new(File::create("resources/centroids.bin")?);
    for c in &centroids {
        let bytes = unsafe {
            std::slice::from_raw_parts(c.as_ptr() as *const u8, 14 * 4)
        };
        centroid_file.write_all(bytes)?;
    }
    centroid_file.flush()?;

    let mut vector_file = BufWriter::new(File::create("resources/ivf_vectors.bin")?);
    let mut label_file  = BufWriter::new(File::create("resources/ivf_labels.bin")?);
    let mut offset_file = BufWriter::new(File::create("resources/ivf_offsets.bin")?);

    // Escrita sequencial dos buffers pré-computados (ordem preservada)
    let mut current_offset = 0u32;
    for (cluster, (vbuf, lbuf)) in clusters.iter().zip(cluster_buffers.iter()) {
        offset_file.write_all(&current_offset.to_le_bytes())?;
        let cluster_size = cluster.len() as u32;
        offset_file.write_all(&cluster_size.to_le_bytes())?;

        vector_file.write_all(vbuf)?;
        label_file.write_all(lbuf)?;

        current_offset += cluster_size;
    }

    // Flush explícito dos BufWriters antes de encerrar
    vector_file.flush()?;
    label_file.flush()?;
    offset_file.flush()?;

    println!("Preprocessing complete. K={}, total vectors={}", K, vectors.len());
    Ok(())
}
