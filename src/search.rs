use std::cmp::Ordering;
use std::collections::BinaryHeap;
use half::f16;

#[derive(PartialEq, Clone, Copy)]
struct Neighbor {
    distance_sq: f32,
    index_or_fraud: usize, // Overloaded for both centroid index and fraud boolean
}

impl Eq for Neighbor {}

impl Ord for Neighbor {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap behavior
        self.distance_sq.partial_cmp(&other.distance_sq).unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for Neighbor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct VectorStore {
    centroids: Vec<[f32; 14]>,
    vectors: Vec<f16>, 
    labels: Vec<u8>,
    offsets: Vec<(u32, u32)>, // (start, size)
}

impl VectorStore {
    pub fn from_binary(
        centroids: Vec<[f32; 14]>,
        vectors: Vec<f16>,
        labels: Vec<u8>,
        offsets: Vec<(u32, u32)>,
    ) -> Self {
        Self {
            centroids,
            vectors,
            labels,
            offsets,
        }
    }

    pub fn find_k_nearest(&self, query: &[f32; 14], k: usize) -> Vec<bool> {
        let mut heap: BinaryHeap<Neighbor> = BinaryHeap::with_capacity(k);
        
        // 1. Find the top N nearest centroids (e.g., N=6)
        let n_probes = 6;
        let mut centroid_heap: BinaryHeap<Neighbor> = BinaryHeap::with_capacity(n_probes);
        
        for (idx, c) in self.centroids.iter().enumerate() {
            let mut d_sq = 0.0;
            for j in 0..14 {
                let d = query[j] - c[j];
                d_sq += d * d;
            }
            
            if centroid_heap.len() < n_probes {
                centroid_heap.push(Neighbor { distance_sq: d_sq, index_or_fraud: idx });
            } else if d_sq < centroid_heap.peek().unwrap().distance_sq {
                centroid_heap.pop();
                centroid_heap.push(Neighbor { distance_sq: d_sq, index_or_fraud: idx });
            }
        }
        
        // 2. Search only in the vectors of those top N centroids
        for centroid_neighbor in centroid_heap {
            let centroid_idx = centroid_neighbor.index_or_fraud;
            let (start, size) = self.offsets[centroid_idx];
            
            for i in 0..size {
                let absolute_idx = (start + i) as usize;
                let vec_start = absolute_idx * 14;
                let v = &self.vectors[vec_start..vec_start+14];
                
                let mut d_sq = 0.0;
                // Manual unroll for speed
                for j in 0..14 {
                    let d = query[j] - v[j].to_f32();
                    d_sq += d * d;
                }

                if heap.len() < k {
                    heap.push(Neighbor { distance_sq: d_sq, index_or_fraud: self.labels[absolute_idx] as usize });
                } else if d_sq < heap.peek().unwrap().distance_sq {
                    heap.pop();
                    heap.push(Neighbor { distance_sq: d_sq, index_or_fraud: self.labels[absolute_idx] as usize });
                }
            }
        }
        
        heap.into_iter().map(|n| n.index_or_fraud == 1).collect()
    }
}
