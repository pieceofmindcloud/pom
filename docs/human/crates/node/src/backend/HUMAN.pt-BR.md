---
id: human-backend
label: Backend e API
summary: O gateway que todo nó serve, e quem pode chamar o quê
title: Por dentro do backend
reference: crates/node/src/backend/AGENTS.md
order: 50
---

:::lead
Todo nó é um gateway: na porta 8080 ele serve esta interface, a API compatível com OpenAI e Anthropic e as rotas de administração. Esta pasta é esse lado HTTP. Ela tem o único jeito de gerar texto - o pipeline distribuído - e as regras de quem pode chamar cada rota.
:::

:::diagram api
O cliente chama qualquer gateway com uma chave de API; a resposta volta com x_pom.source contando quais máquinas trabalharam.
:::

:::cards
### Um caminho de geração
`/api/generate` e as rotas `/v1` só rodam o pipeline distribuído. Sem um plano que cubra todas as camadas, a requisição falha com uma mensagem - não há alternativa.
### Duas superfícies separadas
As rotas do pipeline e as de administração nunca dividem uma regra de autorização. A inferência pede uma chave de API criada por este nó; a administração pede a sessão do navegador ou uma requisição assinada de outro membro.
### Cada rota tem um escopo
Algumas rotas só o principal chama, outras qualquer admin, outras um membro que está servindo, outras só esta máquina. O papel é imposto pelos nós, nunca pelas telas.
### O orçamento do cliente é final
O `max_tokens` que o cliente pede é um teto que nada atravessa. O `max_new_tokens` da implantação só dimensiona uma perna do laço de continuação do gateway, invisível para o cliente.
:::

:::diagram profiles
O admin principal governa a rede; o admin secundário implanta e admite; o worker serve camadas; o visitante conversa com os modelos.
:::

:::deep crates/node/src/backend/AGENTS.md
Cada roteador e handler do backend
:::

:::deep docs/subsystems/http-api.md
Cada rota, autenticação e escopo
:::
