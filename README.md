# pom

> Architecture note: this repository is a Cargo workspace. The shipped
> `pom` binary is the `node` package in `crates/node`; public routing
> crates live in `crates/routing-contract` and `crates/routing-base`. The
> contract extraction is complete, but runtime provider selection remains on
> the existing node planner while equivalence coverage is expanded. A future
> enterprise provider is a separate private local plugin, never a dependency of
> this repository. See `docs/architecture/routing-plugin-contract.md`.

**POM - Piece Of Mind: inferência distribuída de LLM entre máquinas comuns, sem servidor central**

Cada nó aloca uma fatia de RAM, calcula automaticamente quantas camadas do modelo cabem, e se coordena com outros nós via UDP P2P para cobrir o modelo completo.

---

## Instalação

```bash
# Requer Rust 1.75+
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

git clone <repo>
cd pom
cargo build --release
```

O binário estará em `./target/release/pom`.

---

## Docker

### Build

```bash
./scripts/build-docker.sh
# Ksia:
./scripts/build-docker-ksia.sh
```

A imagem compila CUDA+Vulkan, Vulkan e CPU, além de `pom-plugin-host`; o
frontend é um estágio independente. Os builds de desenvolvimento usam por
padrão as bibliotecas da release exata do fork `pereirajair/llama.cpp`, CUDA
13.2.1, para evitar recompilar o núcleo nativo. A release correspondente deve
estar publicada antes do build; `POM_LLAMA_CPP_RELEASE_TAG` pode fixar uma tag
específica. Para compilar o fork local, inclusive uma cópia
fora do POM ou com alterações não commitadas:

```bash
POM_LLAMA_CPP_MODE=source \
POM_LLAMA_CPP_SOURCE_DIR=/caminho/para/llama.cpp \
  ./scripts/build-docker.sh
```

`build-docker-ksia.sh` aceita os mesmos overrides. O script lê o commit do POM
no host e o passa apenas ao runtime como `POM_BUILD_COMMIT`; o `.dockerignore`
corta `.git/` do contexto, então sem isso o binário carimba `desconhecido` em
`/api/ui/node`. A publicação de binários do POM não depende desse modo Docker.

### Executar

```bash
# Nó simples com models montado em volume
docker run --rm \
    -p 8080:8080 -p 42777:42777/udp -p 8181:8181 \
    -v ./models:/models \
    pom --model /models/meu-modelo.gguf \
             --gateway --gateway-port 8080 --inference-port 8181

# Nó conectando a bootstrap
docker run --rm \
    -p 8080:8080 -p 42778:42778/udp -p 8182:8182 \
    -v ./models:/models \
    pom --model /models/shard_16_31.gguf \
             --port 42778 --bootstrap 192.168.1.10:42777 \
             --gateway --gateway-port 8080 --inference-port 8182
```

A interface web fica disponível em `http://localhost:8080/` assim que o contêiner sobe.

---

## Uso

```bash
# Ver modelos suportados
./pom --list-models

# Iniciar um nó com 10GB para o modelo gemma3
./pom --model gemma3 --memory 10GB

# Nó com porta customizada e bootstrap
./pom --model gemma3 --memory 4GB --port 42777 --bootstrap 192.168.1.10:42777

# Nó conectando a bootstrap remoto (internet)
./pom --model llama3:70b --memory 16GB --bootstrap 45.33.32.156:42777
```

---

## Como funciona

### 1. Descoberta de camadas
Ao iniciar, o nó:
1. Envia `PeerRequest` para bootstrap nodes
2. Faz broadcast UDP local (mDNS-style)
3. Aguarda 2 segundos para coletar respostas
4. Analisa quais camadas já estão cobertas na rede

### 2. Cálculo de assignment
```
Memória disponível = --memory - overhead do modelo (~10-20%)
Camadas que cabem  = memória disponível / MB por camada
Início             = primeiro gap não coberto pelos peers
```

**Exemplo com gemma3 (28 camadas, 180MB/camada):**
```
10GB disponível → ~9.5GB usável → ~52 camadas caberiam
Mas gemma3 só tem 28 → pega todas as 28

4GB disponível → ~3.8GB usável → ~21 camadas
Peers cobrem 0-10 → este nó pega 11-21 (próximo gap)
```

