use std::cmp::Ordering;
use std::collections::BinaryHeap;
use memmap2::Mmap;

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
    centroid_mmap: Mmap,
    vector_mmap: Mmap,
    label_mmap: Mmap,
    offset_mmap: Mmap,
}

impl VectorStore {
    pub fn from_mmaps(
        centroid_mmap: Mmap,
        vector_mmap: Mmap,
        label_mmap: Mmap,
        offset_mmap: Mmap,
    ) -> Self {
        Self {
            centroid_mmap,
            vector_mmap,
            label_mmap,
            offset_mmap,
        }
    }

    #[inline(always)]
    fn centroids(&self) -> &[[f32; 14]] {
        unsafe {
            let ptr = self.centroid_mmap.as_ptr() as *const [f32; 14];
            let len = self.centroid_mmap.len() / std::mem::size_of::<[f32; 14]>();
            std::slice::from_raw_parts(ptr, len)
        }
    }

    #[inline(always)]
    fn vectors(&self) -> &[f32] {
        unsafe {
            let ptr = self.vector_mmap.as_ptr() as *const f32;
            let len = self.vector_mmap.len() / std::mem::size_of::<f32>();
            std::slice::from_raw_parts(ptr, len)
        }
    }

    #[inline(always)]
    fn labels(&self) -> &[u8] {
        &self.label_mmap
    }

    #[inline(always)]
    fn offsets(&self) -> &[(u32, u32)] {
        unsafe {
            let ptr = self.offset_mmap.as_ptr() as *const (u32, u32);
            let len = self.offset_mmap.len() / std::mem::size_of::<(u32, u32)>();
            std::slice::from_raw_parts(ptr, len)
        }
    }

    pub fn find_k_nearest(&self, query: &[f32; 14], k: usize, n_probes: usize) -> Vec<bool> {
        let mut heap: BinaryHeap<Neighbor> = BinaryHeap::with_capacity(k);
        
        let centroids = self.centroids();
        let vectors = self.vectors();
        let labels = self.labels();
        let offsets = self.offsets();

        // 1. Find the top N nearest centroids
        let mut centroid_heap: BinaryHeap<Neighbor> = BinaryHeap::with_capacity(n_probes);
        
        for (idx, c) in centroids.iter().enumerate() {
            let mut d_sq = 0.0;
            // Using a loop that the compiler can easily auto-vectorize
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
            let (start, size) = offsets[centroid_idx];
            
            for i in 0..size {
                let absolute_idx = (start + i) as usize;
                let vec_start = absolute_idx * 14;
                let v = &vectors[vec_start..vec_start+14];
                
                let mut d_sq = 0.0;
                // Optimization: using iterators or fixed-size loops often helps auto-vectorization
                for j in 0..14 {
                    let d = query[j] - v[j];
                    d_sq += d * d;
                }

                if heap.len() < k {
                    heap.push(Neighbor { distance_sq: d_sq, index_or_fraud: labels[absolute_idx] as usize });
                } else if d_sq < heap.peek().unwrap().distance_sq {
                    heap.pop();
                    heap.push(Neighbor { distance_sq: d_sq, index_or_fraud: labels[absolute_idx] as usize });
                }
            }
        }
        
        heap.into_iter().map(|n| n.index_or_fraud == 1).collect()
    }
}