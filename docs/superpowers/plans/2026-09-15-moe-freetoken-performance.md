# Plano: alinhar o executor MoE ao caminho FreeToken

> **Para o agente que executar:** use a skill `superpowers:executing-plans` e avance por cada etapa sem pausar para pedir confirmação.

**Objetivo:** eliminar o trabalho repetido e as esperas síncronas que tornam o prefill e o decode do executor MoE CUDA mais lentos que o caminho denso, preservando o cache residente e a divisão existente entre as GPUs.

**Causa inicial já localizada:** o caminho heterogêneo materializa os bytes de todos os tensores de cada expert antes de consultar o cache residente. Em seguida, o backend CUDA aloca e libera todos os buffers, cria eventos e faz `cudaDeviceSynchronize` em cada chamada. Isso diverge do FreeToken, que mantém descritores, pesos e buffers residentes e usa o estado GPU como fonte de dados quente.

## Etapas

1. **[x] Separar metadados de payload no carregamento de experts**
   - Escrever primeiro um teste em `crates/llama-engine/src/moe/format_heterogeneous.rs` que obtenha apenas as especificações de tensor e compare a carga explícita posterior.
   - Extrair uma API de especificações sem cópia do mmap e fazer `block.rs` consultar o cache antes de materializar bytes.
   - Rodar o teste direcionado e o conjunto de testes do crate.

2. **[x] Tornar persistente o caminho CUDA heterogêneo**
   - Escrever uma verificação direcionada para o novo handle de pipeline residente.
   - Adicionar ao fork local do backend CUDA customizado um pipeline por device com buffers que crescem sob demanda e eventos reutilizáveis.
   - Remover a sincronização global redundante; a cópia D2H continua sendo a barreira necessária para o resultado.
   - Manter a divisão por ordinal já usada pelo cache residente e serializar somente o pipeline do mesmo device, permitindo execução concorrente nos devices distintos.

3. **[x] Auditar espera e estado entre requisições**
   - Verificar o ciclo de vida do warmup e o reaproveitamento de leases/cache entre prefill e decode.
   - Adicionar ou ajustar testes para garantir que uma segunda chamada não rematerialize payloads nem fique presa ao warmup em andamento.
   - Não alterar os timeouts de transporte sem evidência: a telemetria fornecida mostra fila e setup locais próximos de zero, enquanto a rede domina apenas o caminho distribuído.

4. **[x] Corrigir o caminho efetivo IQ3_XXS**
   - Confirmar que o modelo da telemetria usa dtype GGML 18 e ainda passa pela
     ponte IQ antiga, fora do pipeline heterogêneo otimizado.
   - Reutilizar o workspace CUDA residente no caminho IQ, criar um pipeline por
     shard de device e retirar a sincronização global adicional.
   - Trocar os dot products IQ por reduções cooperativas de 128 threads, sem
     alterar o layout packed nem a ordem numérica do CPU de referência.

5. **Documentar e validar**
   - Atualizar `crates/llama-engine/AGENTS.md` e `docs/subsystems/freetoken-moe.md` com o comportamento entregue.
   - Rodar testes Rust, `cargo build --release --features cuda` com a arquitetura local quando possível e os checks de formatação.
   - Sincronizar novamente com `origin/feat/implantar3`, revisar o diff, commitar e publicar conforme o contrato de entrega.

## Verificação esperada

- O teste de especificações falha antes da API ser implementada e passa depois.
- O teste direcionado do pipeline passa quando a feature CUDA estiver disponível.
- O build completo com `--features cuda` termina sem erro; se a máquina não possuir toolkit CUDA, registrar a limitação sem alegar sucesso.
- A telemetria final deve mostrar que o custo de cópia/alocação por chamada desapareceu ou caiu para crescimento inicial de capacidade, sem introduzir espera de scheduler ou fallback de transporte.
