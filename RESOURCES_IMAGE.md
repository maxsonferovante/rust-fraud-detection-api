# Tatica: Imagem de Resources Preprocessados (sem rodar `preprocessor` no CI)

Este documento explica a estrategia usada neste repo para **remover o custo e a nao-deterministicidade** do `preprocessor` durante o build no GitHub Actions, sem precisar commitar arquivos grandes (`.bin`) no git.

## Contexto

O runtime depende de recursos preprocessados:

- `resources/specialist.bin` (indice IVF compactado)
- `resources/normalization.json`
- `resources/mcc_risk.json`

Historicamente, o `Dockerfile` rodava:

1. download de `references.json.gz` e JSONs oficiais
2. build e execucao do binario `preprocessor`
3. `COPY` dos artefatos gerados para a imagem final

Isso tem problemas:

- **tempo**: o `preprocessor` eh pesado e deixa o build lento
- **network**: o build passa a depender de download de arquivos durante o Docker build
- **nao-determinismo**: o `preprocessor` usa RNG (`thread_rng`) na inicializacao dos centroids, entao rodar no CI pode gerar `specialist.bin` diferente a cada build, alterando ANN/recall e potencialmente o `E`
- **limites do GitHub**: nao eh desejavel versionar binarios grandes no repo, mesmo quando cabem no limite por arquivo

## Solucao (visao geral)

Separar em 2 imagens:

1. **Imagem de resources** (data-only):
   - `maxsonferovante/fraud-detection-resources:<tag>`
   - base `scratch`, sem `RUN`, contendo somente os 3 arquivos em `/resources/*`
2. **Imagem de runtime** (API/LB):
   - build normal (compila `fraud-detection-api` e `lb`)
   - consome resources com `COPY --from=<resources-image> /resources/...`

E, para garantir reproducibilidade:

- o `Dockerfile` principal referencia a imagem de resources por **digest**:
  - `ARG RESOURCES_IMAGE=maxsonferovante/fraud-detection-resources@sha256:...`

## Arquivos envolvidos

- `Dockerfile.resources`
  - define a imagem minima (scratch) com os artefatos.
- `scripts/publish_resources_image.sh`
  - automatiza: baixar resources oficiais (se faltarem), rodar `preprocessor`, empacotar e publicar a imagem, e por fim atualizar o `Dockerfile` com o digest.
- `Dockerfile`
  - nao roda mais `preprocessor`.
  - consome os arquivos por `COPY --from=resources ...`.
- `docker-compose.yml`
  - define `N_PROBES=192` para `api1` e `api2`.

## Como gerar e publicar a imagem de resources (local)

Requisitos:

- `docker` (Docker Desktop) rodando
- `docker buildx` habilitado
- login no Docker Hub (`docker login`)
- `cargo` instalado

Comando:

```bash
./scripts/publish_resources_image.sh
```

Opcionalmente, passe uma tag:

```bash
./scripts/publish_resources_image.sh 30-05-2026-12-00-00
```

O script faz:

1. garante que existam em `resources/`:
   - `references.json.gz`
   - `normalization.json`
   - `mcc_risk.json`
2. roda o `preprocessor` com SIMD nativo:
   - `RUSTFLAGS="-C target-cpu=native -C opt-level=3" cargo run --release --bin preprocessor`
3. cria um build context temporario com os 3 arquivos
4. publica multi-arch:
   - `docker buildx build --platform linux/amd64,linux/arm64 --push ...`
5. resolve o digest do manifest e atualiza:
   - a linha `ARG RESOURCES_IMAGE=...` no `Dockerfile`

Importante:

- depois de rodar o script, faca commit do `Dockerfile` (ele muda para ficar pinned no digest).

## Como o Dockerfile principal consome os resources

No `Dockerfile`:

- existe um `ARG RESOURCES_IMAGE=...`
- existe um stage:
  - `FROM --platform=$BUILDPLATFORM ${RESOURCES_IMAGE} AS resources`
- no stage final (`runtime`) sao copiados os arquivos:
  - `COPY --from=resources /resources/specialist.bin ./resources/specialist.bin`
  - `COPY --from=resources /resources/normalization.json ./resources/normalization.json`
  - `COPY --from=resources /resources/mcc_risk.json ./resources/mcc_risk.json`

Com isso:

- o build do CI **nao roda** `preprocessor`
- o build fica mais rapido e previsivel
- o `specialist.bin` passa a ser uma dependencia imutavel (por digest)

## Tuning: `N_PROBES=192`

O runtime le `N_PROBES` do env e faz fallback para `192` quando nao existe.

Mesmo assim, fixamos no `docker-compose.yml`:

- `api1`: `N_PROBES=36`
- `api2`: `N_PROBES=36`

Motivo: deixar explicito e evitar drift entre ambientes.

## Fluxo recomendado de atualizacao (para submeter na Rinha)

1. Atualize codigo normalmente na `master`.
2. Quando quiser trocar o indice:
   1. rode `./scripts/publish_resources_image.sh <tag>`
   2. commite a mudanca do `ARG RESOURCES_IMAGE=...@sha256:...` no `Dockerfile`
3. Abra PR `master -> submission`.
4. O GitHub Actions vai:
   - buildar a imagem do runtime (sem preprocessor)
   - atualizar a branch `submission` com o `docker-compose.yml` apontando para a nova imagem

## Armadilhas comuns

- Se a imagem de resources nao estiver pinned por digest, voce pode ter drift (tag mudou).
- Se o Docker local nao estiver com permissao para acessar o daemon, o script falha.
- Rodar o `preprocessor` em maquinas diferentes pode gerar indices diferentes (RNG); por isso a recomendacao eh sempre publicar a imagem e pin por digest, e nao re-gerar no CI.
