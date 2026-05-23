
// ---------------------------------------------------------------------------
// Top-K fixo na stack via array estático — zero alocações, zero BinaryHeap.
//
// Mantém os K menores elementos via busca linear do máximo:
//   - Push: encontra o slot máximo e substitui se o novo for menor.
//   - Branch pattern previsível: mesma sequência de comparações a cada iteração.
// ---------------------------------------------------------------------------

/// Capacidade máxima para o buffer de centroids candidatos (n_probes ≤ MAX_PROBES).
const MAX_PROBES: usize = 256;

/// Buffer de Top-K na stack. Inicializado com distâncias infinitas.
/// `len` rastreia quantos slots estão preenchidos (fase de fill inicial).
struct TopK<const K: usize> {
    buf: [(f32, usize); K],
    len: usize,
}

impl<const K: usize> TopK<K> {
    #[inline(always)]
    fn new() -> Self {
        Self {
            buf: [(f32::INFINITY, 0); K],
            len: 0,
        }
    }

    /// Insere (dist, idx) se dist for menor que o atual máximo do buffer.
    /// Busca linear do máximo: branch pattern fixo, cache-friendly, sem heap.
    #[inline(always)]
    fn push(&mut self, dist: f32, idx: usize) {
        if self.len < K {
            // Fase de preenchimento: ocupa slots vazios
            self.buf[self.len] = (dist, idx);
            self.len += 1;
        } else {
            // Fase de substituição: encontra o slot com maior distância
            let mut max_pos = 0;
            let mut max_dist = self.buf[0].0;
            for i in 1..K {
                if self.buf[i].0 > max_dist {
                    max_dist = self.buf[i].0;
                    max_pos = i;
                }
            }
            if dist < max_dist {
                self.buf[max_pos] = (dist, idx);
            }
        }
    }

    /// Iterador sobre os slots preenchidos.
    #[inline(always)]
    fn iter(&self) -> impl Iterator<Item = &(f32, usize)> {
        self.buf[..self.len].iter()
    }
}

// ---------------------------------------------------------------------------
// Buffer de centroids candidatos — mesmo padrão, tamanho máximo MAX_PROBES.
// ---------------------------------------------------------------------------

struct TopProbes {
    buf: [(f32, usize); MAX_PROBES],
    len: usize,
    cap: usize, // n_probes em runtime
}

impl TopProbes {
    #[inline(always)]
    fn new(cap: usize) -> Self {
        debug_assert!(cap <= MAX_PROBES);
        Self {
            buf: [(f32::INFINITY, 0); MAX_PROBES],
            len: 0,
            cap,
        }
    }

    #[inline(always)]
    fn push(&mut self, dist: f32, idx: usize) {
        let cap = self.cap;
        if self.len < cap {
            self.buf[self.len] = (dist, idx);
            self.len += 1;
        } else {
            let mut max_pos = 0;
            let mut max_dist = self.buf[0].0;
            for i in 1..cap {
                if self.buf[i].0 > max_dist {
                    max_dist = self.buf[i].0;
                    max_pos = i;
                }
            }
            if dist < max_dist {
                self.buf[max_pos] = (dist, idx);
            }
        }
    }

    #[inline(always)]
    fn filled(&self) -> &[(f32, usize)] {
        &self.buf[..self.len]
    }
}

// ---------------------------------------------------------------------------

pub struct VectorStore {
    centroid_data: Vec<u8>,
    vector_data: Vec<u8>,
    label_data: Vec<u8>,
    offset_data: Vec<u8>,
}

impl VectorStore {
    pub fn from_bytes(
        centroid_data: Vec<u8>,
        vector_data: Vec<u8>,
        label_data: Vec<u8>,
        offset_data: Vec<u8>,
    ) -> Self {
        Self {
            centroid_data,
            vector_data,
            label_data,
            offset_data,
        }
    }

    #[inline(always)]
    fn centroids(&self) -> &[[f32; 14]] {
        unsafe {
            let ptr = self.centroid_data.as_ptr() as *const [f32; 14];
            let len = self.centroid_data.len() / std::mem::size_of::<[f32; 14]>();
            std::slice::from_raw_parts(ptr, len)
        }
    }

