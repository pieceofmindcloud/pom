# rdma-p2p

**P2P Distributed LLM Inference with RDMA-sim layer**

Cada nó aloca uma fatia de RAM, calcula automaticamente quantas camadas do modelo cabem, e se coordena com outros nós via UDP P2P para cobrir o modelo completo.

---

## Instalação

```bash
# Requer Rust 1.75+
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

git clone <repo>
cd rdma-p2p
cargo build --release
```

O binário estará em `./target/release/rdma-p2p`.

---

## Uso

```bash
# Ver modelos suportados
./rdma-p2p --list-models

# Iniciar um nó com 10GB para o modelo gemma3
./rdma-p2p --model gemma3 --memory 10GB

# Nó com porta customizada e bootstrap
./rdma-p2p --model gemma3 --memory 4GB --port 42777 --bootstrap 192.168.1.10:42777

# Nó conectando a bootstrap remoto (internet)
./rdma-p2p --model llama3:70b --memory 16GB --bootstrap 45.33.32.156:42777
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

## Arquitetura dos módulos

```
src/
├── main.rs       # CLI, startup, display loop
├── model.rs      # Perfis de modelos, cálculo de camadas
├── network.rs    # UDP P2P, gossip, peer discovery
├── identity.rs   # Ed25519 keypair, Node ID
└── display.rs    # Terminal UI, layer map, peer table
```

### Protocolo de mensagens (UDP JSON)

```json
// Announce — nó anuncia suas camadas
{ "type": "Announce", "node_id": "abc...", "model": "gemma3",
  "start_layer": 11, "end_layer": 21, "memory_mb": 3840 }

// PeerRequest — pede lista de peers
{ "type": "PeerRequest", "from": "abc..." }

// PeerList — responde com peers conhecidos
{ "type": "PeerList", "peers": [...] }

// Heartbeat — sinal de vida
{ "type": "Heartbeat", "node_id": "abc...", "timestamp": "..." }

// LocalDiscover — broadcast LAN
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
- [ ] Interface de inferência — encaminha tensores entre nós
- [ ] Suporte a mais modelos (auto-detect via manifest Ollama)
- [ ] DHT Kademlia completo via libp2p
- [ ] Dashboard web (HTTP API no nó)

---

## Segurança (próxima fase)

Atualmente os nós se comunicam sem criptografia (adequado para LAN confiável).

Para redes públicas, o plano é:
1. **WireGuard mesh** — cada node_id mapeia para uma chave WireGuard
2. **Capability tokens** — Ed25519 assinados para acesso a regiões de memória
3. **Canal separado** — controle (TLS) vs dados (QUIC + ChaCha20)

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
