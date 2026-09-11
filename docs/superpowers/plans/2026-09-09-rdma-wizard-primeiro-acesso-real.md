# Wizard real de primeiro acesso do POM - plano de implementação

> **Para agentes:** use TDD por tarefa. Este plano cobre o wizard real servido
> quando `pom.json` ainda não existe, incluindo a entrada pública sem segredo.

**Objetivo:** substituir o formulário curto de `Setup.tsx` por um wizard real,
com hardware detectado, configuração de recursos, modelos locais e referências
do Hugging Face, armazenamento, rede administrativa, raiz de confiança e
reinício real do nó.

**Arquitetura:** o backend de setup continua separado do gateway configurado e
expõe apenas `/api/setup/*`. A UI mantém um estado central do percurso e usa o
layout e os cartões do protótipo como linguagem visual. O administrador gera e
persiste uma cadeia de confiança assinada pelo próprio nó. Em redes públicas,
as mutações de controle carregam um envelope Ed25519 assinado pela cadeia e a
inferência permanece fora dessa assinatura; redes privadas conservam a
connection key como segredo compartilhado adicional.

**Stack:** Rust/Axum, configuração JSON existente, `admin_trust`, `sysmon`,
React 18, TypeScript, Tailwind, Framer Motion, Vitest e Testing Library.

**Restrições:**

- Não alterar o protocolo P2P, o protocolo de inferência, llama.cpp ou
  `FIT_MAX_UBATCH`. A camada de controle assinada é HTTP administrativo e não
  altera os quadros de inferência.
- Todo texto da interface passa por `useI18n`, com paridade `en` e `pt-BR`.
- Usar as pastas padrão `~/.pom/models` e `~/.pom/models_sliced`.
- GPU fica sempre em capacidade máxima; somente RAM, CPU e disco são ajustáveis.
- A verificação final cobre GPU, CUDA, configuração, descoberta P2P e IPv6;
  modelo e inferência ficam fora do wizard.
- Não editar arquivos gerados em `apps/frontend/src/docs`.

## Tarefas

### 1. Contrato de configuração e API de setup

- Adicionar ao `Config` as preferências persistentes de rede, reserva de RAM e
  CPU, diretórios de modelos, `shard_max_gb` e modelo escolhido.
- Adicionar respostas tipadas para hardware real, catálogo local mais modelos
  de referência do Hugging Face, estado da âncora, prévia da âncora e checks.
- Reusar `admin_trust` para criar a cadeia primária local e para instalar a
  cadeia recebida, sem duplicar validação de assinatura ou fingerprint.
- Adicionar testes Axum para hardware, prévia e persistência administrativa.

### 2. UI do wizard real

- Reescrever `Setup.tsx` usando o fluxo de uma tela por vez e o layout/cartões
  existentes.
- Buscar hardware e modelos ao entrar, com loading, erro recuperável e dados
  reais do nó.
- Implementar recursos, storage, modelos, rede administrativa, convite com
  documento de confiança e chave pública, health reduzido e salvamento.
- Deixar node/infer na mesma experiência até a tela de confiança, aceitando
  raiz + impressão digital digitada e escolhendo entre rede pública sem segredo
  e rede privada com um segredo compartilhado.

### 3. Redes públicas

- Expor `/api/public/network` e `/api/public/networks` sem dados sensíveis;
  deduplicar registros por fingerprint usando a tabela de peers alimentada pela
  descoberta pkarr existente, sem alterar o wire format P2P.
- Adicionar `PublicNetworks.tsx` como item de topo em `/redes-publicas`, com
  nome, fingerprint, endereço e caminho para o fluxo de raiz de confiança.

### 4. Tradução, documentação e testes

- Adicionar todas as chaves en/pt-BR e atualizar o teste de paridade.
- Atualizar os AGENTS e `docs/subsystems/web-ui.md` com a rota real, endpoints
  de setup e separação em relação a `SetupWizard.tsx`.
- Rodar testes frontend, build frontend, testes Rust relevantes e build release.

### 5. Ciclo WIP local

- Fazer commits WIP na branch `fm/pom-wizard-primeiro-acesso`.
- Antes de cada rodada do captain, compilar frontend e backend, limpar apenas
  o estado permitido no checkout local do captain, subir o binário e registrar
  `needs-decision` com a chave da rodada.
