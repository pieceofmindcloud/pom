---
id: strata-pom-settings
label: Configurações POM do Strata
summary: Como o plugin Strata divide um modelo entre máquinas
title: Configurações POM do Strata
order: 130
---

:::lead
Quando uma implantação roda no plugin Strata, o inspetor de implantação mostra um card de configurações POM com as configurações que o Strata declara. Elas decidem como o POM distribui o modelo e as conversas entre as máquinas. As configurações do próprio motor (amostragem, raciocínio, MTP) ficam no card do motor, acima dele.
:::

| Propriedade | O que faz | Quando mudar |
|---|---|---|
| Papel na inferência | Aparece quando o modelo inteiro roda em mais de uma máquina, uma linha por máquina. **Prefill e decode** (o padrão): a máquina responde às próprias conversas e lê os próprios prompts. **Prefill primeiro**: faz o mesmo e, enquanto está livre, também lê os prompts longos das outras máquinas e os entrega como sessão. | Use **Prefill primeiro** na máquina que lê prompts mais rápido. Deixe as outras em Prefill e decode. |
| Máquina de prefill a partir de (tokens) | Um turno passa pela máquina de prefill só quando a parte do prompt que a máquina que vai responder ainda não tem soma pelo menos esse número de tokens. Turnos menores são lidos pela própria máquina que responde, porque enviar a sessão custa cerca de 118 MB, qualquer que seja o prompt. Vazio: 4096. 0: todo turno com algo novo. | Diminua num link rápido entre as máquinas. Aumente quando o link for lento. |
| Salvar o cache KV | Salva em disco o cache KV de cada conversa com 8.000 tokens de prompt ou mais, para que outra máquina o restaure em vez de ler o prompt de novo. A pasta guarda no máximo 5 GB por máquina, e conversas sem uso há 24 horas são removidas. | Ligue quando as conversas mudam de máquina. O modelo inteiro em várias máquinas já mantém as sessões ligadas. |
| Formato do cache KV | Como o Strata guarda o cache KV: int8 (o padrão), q4_0 (metade da memória, um pouco menos preciso) ou k8v4. Todas as máquinas da implantação usam o mesmo formato, para que uma sessão salva numa máquina restaure em outra. | Use q4_0 quando faltar memória. |
| Pedaços de prefill em voo | Aparece quando o modelo é dividido em fatias entre máquinas. Define quantos pedaços de prefill andam entre os estágios ao mesmo tempo (1 a 8). | Aumente num pipeline profundo. Use 1 para desligar a sobreposição. |
| Precisão do estado oculto | Aparece quando o modelo é dividido em fatias entre máquinas. Define os bytes enviados entre os estágios por token: f16 (o padrão), f32 (sem perda, o dobro do volume), f8 ou int4 (com perda). | Experimente f8 num link lento. |

:::warn Você não escolhe o papel compartilhado
Quando o modelo inteiro vai para duas ou mais máquinas, o POM dá sozinho um terceiro papel, compartilhado, a toda máquina deixada em Prefill e decode. Esse papel mantém as sessões do Strata ligadas, que é como as máquinas passam conversas umas para as outras. No Dashboard essas máquinas mostram o selo **Prefill/Decode**, e a máquina de prefill mostra **Prefill primeiro**.
:::

## Receitas

:::steps
### Duas máquinas iguais
Coloque o modelo inteiro nas duas e deixe as duas em Prefill e decode. Cada máquina responde às conversas que o gateway manda para ela, e uma conversa volta para a máquina que a guarda.
### Uma máquina lê prompts mais rápido
Use Prefill primeiro nessa máquina. Enquanto está livre, ela lê os prompts longos das outras, e as outras máquinas só decodificam.
### Um modelo em fatias mais uma máquina de prefill
Implante as fatias e adicione o modelo inteiro em outra máquina com Prefill primeiro. Essa máquina responde requisições sozinha, ao lado do pipeline.
:::

:::faq
### O que aconteceu com o Decode primeiro?
Ele fazia o mesmo que Prefill e decode: toda máquina que não é de prefill já recebe os prompts que uma máquina de prefill lê para ela. Foi removido.
### Uma máquina em Prefill primeiro também responde?
Sim. Todo papel responde requisições. Prefill primeiro só acrescenta a leitura de prompts para as outras máquinas.
### O Qwen3.6-35B-A3B roda em várias máquinas?
Sim. Cada máquina lê os próprios prompts, porque esse motor não tem slots de batch para entregar um prompt. Uma máquina de prefill precisa do Qwen3.8-Flash-Next.
### Por que um turno curto não passa pela máquina de prefill?
A sessão do Strata carrega cerca de 118 MB de estado recorrente, qualquer que seja o tamanho do prompt. Para um prompt curto, ler de novo é mais rápido que enviar a sessão.
:::

:::deep docs/subsystems/inference.md
Serviços de inferência de plugins e a divisão prefill/decode
:::
