# Plano: F5-E2 e cache de experts residente em VRAM

> Execução autorizada pelo captain em 2026-08-25. O F5-E2 do loader já está
> presente no fork publicado em `03902a5ba`; esta execução verifica o contrato
> e entrega a residência do cache que ainda falta.

## Objetivo

Quando o caminho MoE opt-in estiver ativo, o llama.cpp mantém apenas metadata
dos experts roteados e o cache próprio mantém os payloads quentes no device que
executa o lote. Um hit de cache não faz H2D de peso. O caminho denso e o MoE
CPU continuam usando suas implementações atuais.

## Sequência

1. **Contrato F5-E2**
   - verificar no fork a classificação genérica por `LLM_TENSOR_*EXP`, o
     contexto `no_alloc`, a preservação de nome/rank/forma/dtype e a ausência
     do opt-in no caminho nativo;
   - manter a documentação do fork e do subsistema alinhadas.
2. **Teste red da residência**
   - criar um `DeviceAllocation`/`DeviceAllocator` abstrato, com sonda de
     memória livre e handles opacos, sem qualquer ramo por arquitetura ou
     dtype;
   - testar com allocator falso: primeiro acesso faz uma admissão/upload,
     segundo acesso retorna o mesmo handle sem novo upload, eviction respeita
     lease ativo e orçamento medido recusa nominalmente quando não cabe.
3. **Cache de device em Rust**
   - separar a LRU quente do payload frio em host;
   - usar `MemoryAccount` contra a memória livre medida no device;
   - publicar estatísticas de hits, misses, admissões, evictions, bytes
     residentes e uploads.
4. **Ponte CUDA NVFP4 sobre a abstração**
   - implementar upload/free de payload opaco e sonda `cudaMemGetInfo`;
   - adaptar o lote NVFP4 para receber ponteiros residentes e transferir
     apenas hidden, rotas e resultados;
   - preservar a prova numérica e recusar qualquer erro CUDA nominalmente.
5. **Validação e publicação**
   - rodar testes Rust locais e a prova F5-E2 disponível;
   - publicar primeiro `fm/ftk-cuda-check`, avisar no status antes da build
     remota, e compilar na ksia com `--features vulkan,cuda` sem deploy ou
     reinício;
   - só com build verde promover o commit Rust para `feat/ftk`; o fork só
     recebe novo commit se a ABI CUDA exigir alteração nele.

## Critérios de aceite

- F5-E2: em modo externo, os experts roteados não aparecem em buffers de
  modelo; em modo denso/native, o carregamento não muda.
- Cache: para uma chave e um device, `uploads == 1` após aquecimento e
  `uploads == 1` em qualquer quantidade de hits; H2D de payload de peso é zero
  nos hits. O handle não é liberado enquanto houver lease ou operação pendente.
- Orçamento: nenhuma capacidade fixa de VRAM; a decisão usa a sonda real e
  erro de admissão nomeia device, bytes pedidos e orçamento.
- Generalidade: a LRU não conhece Gemma, NVFP4 ou número de experts. O backend
  CUDA é apenas uma implementação de allocator/handle; outros dtypes podem
  reutilizar a mesma fronteira quando tiverem kernel próprio.
- Sem medição de tok/s nesta etapa: a ksia está servindo o captain. O ganho
  será medido depois que ele liberar o estado servido.
