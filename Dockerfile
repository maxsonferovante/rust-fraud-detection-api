# Stage 1: Build
FROM rust:1.95-slim-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .

# Build the preprocessor and process the data (IVF)
RUN cargo run --release --bin preprocessor

# Build the main API
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y curl && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/fraud-detection-api .
COPY --from=builder /app/resources/centroids.bin ./resources/
COPY --from=builder /app/resources/ivf_vectors.bin ./resources/
COPY --from=builder /app/resources/ivf_labels.bin ./resources/
COPY --from=builder /app/resources/ivf_offsets.bin ./resources/
COPY resources/normalization.json ./resources/
COPY resources/mcc_risk.json ./resources/

EXPOSE 9999
CMD ["./fraud-detection-api"]
