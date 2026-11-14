---
id: getting-started
label: Primeiros passos
summary: Da primeira máquina a uma rede com duas
title: Primeiros passos
order: 20
---

:::lead
Uma rede começa com uma máquina administradora. As outras entram por convite. Nenhum modelo é passado na linha de comando: tudo depois do primeiro arranque acontece por esta interface.
:::

:::diagram setup
O administrador cria a rede e gera o convite. A máquina nova acha o administrador pela descoberta local, ou recebe o convite colado, e passa de entrando para conectada.
:::

:::steps
### Inicie o binário
Rode o executável pom sem argumentos. Sem arquivo de configuração, o nó sobe em modo de configuração e espera pelo navegador.
### Abra a interface
Acesse http://<máquina>:8080/. A interface e a API sempre ficam na porta do gateway, que é 8080 por padrão.
### Escolha o papel
A primeira máquina escolhe Administrador. O assistente pede só o nome da máquina, o nome da rede e se ela é privada ou pública.
### Guarde o convite
No último passo o administrador recebe o convite: a chave de conexão (em rede privada) e o arquivo trust.json. É com eles que as outras máquinas entram.
### Junte a segunda máquina
Na outra máquina, rode o mesmo binário, abra a interface e escolha Nó. Na mesma rede local, o assistente acha o administrador sozinho; senão, cole o trust.json e a chave.
:::

:::note Enquanto o assistente roda, o administrador vê quem chega
No passo do convite, cada computador procurando a rede aparece com nome, sistema, IP, CPU, RAM e GPU, e passa de entrando para conectado quando recebe a raiz. Terminar sem ninguém também é normal.
:::

:::warn Troque a senha padrão
Depois da configuração, a senha do painel é admin e a interface mostra um aviso fixo até você trocá-la em Preferências. Pastas, reservas de memória e limite térmico vêm de padrões da máquina e também podem ser ajustados depois.
:::

Com as duas máquinas na rede, o próximo passo é ter um modelo: traga um GGUF para a pasta de modelos ou use Procurar Modelos (o plugin do Hugging Face, instalado em Plugins), e siga o guia Do modelo ao cluster servindo.

:::note Máquinas em redes diferentes
Fora da mesma rede local, a descoberta automática não alcança o administrador. Inicie o nó com --bootstrap <administrador>:42777 e veja o guia Rede local e internet.
:::

:::checklist Pronto para o próximo passo quando
- As duas máquinas aparecem no Painel.
- A senha padrão do painel foi trocada em Preferências.
- Há um modelo GGUF na pasta de modelos, ou um download concluído em Downloads.
:::

:::faq
### Preciso abrir portas no roteador?
Na mesma rede local, não: a descoberta por UDP basta. Entre redes diferentes, veja o guia Rede local e internet, que lista as portas e o endereço anunciado.
### Posso rodar dois nós na mesma máquina?
Sim, desde que cada um tenha o próprio diretório de trabalho (POM_HOME) e portas próprias, por exemplo --port 42778 --gateway-port 8091 no segundo.
### Perdi o convite. E agora?
No administrador, Configurações > Compartilhamento mostra de novo a raiz de confiança e a chave de conexão.
:::

:::deep docs/subsystems/operations.md
Subida de um cluster de duas máquinas e verificação
:::

:::deep docs/subsystems/web-ui.md
O assistente de primeiro acesso na interface
:::