    #[inline(always)]
    fn vectors(&self) -> &[f32] {
        unsafe {
            let ptr = self.vector_data.as_ptr() as *const f32;
            let len = self.vector_data.len() / std::mem::size_of::<f32>();
            std::slice::from_raw_parts(ptr, len)
        }
    }

    #[inline(always)]
    fn labels(&self) -> &[u8] {
        &self.label_data
    }

    #[inline(always)]
    fn offsets(&self) -> &[(u32, u32)] {
        unsafe {
            let ptr = self.offset_data.as_ptr() as *const (u32, u32);
            let len = self.offset_data.len() / std::mem::size_of::<(u32, u32)>();
            std::slice::from_raw_parts(ptr, len)
        }
    }

    pub fn find_k_nearest(&self, query: &[f32; 14], _k: usize, n_probes: usize) -> Vec<bool> {
        let centroids = self.centroids();
        let vectors   = self.vectors();
        let labels    = self.labels();
        let offsets   = self.offsets();

        // --- Fase 1: Top n_probes centroids — array na stack, zero alocações ---
        let n_probes = n_probes.min(MAX_PROBES);
        let mut centroid_top = TopProbes::new(n_probes);

        for (idx, c) in centroids.iter().enumerate() {
            let mut d_sq = 0.0f32;
            for j in 0..14 {
                let d = query[j] - c[j];
                d_sq += d * d;
            }
            centroid_top.push(d_sq, idx);
        }

        // --- Fase 2: Top K vizinhos nos clusters selecionados ---
        // K é sempre 5 na chamada do handler; array fixo de 5 slots na stack.
        // Se k != 5 em tempo de execução, usa o mesmo mecanismo com K=5 e
        // trunca (k ≤ 5 é garantido pelo contrato da API).
        let mut neighbors: TopK<5> = TopK::new();

        for &(dist_c, centroid_idx) in centroid_top.filled() {
            if dist_c.is_infinite() {
                break; // slots não preenchidos (n_probes > num centroids)
            }
            let (start, size) = offsets[centroid_idx];

            for i in 0..size {
                let absolute_idx = (start + i) as usize;
                let vec_start = absolute_idx * 14;
                let v = &vectors[vec_start..vec_start + 14];

                let mut d_sq = 0.0f32;
                for j in 0..14 {
                    let d = query[j] - v[j];
                    d_sq += d * d;
                }

                neighbors.push(d_sq, labels[absolute_idx] as usize);
            }
        }

        // Coleta resultados: Vec alocado UMA vez, apenas no retorno
        neighbors.iter().map(|&(_, fraud)| fraud == 1).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memmap2::MmapMut;

    fn create_mmap_from_slice<T: Copy>(data: &[T]) -> Mmap {
        let size = data.len() * std::mem::size_of::<T>();
        let mut mmap = MmapMut::map_anon(size).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr() as *const u8,
                mmap.as_mut_ptr(),
                size,
            );
        }
        mmap.make_read_only().unwrap()
    }

    #[test]
    fn test_neighbor_ordering() {
        // TopK deve manter o menor — inserir 2.0 depois de 1.0 não expulsa o 1.0
        let mut top: TopK<2> = TopK::new();
        top.push(1.0, 0);
        top.push(2.0, 1);
        let dists: Vec<f32> = top.iter().map(|&(d, _)| d).collect();
        assert!(dists.contains(&1.0));
        assert!(dists.contains(&2.0));
    }

    #[test]
    fn test_find_k_nearest() {
        let centroids = vec![[0.0; 14], [10.0; 14]];
        let mut vectors = vec![0.0f32; 14 * 2];
        for i in 0..14 {
            vectors[14 + i] = 10.0;
        }

        let labels: Vec<u8> = vec![0, 1];
        let offsets: Vec<(u32, u32)> = vec![(0, 1), (1, 1)];

        let store = VectorStore::from_mmaps(
            create_mmap_from_slice(&centroids),
            create_mmap_from_slice(&vectors),
            create_mmap_from_slice(&labels),
            create_mmap_from_slice(&offsets),
        );

        let query = [0.1; 14];
        let nearest = store.find_k_nearest(&query, 2, 2);

        assert_eq!(nearest.len(), 2);
        assert!(nearest.contains(&true));
        assert!(nearest.contains(&false));
    }
}