# Plano: transporte QUIC opcional no hop de inferência

> Execução autônoma neste worktree. A política de transporte será adicionada
> sem alterar o motor de inferência nem o transporte libp2p.

## Objetivo

Adicionar uma preferência `quic_enabled`, controlada pelo principal e
propagada de forma autenticada, para que o hop HTTP da inferência e o canal de
retorno possam negociar HTTP/3 sobre QUIC. O comportamento padrão continua
HTTP/1.1 puro quando TLS está desligado e HTTPS quando TLS está ligado.

## Tarefas

- [x] Escrever testes vermelhos para a configuração, anúncio e política
  principal -> peer, incluindo default-off, autenticação e mudança sem fechar
  clientes ativos.
- [x] Implementar a política de rede e a persistência da preferência, com
  endpoints administrativos e anúncio explícito no cluster.
- [x] Adicionar a camada HTTP/3 sobre `quinn`, incluindo o endpoint UDP,
  certificados pinados dos nós, cliente persistente e adaptador das rotas de
  inferência.
- [x] Escrever testes vermelhos para sucesso QUIC e fallback para HTTP/HTTPS;
  integrar a seleção no envio em lote, no envio direto e no canal de retorno,
  preservando o link persistente quando QUIC está desligado.
- [x] Corrigir a evidência de transporte para distinguir `http`, `https` e
  `quic`, mantendo fallback observável por hop e sem interromper sessões já
  abertas.
- [x] Adicionar a preferência somente para o principal na tela Preferences,
  estado read-only para peers, tipos, mensagens e testes de frontend.
- [x] Atualizar os AGENTS/READMEs de subsistema afetados em pt-BR e rodar a
  suíte direcionada, build do frontend e validações completas antes do commit.

## Critérios de verificação

- A configuração ausente resulta em QUIC desligado.
- Um peer só adota a preferência após busca autenticada no principal; anúncio
  UDP/gossip não é fonte de verdade.
- Falha de negociação, ausência de suporte ou indisponibilidade do endpoint
  QUIC retorna ao esquema TCP atual sem perder a requisição.
- A telemetria registra o transporte realmente usado e indica fallback.
- Trocar a política não fecha nem invalida conexões em uso.
- O branch final contém apenas o commit local da implementação.
