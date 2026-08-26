# Precisão configurável do estado oculto

## Objetivo

Permitir que uma implantação escolha a precisão do tensor de estado oculto que
atravessa as fronteiras entre peers, mantendo f16 como padrão e tornando
mensurável o custo real de uma alternativa menor.

## Decisão

O formato configurável é `f16` ou `f8`. O f8 usa a representação FP8 E4M3FN,
com um byte por valor. O motor de inferência continua trabalhando em f32; a
conversão ocorre na borda do protocolo, em `inference/wire.rs`, tanto ao
codificar o estado para a rede quanto ao decodificá-lo de volta para f32.

O f32 permanece disponível para caminhos internos e compatibilidade do
protocolo, mas não é oferecido como escolha de implantação porque aumenta o
tráfego e não atende ao experimento de redução de precisão. Nenhum formato
adicional é anunciado sem conversão implementada, validada e testada de ponta
a ponta.

## Configuração e persistência

`RuntimeParams.hidden_state_dtype` é o único valor global da implantação. Ele
segue o mesmo caminho dos parâmetros de contexto, batch, chunk de prefill e
cache KV: é editado no formulário compartilhado, enviado em `ApplyRequest` e
`LaunchRequest`, persistido em `DeploymentNode.config` e convertido para
`NodeConfig` antes de iniciar o runtime.

O default de Rust e da interface é `f16`. O campo aceita ausência na leitura
de configurações antigas por meio de `serde(default)`, sem migrar arquivos
manualmente.

## Negociação

Peers atuais anunciam `inference/frame-v1-dtype-f8` junto de
`inference/frame-v1-batch` no vetor já existente
`inference_frame_capabilities`. O planejador exige a capacidade f8 em todos os
hops quando `hidden_state_dtype=f8`. Se uma máquina não anunciar a capacidade,
ou se não for possível lê-la, o plano é recusado com uma mensagem identificando
que o formato escolhido não foi negociado. Nenhum frame f8 é enviado nesse
caso.

O f16 não passa a depender de uma capacidade nova. Assim, uma implantação que
não altera a configuração preserva o comportamento atual inclusive com peers
mais antigos. Um peer que receba um dtype desconhecido também responde com
erro de protocolo, nunca interpretando os bytes como outro formato.

## Medição

Cada fase de `RequestTrace` registra:

- `hidden_state_dtype`: precisão observada nos estados da fase;
- `hidden_state_bytes_sent`: bytes de tensor enviados pelos hops da fase;
- `hidden_state_bytes_received`: bytes de tensor recebidos pelos hops da fase.

Esses valores não incluem o cabeçalho JSON nem o overhead HTTP, permitindo
comparar diretamente a redução do tensor entre execuções. A tela de chat
mostra os bytes enviados e a precisão ao lado do tamanho total do tráfego.

## Segurança de qualidade

O f8 recusa NaN, infinito e valores fora do alcance finito E4M3FN, em vez de
saturá-los silenciosamente. O decodificador recusa os padrões reservados. A
suíte testa conversão de ida e volta, tamanho do payload, erro de overflow e
negociação incompatível.

## Fora do escopo

Não há escolha automática de precisão, benchmark embutido, escala por tensor,
calibração de modelo ou promessa de qualidade/performance. A decisão de usar
f8 continua sendo do operador, medida no cluster real.

O contexto de campo mais recente relativiza as estimativas anteriores: a
transferência de arquivos do POM via libp2p atingiu cerca de 40 MB/s, então 126
MB seriam aproximadamente 3 s se o caminho de inferência usasse o mesmo
transporte. Isso torna os bytes e o formato medidos por travessia ainda mais
importantes que estimativas baseadas em HTTP de objeto pequeno.
