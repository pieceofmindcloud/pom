# Plano: visão no POM quando há mmproj

## Objetivo

Descobrir e persistir o projector GGUF associado a cada modelo, expor a
decisão na implantação e permitir que o Chat envie imagens somente para uma
implantação carregada com visão. Ficheiros de texto continuam a ser aceites
em qualquer implantação.

## Desenho

1. **Catálogo**: procurar somente irmãos `.gguf` no mesmo diretório cujo nome
   contenha `mmproj`, seguindo a seleção de sibling do llama.cpp. Guardar o
   caminho escolhido em `DiscoveredModel`, sem incluir o projector como modelo
   selecionável. Cobrir presença, ausência e escolha determinística.
2. **Implantação**: transportar o projector selecionado na planificação e no
   `NodeConfig`; a opção de visão só aparece quando o plano tem projector.
   Com a opção ligada, o loader em processo recebe o caminho do projector e o
   estado publicado pelo nó e pelo cluster informa `mmproj_path` na cabeça
   ativa.
3. **Inferência e Chat**: habilitar a ponte `mtmd` já vendorizada, carregar o
   projector junto do GGUF e transportar a imagem somente no prefill da
   cabeça. O backend recusa imagem quando a cabeça selecionada não publica
   visão. O navegador lê texto sempre, redimensiona imagens preservando
   proporção para no máximo 2048 px no lado maior antes de enviá-las e aceita
   botão ou arrastar e largar com retorno visual, miniatura e remoção antes do
   envio.
4. **Documentação e verificação**: atualizar os mapas dos subsistemas tocados,
   rodar testes focados para detecção, recusa e miniaturização, build/testes
   completos e comparar a lista de falhas com os sete vermelhos capturados
   antes da implementação.

## Limites

- O checkout vendorizado de llama.cpp não será alterado.
- `FIT_MAX_UBATCH` permanece em 512.
- Não haverá sessão de revisão visual nesta tarefa.
