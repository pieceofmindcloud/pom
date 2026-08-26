# Goal FreeToken e caminho MoE - design

## Contexto

O estudo do FreeToken separa políticas reaproveitáveis de um motor GPU próprio.
Este goal adota explicitamente o motor próprio para MoE, mas mantém o caminho
denso atual no `llama.cpp`. A mudança é arquitetural porque adiciona uma
segunda implementação de inferência, um orçamento de memória por modelo e um
cache global de experts.

## Decisões

1. O caminho é selecionado por metadados GGUF. A ausência de metadados MoE é
   tratada como denso, nunca como MoE.
2. O caminho denso não muda de engine. O fallback de configuração do caminho
   MoE é explícito e reversível, nunca uma queda silenciosa para o motor novo.
3. O orçamento de KV é uma política pura em bytes, separada da leitura de
   device. A aritmética recusa planos que não cabem antes de escrever fatias.
4. Reconfiguração de contexto preserva o objeto de pesos carregado, fecha a
   geração antiga em uma porta de manutenção e só publica a nova depois de
   `/health` confirmar carga.
5. O motor MoE começa com um formato mínimo de quantização, escolhido após
   verificar um modelo MoE real disponível localmente. Cache LRU, admissão e
   execução são testados sem depender de uma GPU específica sempre que a
   fronteira permitir.
6. Co-execução CPU/GPU só é habilitada depois de medir banda local e armazenar
   um perfil ligado à máquina, ao backend e ao formato de quantização.

## Componentes

- `models/gguf.rs`: leitura de arquitetura e geometria MoE/KV sem tocar nos
  pesos.
- `models/kv_budget.rs`: bytes por token e orçamento de uma fatia, com
  overflow explícito e testes puros.
- `inference/runtime.rs` e `infer_server.rs`: ciclo de vida de pesos, contexto,
  manutenção, cancelamento e seleção de engine.
- `inference/utf8.rs` ou o `Utf8Assembler` já ligado pelo ramo upstream:
  saída incremental e retenção de prefixos de stop strings.
- `inference/moe/`: executor próprio, cache LRU global, política de admissão,
  métricas e primeiro formato de quantização suportado.
- `docs/subsystems/freetoken-moe.md`: método, números e limites medidos.

## Fluxo de dados

O gateway valida orçamento e cria uma solicitação cancelável. O runtime escolhe
o engine depois de ler a arquitetura do GGUF. Denso vai ao `LlmEngine` atual;
MoE vai ao executor MoE quando a configuração permitir. O estado de sessão e a
resposta seguem os contratos atuais até uma mudança de fio ser indispensável.
Uma mudança de contrato de API ou fio exige decisão superior antes de ser
implementada.

## Verificação

Cada fase começa com teste que falha e termina com teste unitário, teste de
integração aplicável e `cargo test` do workspace. O caminho denso recebe uma
regressão explícita a cada bifurcação. O cache de prefixo, se aprovado pela
medição, exige teste diferencial contra uma referência e um autoteste com proxy
propositalmente corrompido.

## Fora de escopo

- copiar código do FreeToken;
- substituir o `llama.cpp` para modelos densos;
- transportar pesos pela rede por causa do cache ou do fatiamento;
- implementar todos os formatos de quantização de uma vez;
- deploy, SSH ou uso do cluster remoto para obter números.
