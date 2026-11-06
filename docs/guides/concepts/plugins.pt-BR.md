---
id: plugins
label: Plugins
summary: Licenças e repositórios, máquina por máquina
title: Plugins
order: 80
---

:::lead
Plugins acrescentam telas e funções ao painel. Eles são locais: cada máquina ativa e instala os seus, e uma licença de um plugin não liga nem desliga outro.
:::

:::diagram plugins
Cada plugin roda num processo próprio ao lado do nó. A licença ou o repositório GitHub vale só para esta máquina.
:::

:::steps
### Abra Plugins
Em Configurações > Plugins aparece o catálogo, com o que está ativo nesta máquina.
### Ative por licença
Cole a chave de licença no formulário. A resposta diz quais plugins aquela chave concedeu e instalou.
### Ou instale do GitHub
Adicione a URL de um repositório público em Adicionar do GitHub e escolha uma release compatível com esta máquina. Instalar e atualizar pedem confirmação.
:::

:::cards
### Qualquer perfil
Administrador, nó e inferência podem ativar, instalar, atualizar e usar plugins na própria máquina.
### Telas descobríveis
Logs de requisições e Estatísticas aparecem no menu mesmo sem licença; os dados só chegam quando o plugin correspondente está ativo.
### Processo separado
Cada plugin é carregado por um processo próprio, o pom-plugin-host, e não dentro do binário principal. Plugins são independentes entre si.
:::

:::note Atualizar o pom não é um plugin
A verificação de nova versão do próprio binário fica em Configurações > Sobre, separada da tela de Plugins.
:::

:::deep docs/subsystems/enterprise-licensing.md
Licenças, repositórios GitHub e a fronteira de processo
:::

:::deep docs/subsystems/plugins-ui.md
Menus, telas e rotas fornecidos por plugins
:::
