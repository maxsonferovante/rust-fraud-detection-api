# Stage 1: Preprocessor (runs on native host architecture to avoid emulation slowness)
FROM --platform=$BUILDPLATFORM rust:1.95-slim-bookworm AS preprocessor-runner

RUN apt-get update && apt-get install -y pkg-config libssl-dev curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Download resources (needed for preprocessing)
RUN mkdir -p resources && \
    curl -L https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/resources/mcc_risk.json -o resources/mcc_risk.json && \
    curl -L https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/resources/normalization.json -o resources/normalization.json && \
    curl -L https://raw.githubusercontent.com/zanfranceschi/rinha-de-backend-2026/main/resources/references.json.gz -o resources/references.json.gz

# Build and run the preprocessor natively with SIMD otimizado para a máquina de build
RUN RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin preprocessor

# Stage 2: Compila a API via cross-compilation a partir da plataforma nativa ($BUILDPLATFORM)
# Isso evita o bug de use-after-free em proc_macro do rustc 1.95 no QEMU sob Apple Silicon
FROM --platform=$BUILDPLATFORM rust:1.95-slim-bookworm AS builder

# Instala o compilador C cross-compiler para x86_64 e o target Rust correspondente
RUN apt-get update && apt-get install -y gcc-x86-64-linux-gnu && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-gnu

WORKDIR /app
COPY . .

# Indica ao Cargo qual linker usar para o target x86_64
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc

# Haswell: AVX2, FMA, BMI1/BMI2 — instruções vetoriais para o Mac Mini Intel
RUN RUSTFLAGS="-C target-cpu=haswell -C opt-level=3" cargo build --release --target x86_64-unknown-linux-gnu --bin fraud-detection-api

# Stage 3: Runtime — mesma plataforma do binário compilado
FROM --platform=linux/amd64 debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
# Copia o binário compilado para o target correspondente
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/fraud-detection-api .

# Copy the preprocessed data (shared across all architectures)
COPY --from=preprocessor-runner /app/resources/*.bin ./resources/
COPY --from=preprocessor-runner /app/resources/normalization.json ./resources/
COPY --from=preprocessor-runner /app/resources/mcc_risk.json ./resources/

EXPOSE 9999
CMD ["./fraud-detection-api"]
