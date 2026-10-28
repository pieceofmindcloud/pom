---
id: access
label: Acesso e segurança
summary: Senha, chaves e o que cada uma abre
title: Acesso e segurança
order: 100
---

:::lead
Há três credenciais, e cada uma abre uma porta diferente. Saber qual é qual evita entregar a chave errada para a pessoa errada.
:::

:::cards
### Senha do painel
Credencial humana para abrir a interface. Fica só na memória da aba; o nó guarda apenas um hash dela. A sessão vale para as rotas administrativas.
### Chave de conexão
A mesma no cluster inteiro. Identifica o operador e o canal entre os nós: autentica as rotas administrativas e de implantação. Não serve para inferência.
### Chave de API
A única credencial aceita em /v1 e /api/generate. Cada uma tem uma identidade não secreta, então os registros mostram quem pediu sem expor o segredo.
:::

:::note A porta de inferência não aceita chaves de cliente
O servidor de inferência entre as máquinas só aceita a credencial interna do cluster. Chaves de API de clientes são recusadas ali, e só o /health fica público para conferir prontidão.
:::

:::warn Pense na rede antes de expor uma porta
Sem TLS entre os nós, o estado oculto trafega em claro. Ligue o TLS em Preferências administrativas quando as máquinas conversam por redes que você não controla.
:::

:::note Bloquear uma máquina
Um nó pode ser bloqueado pela identidade, e a identidade dele é derivada de uma chave que vive no diretório de trabalho da máquina. Apagar essa chave cria uma máquina nova aos olhos das outras.
:::

:::faq
### Onde vejo a chave de conexão?
Em Configurações > Chaves de conexão. Trocá-la corta todos os outros nós, por isso o botão só aparece para quem tem autoridade.
### Uma chave de API vazada compromete o cluster?
Ela só abre as rotas de inferência. Revogue a chave em API keys e gere outra; a chave de conexão continua valendo.
:::

:::deep docs/subsystems/http-api.md
Autenticação, perfis e todas as rotas
:::

:::deep docs/subsystems/p2p-network.md
TLS entre peers e limitações para a internet
:::
