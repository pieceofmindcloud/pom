---
id: human-plugin-host
label: Host de plugins
summary: Por que um plugin roda num processo próprio, e como ele fala com o POM
title: Por dentro do host de plugins
reference: crates/plugin-host/AGENTS.md
order: 60
---

:::lead
Um plugin nunca roda dentro do processo `pom`. O `pom-plugin-host` o carrega ao lado, uma instância por plugin ativo, e os dois conversam por um protocolo pequeno e versionado. Um plugin que quebra derruba só o próprio processo.
:::

:::diagram plugins
Cada plugin roda num processo próprio ao lado do nó. A licença ou o repositório GitHub vale só para esta máquina.
:::

:::steps
### Ativar
Um plugin é ativado por `plugin_code`: por uma chave de licença que o servidor de licenças concedeu, ou a partir de um repositório público no GitHub. Os plugins não dividem estado entre si.
### Carregar
O `pom` inicia o `pom-plugin-host` como filho com a biblioteca do plugin. A biblioteca é carregada por uma ABI em C, e os dois processos conversam por stdin e stdout - sem socket, sem volume compartilhado.
### Aparecer
O plugin declara menus, telas, rotas e textos num manifesto (`pom-plugin-ui/v1`), e o POM os desenha nesta interface.
### Ficar em dia
Cada plugin se atualiza sozinho. A atualização do próprio POM não é um plugin: fica à parte e troca o binário pelo mesmo host.
:::

:::note Uma licença por plugin
Não existe uma licença única do produto: cada plugin tem a própria ativação, lease e heartbeat. A tela de um plugin pago continua no menu sem licença, mas o nó recusa os dados dela.
:::

:::deep crates/plugin-host/AGENTS.md
O processo host
:::

:::deep docs/subsystems/enterprise-licensing.md
Licenças, a fronteira de processo e as atualizações
:::
