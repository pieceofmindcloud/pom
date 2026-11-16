---
id: screen-my-keys
label: Minhas chaves de API
summary: As chaves de API de uma pessoa conectada
title: Minhas chaves de API
route: /config/my-keys
order: 282
---

:::lead
Minhas chaves de API é onde uma pessoa conectada a este nó com a própria conta cria chaves para as próprias ferramentas. As requisições feitas com essas chaves levam a identidade dessa pessoa, então contam na cota dela e aparecem nos logs dela. Só a sessão de uma pessoa abre esta tela; a sessão do painel do administrador não abre, e cria chaves do nó em API keys.
:::

:::steps
### Criar uma chave
**Criar chave**, com um nome opcional que diz depois para que ela serve.
### Copiar uma vez
A chave aparece só uma vez, logo depois de criada. Copie nesse momento.
### Revogar
**Revogar** para uma chave na hora; a lista a mantém marcada como revogada.
:::

:::deep docs/subsystems/security-model.md
Quem pode o quê, e as chaves de uma pessoa
:::
