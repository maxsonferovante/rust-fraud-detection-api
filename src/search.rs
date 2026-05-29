use anyhow::{bail, Context};
use std::path::Path;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{
    __m128i, _mm256_cvtepi16_epi32, _mm256_cvtepi32_ps, _mm256_fmadd_ps, _mm256_set1_ps,
    _mm256_setzero_ps, _mm256_storeu_ps, _mm256_sub_ps, _mm_loadu_si128,
};

pub const DIM: usize = 14;
pub const SCALE: f32 = 10_000.0;
pub const INDEX_MAGIC: u32 = u32::from_le_bytes(*b"RIVF");
pub const INDEX_VERSION: u32 = 1;
const HEADER_U32S: usize = 8;
const MAX_PROBES: usize = 512;

#[derive(Debug, Clone, Copy)]
struct IndexHeader {
    n_vectors: usize,
    n_clusters: usize,
    scale: f32,
}

struct TopK<const K: usize> {
    buf: [(f32, u8); K],
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

    #[inline(always)]
    fn push(&mut self, dist: f32, label: u8) {
        if self.len < K {
            self.buf[self.len] = (dist, label);
            self.len += 1;
            return;
        }

        let mut max_pos = 0;
        let mut max_dist = self.buf[0].0;
        for i in 1..K {
            if self.buf[i].0 > max_dist {
                max_dist = self.buf[i].0;
                max_pos = i;
            }
        }
        if dist < max_dist {
            self.buf[max_pos] = (dist, label);
        }
    }

    #[inline(always)]
    fn fraud_count(&self) -> usize {
        self.buf[..self.len]
            .iter()
            .filter(|&&(_, label)| label == 1)
            .count()
    }

    #[allow(dead_code)]
    #[inline(always)]
    fn labels(&self) -> impl Iterator<Item = bool> + '_ {
        self.buf[..self.len].iter().map(|&(_, label)| label == 1)
    }
}

struct TopProbes {
    buf: [(f32, usize); MAX_PROBES],
    len: usize,
    cap: usize,
}

impl TopProbes {
    #[inline(always)]
    fn new(cap: usize) -> Self {
        Self {
            buf: [(f32::INFINITY, 0); MAX_PROBES],
            len: 0,
            cap: cap.min(MAX_PROBES),
        }
    }

    #[inline(always)]
    fn push(&mut self, dist: f32, idx: usize) {
        if self.len < self.cap {
            self.buf[self.len] = (dist, idx);
            self.len += 1;
            return;
        }

        let mut max_pos = 0;
        let mut max_dist = self.buf[0].0;
        for i in 1..self.cap {
            if self.buf[i].0 > max_dist {
                max_dist = self.buf[i].0;
                max_pos = i;
            }
        }
        if dist < max_dist {
            self.buf[max_pos] = (dist, idx);
        }
    }

    #[inline(always)]
    fn filled(&self) -> &[(f32, usize)] {
        &self.buf[..self.len]
    }
}

pub struct VectorStore {
    centroids: Vec<[f32; DIM]>,
    cluster_sizes: Vec<u32>,
    cluster_offsets: Vec<u32>,
    panel_offsets: Vec<u32>,
    vectors_soa: Vec<i16>,
    labels: Vec<u8>,
    inv_scale: f32,
}

