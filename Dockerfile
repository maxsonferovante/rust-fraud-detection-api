# syntax=docker/dockerfile:1.7

# Stage 1: Preprocessor (runs on native host architecture to avoid emulation slowness)
FROM --platform=$BUILDPLATFORM rust:1.95-slim-bookworm AS preprocessor-runner

RUN apt-get update && apt-get install -y pkg-config libssl-dev curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the minimal set of files needed to build/run the preprocessor.
# This keeps the expensive preprocessor step cached when only API/LB code changes.
COPY Cargo.toml Cargo.lock ./
COPY src/bin/preprocessor.rs ./src/bin/preprocessor.rs
COPY src/models.rs ./src/models.rs

# Download resources (needed for preprocessing)
RUN mkdir -p resources && \
    curl -L https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/resources/mcc_risk.json -o resources/mcc_risk.json && \
    curl -L https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/resources/normalization.json -o resources/normalization.json && \
    curl -L https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/resources/references.json.gz -o resources/references.json.gz

# Build (cacheable) and run the preprocessor natively with SIMD optimized for the build machine.
# BuildKit cache mounts drastically speed up repeated builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo build --release --bin preprocessor
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    ./target/release/preprocessor

# Stage 2: Compila a API via cross-compilation a partir da plataforma nativa ($BUILDPLATFORM)
# Isso evita o bug de use-after-free em proc_macro do rustc 1.95 no QEMU sob Apple Silicon
FROM --platform=$BUILDPLATFORM rust:1.95-slim-bookworm AS builder

# Instala o compilador C cross-compiler para x86_64 e o target Rust correspondente
RUN apt-get update && apt-get install -y gcc-x86-64-linux-gnu && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-gnu

WORKDIR /app
COPY Cargo.toml Cargo.lock ./

# Warm dependency build with tiny stubs so dependency compilation is cached even when sources change.
RUN mkdir -p src/bin && \
    printf '%s\n' 'fn main() {}' > src/main.rs && \
    printf '%s\n' 'fn main() {}' > src/bin/lb.rs

# Indica ao Cargo qual linker usar para o target x86_64
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc

# Haswell: AVX2, FMA, BMI1/BMI2 — instruções vetoriais para o Mac Mini Intel
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target-cache \
    CARGO_TARGET_DIR=/app/target-cache \
    RUSTFLAGS="-C target-cpu=haswell -C opt-level=3" cargo build --release --target x86_64-unknown-linux-gnu --bin fraud-detection-api --bin lb

# Now copy the full source tree and build the real binaries.
COPY . .

# Haswell: AVX2, FMA, BMI1/BMI2 — instruções vetoriais para o Mac Mini Intel
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    RUSTFLAGS="-C target-cpu=haswell -C opt-level=3" cargo build --release --target x86_64-unknown-linux-gnu --bin fraud-detection-api --bin lb

# Stage 3: Runtime — mesma plataforma do binário compilado
FROM --platform=linux/amd64 debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
# Copia o binário compilado para o target correspondente
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/fraud-detection-api .
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/lb .

# Copy the preprocessed data (shared across all architectures)
COPY --from=preprocessor-runner /app/resources/*.bin ./resources/
COPY --from=preprocessor-runner /app/resources/normalization.json ./resources/
COPY --from=preprocessor-runner /app/resources/mcc_risk.json ./resources/

EXPOSE 9999
CMD ["./fraud-detection-api"]
