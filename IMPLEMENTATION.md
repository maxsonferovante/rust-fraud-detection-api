# Fraud Detection API (Rinha 2026) — Implementation Notes

Este documento descreve, em detalhe tecnico, como a implementacao atual funciona: arquitetura (LB + 2 APIs), IPC por FD-passing, I/O por epoll, parsing HTTP/JSON manual, normalizacao/quantizacao e a busca IVF compacta em `int16` com scan por paineis de 8 e AVX2/FMA.

O objetivo do projeto e maximizar o `detection_score` (idealmente `E=0`) sem estourar p99 e respeitando o budget de CPU/memoria do desafio.

## 1. Visao Geral

**Componentes:**

1. **LB (load balancer) em Rust** (`src/bin/lb.rs`)
   - Aceita conexoes TCP na porta 9999.
   - Nao faz proxy HTTP: ele recebe a conexao e repassa o *file descriptor* (FD) do socket do cliente para uma das instancias de API via Unix socket, usando `sendmsg(SCM_RIGHTS)`.

2. **API em Rust** (`src/main.rs`)
   - Nao usa Axum/Tokio no runtime.
   - Recebe FDs via Unix socket (`recvmsg(SCM_RIGHTS)`), registra em epoll, le a requisicao HTTP do socket do cliente, parseia JSON, classifica e escreve a resposta **direto no socket do cliente**.

3. **Indice IVF compacto** (`resources/specialist.bin`)
   - Vectors quantizados em `int16` com `scale=10000`.
   - Layout por **paineis SoA de 8 vetores** (gancho para AVX2).
   - Centroids em `f32`.

4. **Preprocessor** (`src/bin/preprocessor.rs`)
   - Gera `resources/specialist.bin` a partir de `resources/references.json.gz` (3M referencias).

5. **Optimizer / avaliador offline** (`src/bin/optimizer.rs`)
   - Mede FP/FN/E e latencia offline contra `test/test-data.json` do repositorio da Rinha, variando `N_PROBES`.

## 2. Contrato HTTP

A API exposta para o avaliador segue o contrato:

- `GET /ready`
  - Retorna `200` com JSON simples preformatado:
    - Body: `{"ok":true,"role":"api"}`
- `POST /fraud-score`
  - Body JSON no formato oficial da Rinha.
  - Retorna `200` com JSON preformatado:
    - `{"approved":true|false,"fraud_score":0.0|0.2|0.4|0.6|0.8|1.0}`

Observacao: respostas sao preformatadas e escolhidas por indice (0..5) para evitar custo de formatacao por request.

## 3. Arquitetura de Rede e FD-Passing

### 3.1. Motivacao

Proxy HTTP (nginx) + servidor HTTP em framework tende a introduzir overhead (parse/alloc/copies). O caminho atual remove o proxy do meio:

- LB aceita TCP.
- LB passa o FD do socket do cliente para a API.
- A API responde direto naquele socket.

O LB nao precisa entender HTTP nem JSON.

### 3.2. Como o FD e transferido

Implementacao em `src/fdpass.rs`:

- `send_fd(sock_fd, fd_to_send)`
  - Usa `sendmsg` com um control message `SCM_RIGHTS` para anexar o FD ao datagrama do Unix socket.
- `recv_fd(sock_fd) -> Option<RawFd>`
  - Usa `recvmsg` e extrai o FD do control message.

Esse mecanismo funciona em Linux (target do desafio). Em dev local (macOS), o runtime pode cair em caminhos alternativos (ver secao de epoll).

## 4. Load Balancer (LB)

Arquivo: `src/bin/lb.rs`

### 4.1. Fluxo

1. `bind_listener(addr, backlog)`
   - No Linux: usa `socket/bind/listen` via `libc` para controlar o backlog.
   - Fora do Linux: fallback para `TcpListener::bind`.
2. `accept_loop`
   - Um unico loop chama `accept()`, seta `TCP_NODELAY`, obtém `client_fd`.
   - Distribui `client_fd` para workers via `std::sync::mpsc::Sender<i32>`.
3. Workers
   - Cada worker abre seus proprios `UnixStream`s para os upstreams (`FD_UPSTREAMS`).
   - Para cada `client_fd`, escolhe upstream via round-robin global (`AtomicUsize`) e chama `send_fd`.
   - Fecha o `client_fd` local apos o envio.

### 4.2. Concurrency model

- Evita "thundering herd": apenas 1 thread faz `accept`.
- Workers nao competem por accept; fazem somente repasse de FD.

### 4.3. Configuracao

- `LB_BIND_ADDR` (default `0.0.0.0:9999`)
- `LB_BACKLOG` (default `4096`)
- `LB_WORKERS` (default `1`)
- `FD_UPSTREAMS` (default `/tmp/sock/api1.sock,/tmp/sock/api2.sock`)

