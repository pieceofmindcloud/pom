# Enterprise completo: design de cliente e plugin

## Objetivo

Fechar o ciclo do Enterprise no cliente POM: ativação por chave, lease e
heartbeat, download verificado, host auxiliar, plugin `cdylib` com banco
próprio, consultas de auditoria, propagação autenticada entre peers, UI de
ativação e artefato reproduzível para publicação no servidor de licenças.

O servidor existente em `pereirajair/pom-website`, commit `60b79a3c`, é a
autoridade do wire protocol. O POM não implementa uma cópia do servidor.

## Arquitetura

O processo `pom` contém somente o cliente de licença, o supervisor e um
`EnterpriseBridge`. A bridge fala JSON com tamanho prefixado com
`pom-plugin-host`; somente o host carrega o `cdylib`. O plugin recebe eventos
neutros e consultas, mantém `enterprise.db` separado e não recebe
`GatewayState`, banco do POM ou chaves.

O backend local oferece `/api/ui/enterprise/status`, `/activate`,
`/deactivate` e `/refresh`. A chave entra apenas no POST de ativação e fica em
memória. A resposta do servidor é convertida do contrato publicado:
`fingerprint_scheme` e `fingerprint` são strings, e heartbeat usa o lease token
como bearer. O manifest é verificado antes de instalação e o grant é verificado
antes de marcar o plugin como pronto.

O admin continua sendo a autoridade de bootstrap P2P. Cada peer troca apenas
fingerprint, grant, release e status através do canal autenticado existente;
nenhuma chave de licença é propagada. A consulta agregada exige que os peers
esperados estejam licenciados e prontos.

## Ciclo de vida

1. O POM inicia em Free se não houver estado Enterprise válido.
2. A UI envia uma chave para o backend local.
3. O backend chama `validate` e `activations`, baixa o release compatível,
   verifica campos, hash, tamanho e assinatura Ed25519, instala em diretório
   imutável e inicia o host.
4. O host responde `ready` com a versão esperada. Só então o status vira
   `active`.
5. Uma tarefa de supervisão faz heartbeat conforme a política do lease. Falha
   transitória marca `degraded` dentro da grace; revogação/expiração encerra
   o host e deixa a inferência Free funcionando.
6. Atualizações usam arquivo temporário, flush, rename atômico e preservam o
   release anterior como rollback.

## Plugin real

`crates/enterprise-plugin` implementa a ABI C v1 e o domínio de auditoria com
SQLite em `enterprise.db`. O evento `begin` e `complete` preserva os tipos das
rotas atuais, sem importar o `PomDb`. O plugin responde `list`, `stats` e
`telemetry`; filtros e agregações deixam de executar no banco Free.

`scripts/build-enterprise-plugin.sh` compila o plugin em release, seleciona o
nome nativo (`.so`, `.dylib` ou `.dll`) e copia o artefato mais o manifest
local para uma pasta de distribuição. O comando de publicação é documentado
como `npm run release:add -- --file <artefato> --version ... --os ...
--arch ...` no servidor, sem credenciais no repositório.

## Erros e segurança

Erros `INVALID_KEY`, `FEATURE_NOT_ENTITLED`, `EXPIRED`, `SUSPENDED`, `REVOKED`,
`NO_COMPATIBLE_RELEASE`, hash ou assinatura inválidos nunca iniciam o plugin.
Timeout, 429 e 500 usam retry limitado somente antes de alterar estado. O
frontend mostra estado inativo e link de compra, mas nunca consulta o servidor
de licenças diretamente.

## Verificação

Testes de contrato cobrem o envelope e a conversão para o wire publicado;
testes do host cobrem ABI, frames, query e crash/restart; testes do plugin
cobrem persistência e estatísticas; testes de gateway cobrem ausência de
persistência em Free e proxy Enterprise; testes de UI cobrem ativação,
download, erro e expiração. `cargo test --workspace`, `cargo fmt --all
-- --check`, `npm test` e `npm run build` são os gates finais.
