# Hugging Face por HTTP nativo

## Decisão

O captain decidiu não redistribuir o CLI `hf`. O `HuggingFaceProvider` agora
fala diretamente com a API do Hub através de `reqwest`, preservando a forma do
`ModelProvider`, o contrato interno do backend e o JSON consumido pela tela de
modelos remotos.

O caminho antigo em `hf_cli.rs` foi removido. Depois da troca não existe
caller para uma implementação alternativa e manter o subprocesso preservaria
código morto, duas políticas de autenticação e a origem do erro
`cli_not_installed`.

## Resposta factual sobre o executável `hf`

O `hf` oficial é distribuído como parte do pacote Python
`huggingface_hub`; não foi encontrado um binário oficial autônomo publicado.
O instalador oficial cria um ambiente virtual e instala o pacote. Congelar
esse pacote com PyInstaller eliminaria a dependência do Python instalado pelo
usuário, mas incluiria o interpretador Python e as dependências no executável,
com um artefato diferente para Linux, macOS e Windows. Fontes verificadas:
[documentação do CLI](https://huggingface.co/docs/huggingface_hub/en/guides/cli),
[instalador oficial](https://github.com/huggingface/huggingface_hub/blob/main/utils/installers/install.sh)
e [pacote publicado](https://pypi.org/project/huggingface-hub/).

## Rotas consideradas

| Rota | Tamanho e distribuição | Esforço e risco | Licença |
| --- | --- | --- | --- |
| HTTP nativo Rust | Não acrescenta Python, CLI ou runtime ao pacote; usa as dependências Rust já presentes e funciona nas três plataformas | Implementa autenticação, metadados, streaming, retomada, cancelamento, progresso, cache e integridade no POM; o risco passa a ser manutenção do contrato HTTP | Não redistribui os pacotes Python do Hub; continuam valendo as licenças das dependências Rust do POM |
| CLI congelado | O wheel observado de `huggingface_hub` 1.28.0 tinha 793,2 kB, mas o executável real teria de incluir interpretador, bootloader e dependências por plataforma; a medição final dos três executáveis foi encerrada pela decisão | Menos código de protocolo no POM, porém build, assinatura, empacotamento e matriz de release Python por plataforma | Exigiria avisos e `THIRD_PARTY_LICENSES`; PyInstaller apareceu como GPLv2 no inventário local e requer revisão da exceção do bootloader antes de um produto fechado |

O caminho escolhido é HTTP nativo. A tela não foi alterada.

## O que passou a ser responsabilidade do POM

- Streaming: cada resposta de arquivo é consumida por `bytes_stream` e gravada
  em disco; o arquivo inteiro não é carregado na memória.
- Retomada: o POM consulta `HEAD`, guarda tamanho, `ETag` e
  `x-repo-commit`, e usa o SHA-256 dos metadados quando disponível. Um parcial
  só continua se sua identidade corresponder à identidade atual. A requisição
  usa `Range` e `If-Range`; sem nenhuma identidade confiável o parcial é
  descartado para não corromper silenciosamente um arquivo diferente.
- Cancelamento: `CancellationToken` interrompe a espera da requisição e o
  streaming. O parcial e seu JSON de identidade são removidos, e arquivos
  finais baixados na mesma operação são removidos pelo job.
- Progresso: os bytes gravados, total conhecido, percentual e velocidade são
  calculados localmente, sem interpretar texto de terminal.
- Cache: foi escolhido um layout próprio do POM. Parciais vivem em
  `<destino>/.pom-hf/partials` ou `<cache_dir>/partials`; o arquivo só passa
  para o destino final depois de validado. Isso evita depender de detalhes
  privados do cache Python e deixa a retomada sob controle do POM.
- Integridade: o SHA-256 exposto pela API é comparado depois do streaming e o
  manifesto final também registra o SHA-256 calculado por `pom::hash_file`.

## API, autenticação e segurança

O provider usa `/api/models` para busca, `/api/models/{repo}` para detalhes,
`blobs=true` para metadados de arquivos, `whoami-v2` para autenticação e
`/{repo}/resolve/{revision}/{filename}` para downloads. `repo_id`, revisão,
nome de arquivo, filtros e diretórios passam pelas validações existentes antes
de virar URL ou caminho local.

As fontes de credencial, na ordem, são `HF_TOKEN`,
`HUGGING_FACE_HUB_TOKEN`, `HF_TOKEN_PATH`, `HF_HOME/token` e o caminho padrão
do Hugging Face sob o diretório do usuário. O token só é usado no header
Bearer. Ele não entra em log, telemetria, resposta ou mensagem de erro. Token
ausente produz status não autenticado; HTTP 401 sem token vira
`authentication_required`, HTTP 401 com token e HTTP 403 viram `access_denied`.
Repositório inexistente e arquivo inexistente também têm erros distintos.

O campo histórico `cli.installed` permanece no status por compatibilidade com
a UI e vale `true` quando o provider Rust está disponível. Portanto o caminho
normal não pode gerar `cli_not_installed`; a ausência de token aparece como
ação de autenticação, e falhas de rede aparecem como erro de transporte
acionável.

## Licenças da rota que foi descartada

O inventário local do ambiente Python observou: `huggingface_hub` e `hf-xet`
Apache-2.0; `click` BSD-3-Clause; `filelock` MIT; `fsspec` BSD-3-Clause;
`httpx` BSD; `httpcore` BSD-3-Clause; `PyYAML` MIT; `tqdm` MPL-2.0/MIT;
`typing_extensions` PSF-2.0; `certifi` MPL-2.0; `packaging` Apache-2.0 ou
BSD-2-Clause; `idna` BSD-3-Clause; e `anyio` MIT. O inventário também trouxe
`pyinstaller` GPLv2 e `pyinstaller-hooks-contrib` Apache/GPL. Esses dois
últimos não seriam dependências de runtime do provider HTTP, mas tornam um
congelamento para produto fechado uma questão de revisão jurídica e de avisos,
não uma simples cópia do wheel.

## Cobertura de testes

Os testes do provider usam um servidor HTTP local, nunca o Hub real, e cobrem
busca, detalhes, arquivos, licença, autenticação sem token e autenticada,
acesso negado, arquivo ausente, traversal, retomada com `Range`/`If-Range` e
cancelamento com limpeza do parcial. O backend continua sendo o único dono da
API pública e a UI não precisa conhecer o transporte.