### 4.4. Logs do LB

Somente no startup:

- endereco, workers, backlog e upstreams.

Nao ha logs por request.

## 5. API Runtime: epoll + estado por conexao

Arquivo: `src/main.rs`

### 5.1. Objetivo

Gerenciar muitos sockets com custo baixo:

- Recebe FDs do LB via Unix socket
- Registra FDs em epoll
- Le request HTTP ate completar header + body
- Classifica
- Escreve resposta e fecha FD

### 5.2. Estruturas principais

- `AppState`
  - `vector_store`: indice IVF carregado
  - `normalization_constants`: constantes para features
  - `mcc_table`: tabela O(1) para risco por MCC (0..9999)
  - `n_probes`: probes do IVF
  - `ready_response`: bytes preformatados do `/ready`

- `ClientState` (Linux)
  - `fd`: socket do cliente
  - `buf`: buffer fixo de request (`MAX_HTTP_BYTES`)
  - `filled`: bytes lidos
  - `header_end`: posicao do `\r\n\r\n` se encontrada
  - `header_search_pos`: cursor incremental para achar `\r\n\r\n` sem rescans completos
  - `content_length`
  - `response_kind`: `Ready` ou `Static(&'static [u8])`
  - `write_pos`, `response_ready`, `write_done`, `close_requested`

### 5.3. Buffer fixo

- `MAX_HTTP_BYTES = 16 * 1024`
  - A leitura e feita sem alocar, e requests maiores sao rejeitadas (`400`/drop).
  - Reduz variancia de cache/memory e evita `Vec` resizes no hot path.

### 5.4. epoll e ONESHOT

No Linux a API usa:

- `EPOLLET` (edge-triggered)
- `EPOLLONESHOT` em sockets de cliente e nos Unix sockets

E faz rearm explicito:

- `rearm_read()` apos tentar ler (se ainda nao tem request completa)
- `rearm_write()` / `arm_write()` quando ha resposta pendente
- rearm do Unix socket apos drenar mensagens SCM_RIGHTS

O objetivo do ONESHOT e reduzir wakeups redundantes e estabilizar p99.

### 5.5. Fluxo do loop epoll (alto nivel)

1. epoll recebe evento do `UnixListener`:
   - aceita conexoes de controle (UnixStream) vindas do LB
   - registra esses UnixStream em epoll
2. epoll recebe evento de um UnixStream:
   - chama `recv_fd` repetidamente (drain)
   - para cada `client_fd`:
     - seta nonblocking
     - registra no epoll
     - cria `ClientState` em `clients[fd]`
   - rearma o UnixStream (ONESHOT)
3. epoll recebe evento de um `client_fd`:
   - le bytes no buffer
   - tenta finalizar header/body:
     - encontra `\r\n\r\n` incrementalmente
     - parseia `Content-Length` via scan de bytes (`parse_content_length_fast`)
   - quando request completa:
     - decide rota: `/ready` ou `/fraud-score`
     - prepara resposta (preformatada)
     - arma escrita
   - escreve resposta (write loop ate EAGAIN ou completo)
   - fecha FD quando terminar

### 5.6. Logs da API

Somente no startup:

- sock path, n_probes, index path, buffer size, modo I/O (epoll vs blocking).

Nao ha logs por request.

## 6. Parsing HTTP

Parsing e minimalista, suficiente para:

- identificar `GET /ready` e `POST /fraud-score` via prefix match
- localizar o final do header `\r\n\r\n`
- ler `Content-Length` (quando presente)

Detalhes:

- `find_header_end_from` busca `\r\n\r\n` a partir de um cursor (`header_search_pos`), evitando revarrer o buffer inteiro.
- `parse_content_length_fast` faz scan byte-a-byte por `content-length:` (case-insensitive) e parse decimal sem UTF-8.

## 7. Parsing JSON (manual, zero-copy)

Arquivo: `src/json.rs`

### 7.1. Objetivo

Evitar `serde_json` no hot path:

- sem alocacao de `String`
- sem alocacao de `Vec` na request
- extrair apenas os campos do contrato

### 7.2. Modelo

`parse_transaction(input: &[u8]) -> Option<ParsedTransaction>`

- Retorna `ParsedTransaction` com:
  - floats/ints/bools parseados
  - strings como slices `&[u8]` apontando direto para o buffer do request
  - `known_merchants` em array fixo (cap `MAX_KNOWN_MERCHANTS`)

Observacao importante:

- Escapes em string (`\\u`, `\"`, etc) nao sao suportados. O dataset do desafio nao usa escapes nos campos relevantes.

## 8. Normalizacao + Quantizacao (`int16 scale=10000`)

Arquivo: `src/normalization.rs`

### 8.1. Features (14 dims)

O vetor segue o contrato do desafio (14 features), normalizadas para faixa aproximada [0,1], com sentinelas `-1` em campos ausentes (ex: last_transaction).