### 3. Gossip P2P
Cada 5 segundos o nó:
- Anuncia suas camadas para todos os peers conhecidos
- Broadcast local para novos nós na LAN
- Remove peers que não respondem há 30s
- Troca lista de peers (gossip Kademlia-style)

---

## Estrutura do workspace

```
Cargo.toml                     # workspace e dependências compartilhadas
crates/
├── node/                      # pacote que produz o binário pom
│   ├── src/main.rs             # CLI e orquestração de inicialização
│   ├── src/models/             # catálogo, GGUF, perfis e fatiamento
│   ├── src/network/            # descoberta, identidade e transporte P2P
│   ├── src/routing/            # planejador de pipeline atual
│   ├── src/backend/            # gateway HTTP, API e interface administrativa
│   └── src/utils/              # configuração, TUI e monitoramento
├── routing-contract/           # contrato serializável e versionado de roteamento
├── routing-base/               # planejador determinístico independente de infraestrutura
├── config/                     # ponto de extração para esquemas de configuração públicos
└── types/                      # ponto de extração para tipos de domínio públicos
apps/frontend/                  # fonte React/Vite; build lido do disco pelo node em runtime
old/                            # ferramentas Python e scripts legados, fora do runtime suportado
```

`cargo build`, `cargo test` e `cargo run --bin pom -- <argumentos>` na
raiz usam `crates/node` como membro padrão do workspace. Para validar todos os
crates públicos e o nó, use `cargo test --workspace`.

### Protocolo de mensagens (UDP JSON)

```json
// Announce - nó anuncia suas camadas
{ "type": "Announce", "node_id": "abc...", "model": "gemma3",
  "start_layer": 11, "end_layer": 21, "memory_mb": 3840 }

// PeerRequest - pede lista de peers
{ "type": "PeerRequest", "from": "abc..." }

// PeerList - responde com peers conhecidos
{ "type": "PeerList", "peers": [...] }

// Heartbeat - sinal de vida
{ "type": "Heartbeat", "node_id": "abc...", "timestamp": "..." }

// LocalDiscover - broadcast LAN
{ "type": "LocalDiscover", "from": "abc...", "port": 42777 }
```

---

## Modelos suportados

| Model           | Layers | MB/layer | Description          |
|-----------------|--------|----------|----------------------|
| gemma3          | 28     | 180      | Google Gemma 3 2B    |
| gemma3:7b       | 32     | 420      | Google Gemma 3 7B    |
| gemma3:27b      | 46     | 1100     | Google Gemma 3 27B   |
| llama3          | 32     | 350      | Meta LLaMA 3 8B      |
| llama3:70b      | 80     | 1600     | Meta LLaMA 3 70B     |
| mistral         | 32     | 320      | Mistral 7B           |
| qwen2.5         | 28     | 290      | Qwen 2.5 7B          |
| deepseek-r1     | 64     | 980      | DeepSeek R1 32B      |

---

## Exemplo de rede distribuída

```
Máquina A (10GB) → gemma3 layers 0-27   (cobre tudo sozinha)
Máquina A (3GB)  → gemma3 layers 0-14
Máquina B (3GB)  → gemma3 layers 15-27  (detecta gap, pega a segunda metade)
Máquina C (2GB)  → gemma3 layers 0-9    (já coberto, fica como redundância)
```

---

## Roadmap

- [ ] WireGuard mesh automático entre peers
- [ ] Capability tokens para controle de acesso à memória
- [ ] QUIC como transporte (substituir UDP raw)
- [ ] Suporte a mais modelos (auto-detect via manifest Ollama)
- [ ] DHT Kademlia completo via libp2p

---

## Segurança (próxima fase)

Atualmente os nós se comunicam sem criptografia (adequado para LAN confiável).

Para redes públicas, o plano é:
1. **WireGuard mesh** - cada node_id mapeia para uma chave WireGuard
2. **Capability tokens** - Ed25519 assinados para acesso a regiões de memória
3. **Canal separado** - controle (TLS) vs dados (QUIC + ChaCha20)

---

## Dependências

```toml
tokio       = "1"        # async runtime
clap        = "4"        # CLI
serde_json  = "1"        # protocolo P2P
ed25519-dalek = "2"      # identidade criptográfica
uuid        = "1"        # IDs únicos
sha2        = "0.10"     # hash de chaves públicas
chrono      = "0.4"      # timestamps
tracing     = "0.1"      # logs estruturados
anyhow      = "1"        # error handling
```
