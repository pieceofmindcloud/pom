---
id: using-the-api
label: Usar a API
summary: Chaves de API, rotas compatíveis e harnesses
title: Usar a API
order: 60
---

:::lead
Qualquer nó fala a API compatível com OpenAI e também a de mensagens da Anthropic. Apontar uma ferramenta para o cluster é escolher a URL de um nó e mandar uma chave de API.
:::

:::diagram api
O cliente chama qualquer gateway com uma chave de API; a resposta volta com x_pom.source contando quais máquinas trabalharam.
:::

:::steps
### Crie uma chave de API
Em Configurações > API keys. Cada perfil cria a sua; a chave de conexão do cluster não serve para inferência.
### Descubra o modelo e o limite
GET /v1/models lista os modelos servidos agora pela rede, com o contexto disponível e o máximo de tokens de saída.
### Faça a chamada
Mande a chave em Authorization: Bearer ou em x-api-key. O pedido entra no pipeline distribuído; não há outra rota escondida.
:::

```
curl -s http://<gateway>:8080/v1/chat/completions \
  -H "Authorization: Bearer $POM_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"Diga oi em uma frase."}],"max_tokens":32}'
```

:::note A prova de que a resposta foi distribuída
A resposta traz x_pom.source. Um valor começando por pipeline: e com dois ou mais saltos é a prova de que mais de uma máquina trabalhou. Um salto só, ou none, significa que a cobertura não fechou.
:::

:::cards
### max_tokens
É o teto da sua requisição e é respeitado à risca. Atingi-lo encerra a resposta com finish_reason length.
### Prompt grande demais
Se o prompt não deixa espaço na janela de contexto, a resposta é HTTP 413 antes de qualquer geração.
### Streaming
Com stream: true a resposta chega por SSE, e os argumentos de uma chamada de ferramenta saem em fragmentos conforme o modelo gera.
:::

:::note Claude Code e outros clientes
Para Claude Code, aponte ANTHROPIC_BASE_URL para a origem do gateway (por exemplo http://<gateway>:8080) e use uma chave de API ativa em ANTHROPIC_API_KEY. Clientes compatíveis com OpenAI usam a base /v1.
:::

:::faq
### Qual a diferença entre max_tokens e o max_new_tokens da implantação?
O max_new_tokens da implantação só limita uma perna interna de geração: o gateway continua na mesma sessão sem o cliente perceber. Quem limita a sua requisição é o max_tokens.
### Posso usar a chave de conexão no meu cliente?
Não. As rotas de inferência aceitam somente uma chave de API ativa.
:::

:::deep docs/subsystems/http-api.md
Todas as rotas, autenticação e limites de saída
:::

:::deep docs/subsystems/inference.md
Orçamentos de saída e o laço de continuação
:::
