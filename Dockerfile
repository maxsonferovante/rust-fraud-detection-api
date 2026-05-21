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

# Build and run the preprocessor natively
RUN cargo run --release --bin preprocessor

# Stage 2: Build the API for the target architecture
FROM rust:1.95-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Build the main API (this will run for each target platform)
RUN cargo build --release

# Stage 3: Runtime
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
# Copy the binary built for the target architecture
COPY --from=builder /app/target/release/fraud-detection-api .

# Copy the preprocessed data (shared across all architectures)
COPY --from=preprocessor-runner /app/resources/*.bin ./resources/
COPY --from=preprocessor-runner /app/resources/normalization.json ./resources/
COPY --from=preprocessor-runner /app/resources/mcc_risk.json ./resources/

EXPOSE 9999
CMD ["./fraud-detection-api"]
