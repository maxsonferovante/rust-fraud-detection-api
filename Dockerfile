# syntax=docker/dockerfile:1.7

ARG RESOURCES_IMAGE=maxsonferovante/fraud-detection-resources@sha256:58860c360361ba29369fbf5fd553bd7fcd256916a21d69ee7f7e243c5c979471

# Stage 1: Prebuilt resources (specialist.bin + JSONs), pinned by digest for determinism.
FROM --platform=$BUILDPLATFORM ${RESOURCES_IMAGE} AS resources

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

# Copy the preprocessed data from the prebuilt resources image.
COPY --from=resources /resources/specialist.bin ./resources/specialist.bin
COPY --from=resources /resources/normalization.json ./resources/normalization.json
COPY --from=resources /resources/mcc_risk.json ./resources/mcc_risk.json

EXPOSE 9999
CMD ["./fraud-detection-api"]