impl VectorStore {
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read IVF index at {}", path.display()))?;
        Self::from_bytes(bytes)
            .with_context(|| format!("failed to parse IVF index at {}", path.display()))
    }

    pub fn from_bytes(data: Vec<u8>) -> anyhow::Result<Self> {
        let mut cursor = 0usize;
        let header = read_header(&data, &mut cursor)?;

        let mut centroids = Vec::with_capacity(header.n_clusters);
        for _ in 0..header.n_clusters {
            let mut row = [0.0f32; DIM];
            for item in row.iter_mut() {
                *item = read_f32(&data, &mut cursor)?;
            }
            centroids.push(row);
        }

        let cluster_sizes = read_u32_vec(&data, &mut cursor, header.n_clusters)?;
        let cluster_offsets = read_u32_vec(&data, &mut cursor, header.n_clusters + 1)?;
        let panel_offsets = read_u32_vec(&data, &mut cursor, header.n_clusters + 1)?;

        let vector_units = *panel_offsets
            .last()
            .context("index is missing final panel offset")? as usize;
        let vectors_soa = read_i16_vec(&data, &mut cursor, vector_units)?;
        let labels = read_u8_vec(&data, &mut cursor, header.n_vectors)?;

        if cursor != data.len() {
            bail!(
                "index has {} trailing bytes after parsed payload",
                data.len() - cursor
            );
        }
        if cluster_offsets.last().copied().unwrap_or_default() as usize != header.n_vectors {
            bail!("cluster offsets do not sum to vector count");
        }
        if labels.len() != header.n_vectors {
            bail!("label count does not match vector count");
        }

        Ok(Self {
            centroids,
            cluster_sizes,
            cluster_offsets,
            panel_offsets,
            vectors_soa,
            labels,
            inv_scale: 1.0 / header.scale,
        })
    }

    #[allow(dead_code)]
    pub fn fraud_count_nearest(&self, query: &[f32; DIM], n_probes: usize) -> usize {
        let quantized = quantize_vector(query);
        self.fraud_count_nearest_i16(&quantized, n_probes)
    }

    pub fn fraud_count_nearest_i16(&self, query: &[i16; DIM], n_probes: usize) -> usize {
        self.find_top5_i16(query, n_probes).fraud_count()
    }

    #[allow(dead_code)]
    pub fn find_k_nearest(&self, query: &[f32; DIM], _k: usize, n_probes: usize) -> Vec<bool> {
        let quantized = quantize_vector(query);
        self.find_top5_i16(&quantized, n_probes).labels().collect()
    }

    fn find_top5_i16(&self, query: &[i16; DIM], n_probes: usize) -> TopK<5> {
        let mut centroid_top = TopProbes::new(n_probes.min(self.centroids.len()));
        for (idx, centroid) in self.centroids.iter().enumerate() {
            let mut dist = 0.0f32;
            for dim in 0..DIM {
                let d = query[dim] as f32 * self.inv_scale - centroid[dim];
                dist += d * d;
            }
            centroid_top.push(dist, idx);
        }

        let mut probe_buf: [(u32, usize); MAX_PROBES] = [(0, 0); MAX_PROBES];
        let mut valid_count = 0usize;
        for &(_, centroid_idx) in centroid_top.filled() {
            probe_buf[valid_count] = (self.cluster_sizes[centroid_idx], centroid_idx);
            valid_count += 1;
        }
        probe_buf[..valid_count].sort_unstable_by_key(|&(size, _)| size);

        let mut neighbors = TopK::<5>::new();
        for &(_, centroid_idx) in &probe_buf[..valid_count] {
            self.scan_cluster(centroid_idx, query, &mut neighbors);
        }
        neighbors
    }

    #[inline(always)]
    fn scan_cluster(&self, cluster_idx: usize, query: &[i16; DIM], neighbors: &mut TopK<5>) {
        let size = self.cluster_sizes[cluster_idx] as usize;
        let label_start = self.cluster_offsets[cluster_idx] as usize;
        let panel_start = self.panel_offsets[cluster_idx] as usize;
        let full_panels = size / 8;
        #[cfg(target_arch = "x86_64")]
        let use_avx2 =
            std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma");

        for panel in 0..full_panels {
            let base = panel_start + panel * DIM * 8;
            let label_base = label_start + panel * 8;

            #[cfg(target_arch = "x86_64")]
            let dist = if use_avx2 {
                unsafe { self.panel_distance_avx2(base, query) }
            } else {
                self.panel_distance_scalar(base, query)
            };

            #[cfg(not(target_arch = "x86_64"))]
            let dist = self.panel_distance_scalar(base, query);

            for lane in 0..8 {
                neighbors.push(dist[lane], self.labels[label_base + lane]);
            }
        }

        let tail = size % 8;
        if tail == 0 {
            return;
        }

        let base = panel_start + full_panels * DIM * 8;
        let label_base = label_start + full_panels * 8;
        for lane in 0..tail {
            let mut dist = 0i64;
            for dim in 0..DIM {
                let d = query[dim] as i32 - self.vectors_soa[base + lane * DIM + dim] as i32;
                dist += (d * d) as i64;
            }
            neighbors.push(dist as f32, self.labels[label_base + lane]);
        }
    }

    #[inline(always)]
    fn panel_distance_scalar(&self, base: usize, query: &[i16; DIM]) -> [f32; 8] {
        let mut dist = [0.0f32; 8];
        for dim in 0..DIM {
            let q = query[dim] as f32;
            let dim_base = base + dim * 8;
            for lane in 0..8 {
                let d = q - self.vectors_soa[dim_base + lane] as f32;
                dist[lane] += d * d;
            }
        }
        dist
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,fma")]
    unsafe fn panel_distance_avx2(&self, base: usize, query: &[i16; DIM]) -> [f32; 8] {
        let mut acc = _mm256_setzero_ps();
        for dim in 0..DIM {
            let dim_base = base + dim * 8;
            let packed = _mm_loadu_si128(self.vectors_soa.as_ptr().add(dim_base) as *const __m128i);
            let values = _mm256_cvtepi16_epi32(packed);
            let values = _mm256_cvtepi32_ps(values);
            let q = _mm256_set1_ps(query[dim] as f32);
            let diff = _mm256_sub_ps(q, values);
            acc = _mm256_fmadd_ps(diff, diff, acc);
        }

        let mut out = [0.0f32; 8];
        _mm256_storeu_ps(out.as_mut_ptr(), acc);
        out
    }
}