### 8.2. MCC table O(1)

No startup:

- `resources/mcc_risk.json` vira `Vec<f32>` de tamanho 10000
- MCC e convertido para indice numerico 0..9999 (fallback 0.5)

### 8.3. Quantizacao

O vetor final e `i16[14]` com `scale=10000`:

- aplica `round4` antes de escalar, para ficar consistente com dados oficiais (4 casas).
- exemplo: `0.12345 -> 1235`

Isso reduz diferenca numerica vs float (ex: f16) e ajuda a manter `E=0`.

## 9. Busca IVF compacta em `specialist.bin`

Arquivo: `src/search.rs`

### 9.1. Formato do arquivo

O indice e um unico arquivo binario. Estrutura (alto nivel):

1. Header (u32 LE):
   - magic: `b"RIVF"`
   - version
   - n_vectors
   - dim (14)
   - n_clusters (K)
   - scale (10000)
   - reserved...
2. Centroids: `K * 14 * f32`
3. cluster_sizes: `K * u32`
4. cluster_offsets: `(K+1) * u32` (offset de labels por cluster)
5. panel_offsets: `(K+1) * u32` (offset em unidades de i16 no array de vetores)
6. vectors_soa: `i16` em layout SoA por painéis
7. labels: `n_vectors * u8` (0 legit / 1 fraud)

### 9.2. Layout SoA por painéis

Para cada cluster:

- Paineis completos de 8 vetores:
  - para cada dim 0..13, grava 8 lanes `i16` contiguos
- Cauda (resto < 8):
  - grava AoS (vetor por vetor) em `i16[14]`

### 9.3. Algoritmo de busca

Entrada: `query: i16[14]`, `n_probes`.

1. Distancia para todos centroids (em f32, convertendo query com `inv_scale`)
2. Seleciona top `n_probes` clusters (buffer na stack)
3. Ordena probes por cluster_size (pequenos primeiro) para melhorar p99
4. Scaneia cada cluster:
   - paineis: AVX2/FMA (quando disponivel) computa 8 distancias por vez
   - cauda: escalar
5. Mantem top-5 vizinhos com buffer fixo em stack (`TopK<5>`)
6. Retorna `frauds = count(label==1)` em top-5

Decisao final:

- `fraud_score = frauds / 5.0`
- `approved = fraud_score < 0.6`

## 10. Preprocessor

Arquivo: `src/bin/preprocessor.rs`

### 10.1. Pipeline

1. Le `resources/references.json.gz` (3M vetores)
2. Faz K-means com `K=4096` para obter centroids
3. Atribui cada vetor ao cluster mais proximo
4. Serializa o indice em `resources/specialist.bin` no formato acima

Observacao:

- K-means em 3M x 4096 e caro. Por isso, o Docker build foi ajustado para cachear o maximo possivel.

## 11. Optimizer / avaliador offline

Arquivo: `src/bin/optimizer.rs`

Ele:

- Carrega `test/test-data.json`
- Normaliza, roda busca (`fraud_count_nearest_i16`)
- Computa TP/TN/FP/FN, `E = FP + 3*FN`
- Ajuda a escolher `N_PROBES` (ex: 96 foi um ponto com `E=0`)

## 12. Docker build: cache do preprocessor

Arquivo: `Dockerfile`

Principio:

- O stage do preprocessor copia apenas arquivos necessarios para compilar/executar o preprocessor.
- O `cargo build --release --bin preprocessor` e separado de `./target/release/preprocessor`.
- Cache do cargo registry e usado via BuildKit mounts.
- O stage `builder` faz um "warm build" com stubs para cachear dependencias.

O objetivo e que mudancas em API/LB nao forçem rerun do preprocessor (a parte mais cara).

## 13. docker-compose e limites

Arquivo: `docker-compose.yml`

- 1 LB
- 2 APIs
- volume tmpfs para `/tmp/sock`
- CPU/memoria dentro do budget total (ex: 0.16 + 0.42 + 0.42 CPU e 30MB + 160MB + 160MB RAM).

## 14. Knobs e dicas de tuning

- `N_PROBES`:
  - influencia recall (E) vs p99.
  - offline, `96` atingiu `E=0` e ficou estavel nos testes.
- `LB_WORKERS`:
  - default 1 para evitar overhead com CPU limitada do LB.
- `LB_BACKLOG`:
  - pode ajudar em bursts.
- `MAX_HTTP_BYTES`:
  - reduzir melhora locality; deve ser suficiente para o payload do desafio.

## 15. Observacoes de portabilidade

- epoll e `EPOLLONESHOT` sao Linux-only; no macOS ha fallback blocking.
- target do desafio e Linux/amd64 com AVX2/FMA (o build usa `-C target-cpu=haswell`).