pub fn quantize_value(value: f32) -> i16 {
    let rounded4 = (value * SCALE).round() / SCALE;
    let scaled = (rounded4 * SCALE).round();
    scaled.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

pub fn quantize_vector(vector: &[f32; DIM]) -> [i16; DIM] {
    let mut out = [0i16; DIM];
    for i in 0..DIM {
        out[i] = quantize_value(vector[i]);
    }
    out
}

fn read_header(data: &[u8], cursor: &mut usize) -> anyhow::Result<IndexHeader> {
    if data.len() < HEADER_U32S * 4 {
        bail!("index too small to contain header");
    }
    let magic = read_u32(data, cursor)?;
    let version = read_u32(data, cursor)?;
    let n_vectors = read_u32(data, cursor)? as usize;
    let dim = read_u32(data, cursor)? as usize;
    let n_clusters = read_u32(data, cursor)? as usize;
    let scale = read_u32(data, cursor)? as f32;
    let _reserved0 = read_u32(data, cursor)?;
    let _reserved1 = read_u32(data, cursor)?;

    if magic != INDEX_MAGIC {
        bail!("invalid IVF index magic");
    }
    if version != INDEX_VERSION {
        bail!("unsupported IVF index version {version}");
    }
    if dim != DIM {
        bail!("unsupported IVF dimension {dim}");
    }
    if n_clusters == 0 || n_vectors == 0 {
        bail!("index has empty cluster/vector counts");
    }

    Ok(IndexHeader {
        n_vectors,
        n_clusters,
        scale,
    })
}

fn read_u8_vec(data: &[u8], cursor: &mut usize, len: usize) -> anyhow::Result<Vec<u8>> {
    let end = cursor
        .checked_add(len)
        .context("u8 payload length overflow")?;
    if end > data.len() {
        bail!("index ended while reading u8 payload");
    }
    let out = data[*cursor..end].to_vec();
    *cursor = end;
    Ok(out)
}

fn read_u32_vec(data: &[u8], cursor: &mut usize, len: usize) -> anyhow::Result<Vec<u32>> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_u32(data, cursor)?);
    }
    Ok(out)
}

fn read_i16_vec(data: &[u8], cursor: &mut usize, len: usize) -> anyhow::Result<Vec<i16>> {
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(read_i16(data, cursor)?);
    }
    Ok(out)
}

fn read_u32(data: &[u8], cursor: &mut usize) -> anyhow::Result<u32> {
    let bytes = read_array::<4>(data, cursor)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_i16(data: &[u8], cursor: &mut usize) -> anyhow::Result<i16> {
    let bytes = read_array::<2>(data, cursor)?;
    Ok(i16::from_le_bytes(bytes))
}

fn read_f32(data: &[u8], cursor: &mut usize) -> anyhow::Result<f32> {
    let bytes = read_array::<4>(data, cursor)?;
    Ok(f32::from_le_bytes(bytes))
}

fn read_array<const N: usize>(data: &[u8], cursor: &mut usize) -> anyhow::Result<[u8; N]> {
    let end = cursor.checked_add(N).context("index cursor overflow")?;
    if end > data.len() {
        bail!("index ended unexpectedly");
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&data[*cursor..end]);
    *cursor = end;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_u32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_f32(buf: &mut Vec<u8>, value: f32) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn push_i16(buf: &mut Vec<u8>, value: i16) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn tiny_index_bytes() -> Vec<u8> {
        let mut buf = Vec::new();
        push_u32(&mut buf, INDEX_MAGIC);
        push_u32(&mut buf, INDEX_VERSION);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, DIM as u32);
        push_u32(&mut buf, 2);
        push_u32(&mut buf, SCALE as u32);
        push_u32(&mut buf, 0);
        push_u32(&mut buf, 0);

        for value in [0.0f32; DIM] {
            push_f32(&mut buf, value);
        }
        for value in [1.0f32; DIM] {
            push_f32(&mut buf, value);
        }

        for value in [1u32, 1] {
            push_u32(&mut buf, value);
        }
        for value in [0u32, 1, 2] {
            push_u32(&mut buf, value);
        }
        for value in [0u32, DIM as u32, (DIM * 2) as u32] {
            push_u32(&mut buf, value);
        }

        for _ in 0..DIM {
            push_i16(&mut buf, 0);
        }
        for _ in 0..DIM {
            push_i16(&mut buf, SCALE as i16);
        }
        buf.extend_from_slice(&[0, 1]);
        buf
    }

    #[test]
    fn quantize_preserves_round4_values() {
        assert_eq!(quantize_value(0.12344), 1234);
        assert_eq!(quantize_value(0.12345), 1235);
        assert_eq!(quantize_value(-1.0), -10000);
    }

    #[test]
    fn find_k_nearest_reads_int16_index() {
        let store = VectorStore::from_bytes(tiny_index_bytes()).unwrap();
        let nearest = store.find_k_nearest(&[0.01; DIM], 2, 2);

        assert_eq!(nearest.len(), 2);
        assert!(nearest.contains(&true));
        assert!(nearest.contains(&false));
    }
}
