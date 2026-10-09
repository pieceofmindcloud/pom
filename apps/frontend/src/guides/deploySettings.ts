/**
 * The guides behind the help button of each category of the deployment
 * inspector: Default settings, POM settings and the Beta group inside it.
 *
 * They live apart from `content.ts` because they are reference for one screen,
 * read from a dialog next to the fields, and they follow the inspector's own
 * categories. `content.ts` appends them after the general guides, so they also
 * open from Documentation under `?guide=`.
 *
 * Same rule as the other guides: every number and behaviour here is in the
 * field hints (`deploy.knob.*Hint`) or in `docs/subsystems/deployment.md`.
 */
import type { Guide } from "./content";

/** The `?guide=` id of each inspector category's guide. */
export const DEPLOY_SETTINGS_GUIDES = {
  defaults: "default-settings",
  pom: "pom-settings",
  beta: "beta-settings",
} as const;

export const deploySettingsPtBR: Guide[] = [
  {
    id: DEPLOY_SETTINGS_GUIDES.defaults,
    label: "Configurações padrões",
    summary: "Contexto, lote, amostragem, cache KV e especulação",
    title: "Configurações padrões da implantação",
    blocks: [
      {
        kind: "lead",
        text: "Estas são as configurações que o próprio llama.cpp já oferece. O POM só as leva para cada máquina da implantação. Elas decidem quanta memória cada fatia reserva, quão rápido o prompt entra e como o modelo escolhe cada token.",
      },
      { kind: "h", text: "Memória e capacidade" },
      {
        kind: "table",
        head: ["Propriedade", "O que faz", "Quando mudar"],
        rows: [
          ["Contexto", "Janela de atenção de cada sessão, em tokens. Depois dos pesos, é o que mais pesa na VRAM do cache KV.", "Aumente para conversas e prompts longos. Diminua só se a implantação não couber de outro jeito."],
          ["Sessões", "Gerações atendidas ao mesmo tempo. Cada uma reserva o contexto inteiro, então o cache KV custa Contexto × Sessões.", "Aumente para atender mais gente ao mesmo tempo. Cada sessão a mais custa um contexto inteiro."],
          ["Máx. tokens", "Teto de uma perna do laço de continuação do gateway, não da requisição. Quem limita a resposta é o max_tokens do cliente.", "Raramente."],
          ["Batch", "Tokens do prompt que o llama.cpp processa num passo do prefill. Maior enche melhor a GPU em prompts longos e aumenta o buffer de cálculo. Não muda a velocidade da geração.", "Diminua se faltar VRAM. Aumente se sobrar e os prompts forem longos."],
          ["Microlote", "Quantos tokens o contexto decodifica de uma vez com o ajuste de memória ligado. Os buffers de cálculo na VRAM são dimensionados por ele. Nunca maior que o Batch; com o ajuste desligado, vale o Batch.", "Baixe de 512 para 256 para liberar VRAM para pesos, MTP ou cache de experts. O prefill fica um pouco mais lento."],
          ["Chunk prefill", "Tokens do prompt enviados pelo pipeline a cada ida e volta no prefill.", "Menor começa a transmitir antes num link lento. Maior reduz o número de idas e voltas."],
          ["Cache K / Cache V", "Tipo do cache KV que o llama.cpp aloca (f16, q8_0, q4_0...). Quantizado ocupa menos VRAM. Vale para todo contexto que o nó carregar em seguida.", "q8_0 ou q4_0 em contextos grandes."],
        ],
      },
      {
        kind: "note",
        title: "A conta da memória",
        text: "Cada máquina precisa de: os pesos da fatia, o cache KV (Contexto × Sessões, no tipo do Cache K/V) e os buffers de cálculo (que crescem com o Microlote). Duas sessões de 32k custam o mesmo que uma de 64k.",
      },
      { kind: "h", text: "Amostragem" },
      {
        kind: "p",
        text: "Só o último peer do pipeline amostra. A ordem é a do llama.cpp: primeiro os cortes do conjunto de candidatos, depois as penalidades de repetição, depois o DRY. A semente vem no fim.",
      },
      {
        kind: "table",
        head: ["Propriedade", "O que faz", "Ponto de partida"],
        rows: [
          ["Temperatura", "Suaviza ou acentua a distribuição antes da amostragem. 0 é sempre o token mais provável; acima de 1 espalha a probabilidade.", "0,6 a 1,0 conforme o modelo."],
          ["Top-k", "Mantém só os K tokens mais prováveis. Aplicado antes do top-p.", "0 desliga."],
          ["Top-p", "Mantém o menor conjunto cuja probabilidade soma este valor e renormaliza.", "1 desliga."],
          ["Min-p", "Descarta o candidato abaixo desta fração do mais provável, depois do top-k e do top-p.", "0 desliga; 0,05 é comum."],
          ["Penalidade", "Divide o logit dos tokens vistos na janela recente, contra laços.", "1 é sem penalidade; acima de ~1,3 pode distorcer o texto."],
          ["Janela repetição", "Quantos tokens recentes a Penalidade considera.", "0 desliga a janela."],
          ["Presença", "Subtrai uma vez do logit de todo token já visto na janela, empurrando para palavras novas.", "0 desliga; a Qwen recomenda até 1,5 contra repetição sem fim."],
          ["Frequência", "Subtrai do logit uma vez por aparição: quanto mais a palavra se repete, menos provável fica.", "0 desliga."],
          ["DRY: multiplicador", "Liga o DRY, que pune a continuação de sequências já vistas (não palavras isoladas). Os outros números do DRY só aparecem com ele ligado.", "0 desliga; 0,8 é o usual."],
          ["DRY: base", "O quanto a penalidade cresce a cada token a mais da sequência repetida.", "1,75."],
          ["DRY: tamanho permitido", "Sequências repetidas menores que isto não são punidas.", "2."],
          ["DRY: janela", "Quantos tokens recentes o DRY percorre.", "0 percorre o histórico inteiro, até 8192 tokens."],
          ["Semente", "De onde o gerador aleatório parte quando a implantação carrega. Mesma semente, mesmo modelo e mesmas requisições na mesma ordem dão as mesmas respostas.", "O padrão do POM."],
          ["Limite de raciocínio", "Quantos tokens um bloco de raciocínio pode ocupar; no limite, o fim do raciocínio é forçado.", "Vazio é sem limite; 0 fecha o bloco de imediato."],
          ["Desativar thinking", "O modelo responde direto, sem o preâmbulo entre tags.", "Ligue quando só a resposta importa."],
        ],
      },
      { kind: "h", text: "Especulação" },
      {
        kind: "p",
        text: "A especulação rascunha os próximos tokens e confere todos num lote só. Quando o rascunho acerta, a rodada pelo pipeline rende vários tokens em vez de um. Só um modo roda de cada vez.",
      },
      {
        kind: "table",
        head: ["Propriedade", "O que faz"],
        rows: [
          ["Especulação", "Nenhuma; MTP (draft), que rascunha com a cabeça treinada do próprio modelo (nextn); ou Especulativa (lookup), que repete texto que já está no contexto e só compensa em saída repetitiva."],
          ["Tokens rascunhados pelo MTP", "Quantos tokens a cabeça nextn adivinha por rodada. Mais alarga o lote em todos os hops; menos desperdiça menos quando o palpite erra."],
          ["Probabilidade mínima do MTP", "Piso de confiança para continuar o rascunho. Menor aceita mais palpites, inclusive os fracos; 0 aceita todos."],
          ["Onde o MTP fica", "Automático: VRAM só se o modelo inteiro ainda couber junto. VRAM: o MTP reserva a dele primeiro e o modelo usa o resto. RAM: o MTP roda na CPU e não gasta VRAM."],
          ["MTP (cauda)", "Aparece quando o modelo não traz a cabeça nextn: escolha o rascunho externo (mtp-*.gguf) que a última máquina carrega."],
          ["Largura do rascunho / Janela de busca", "Só no lookup: quantos palpites por lote, e o maior n-grama procurado numa repetição anterior."],
        ],
      },
      {
        kind: "warn",
        title: "Meça o MTP num modelo MoE com experts na RAM",
        text: "A rodada de verificação processa vários tokens de uma vez. Com os experts na RAM, ela busca muito mais experts que um token sozinho, e a geração pode ficar mais lenta que sem especulação. Compare os tok/s na telemetria antes de manter.",
      },
      { kind: "h", text: "Como mudar uma propriedade" },
      {
        kind: "steps",
        items: [
          { title: "Abra o inspetor", text: "Em Implantar, escolha o modelo e as máquinas. O inspetor mostra os valores com que a implantação vai subir. O ícone [i] à esquerda de cada nome explica o campo." },
          { title: "Mude o valor", text: "Digite o número no campo ou marque a caixa. Um campo que depende de outro (os números do DRY, os do MTP) aparece quando o outro o liga." },
          { title: "Confira a prévia de alocação", text: "Ela refaz a conta de memória de cada máquina com os valores novos. Se uma máquina passar do limite, ajuste antes de implantar." },
          { title: "Implante ou reimplante", text: "Para uma implantação que já está no ar, use Reimplantar no Histórico: a versão nova sobe com os parâmetros mudados e o Histórico guarda a anterior." },
        ],
      },
      {
        kind: "faq",
        items: [
          { q: "A implantação não sobe por falta de VRAM. O que mexo primeiro?", a: "Ligue Ajustar à memória, nas Configurações POM. Depois baixe o Microlote para 256 e use Cache K/V quantizado. Reduza Contexto ou Sessões só em último caso." },
          { q: "O max_tokens do cliente ou o Máx. tokens: quem vence?", a: "O max_tokens do cliente limita a resposta. O Máx. tokens só divide a geração em pernas dentro do gateway." },
          { q: "Mudar a amostragem exige recarregar o modelo?", a: "Os valores vão com a implantação, então a mudança entra numa nova versão (Reimplantar)." },
        ],
      },
      { kind: "deep", text: "Implantação, ajuste de memória e ciclo de vida da fatia", doc: "docs/subsystems/deployment.md" },
      { kind: "deep", text: "Pipeline de inferência, prefill e especulação", doc: "docs/subsystems/inference.md" },
    ],
  },
  {
    id: DEPLOY_SETTINGS_GUIDES.pom,
    label: "Configurações POM",
    summary: "O que existe porque a inferência é dividida entre máquinas",
    title: "Configurações POM",
    blocks: [
      {
        kind: "lead",
        text: "O llama.cpp roda um modelo numa máquina. Estas configurações existem porque o POM divide o modelo em fatias, em máquinas diferentes, e precisa decidir como cada fatia cabe na memória e como o estado passa de uma para a outra.",
      },
      {
        kind: "table",
        head: ["Propriedade", "O que faz", "Quando mudar"],
        rows: [
          ["Desativar embeddings", "Normalmente é automático: só o último peer dispensa a extração do estado oculto. Ligado à mão, impede o primeiro peer de extraí-lo, e o pipeline para de funcionar.", "Quase nunca."],
          ["Ajustar à memória", "Antes de carregar, o nó lê a memória livre de cada GPU, reserva uma margem de segurança (256 MiB por padrão) e roda o ajuste do llama.cpp para Contexto × Sessões. Se não couber, reduz as camadas na GPU ou manda parte para a RAM, em vez de falhar por falta de VRAM.", "Deixe ligado. Um Camadas na GPU diferente de 99, ou Experts do MoE na RAM, desligam o ajuste de camadas."],
          ["Chunks em voo", "Quantos pedaços de prefill andam pelo pipeline ao mesmo tempo (1 a 8).", "Maior esconde a latência de cada salto num pipeline profundo; 1 desliga a sobreposição."],
          ["Juntar fatias vizinhas na mesma máquina", "Duas faixas adjacentes na mesma máquina viram um arquivo só antes de subir.", "Ligue quando uma máquina ficou com faixas vizinhas: economiza um salto."],
          ["Precisão do estado oculto", "Bytes enviados entre os peers a cada token. f16 é o padrão; f32 não perde nada e dobra o volume; f8 e int4 por grupo têm perda e exigem suporte de todos os peers.", "Só aparece quando o pipeline cruza máquinas. Experimente f8 num link lento."],
        ],
      },
      {
        kind: "warn",
        title: "Sem o ajuste de memória",
        text: "Com Ajustar à memória desligado, o llama.cpp tenta pôr a fatia inteira, o cache KV e os buffers de cálculo na VRAM. Se não couber, o nó falha ao subir e o supervisor tenta de novo, em laço. Desligado, o Microlote também deixa de valer e vale o Batch.",
      },
      { kind: "h", text: "Receitas" },
      {
        kind: "steps",
        items: [
          { title: "Fazer caber uma fatia grande", text: "Ligue Ajustar à memória, baixe o Microlote para 256 e confira a prévia de alocação. Se a fatia ainda não couber e o modelo for MoE, veja o grupo Beta logo abaixo." },
          { title: "Pipeline em link lento", text: "Suba Chunks em voo (até 8) para sobrepor o prefill entre os saltos. Se todos os peers anunciam suporte, troque a Precisão do estado oculto para f8." },
          { title: "Duas faixas vizinhas numa máquina", text: "Ligue Juntar fatias vizinhas: a máquina serve a união num arquivo só, e o token deixa de atravessar a rede entre as duas faixas." },
        ],
      },
      {
        kind: "faq",
        items: [
          { q: "Ajustar à memória deixa a inferência mais lenta?", a: "Só quando ele precisa mandar camadas ou experts para a RAM, porque é isso ou a fatia não sobe. Quando tudo cabe, ele não muda nada." },
          { q: "Onde vejo o que o ajuste decidiu?", a: "Na prévia de alocação, antes de implantar, e no log de carga do nó, nas linhas que começam com llama.cpp fit." },
          { q: "O que é o estado oculto?", a: "O vetor de ativações que uma fatia entrega à próxima a cada token. É o que atravessa a rede no pipeline." },
        ],
      },
      { kind: "deep", text: "Ajuste de memória, fusão e plano da implantação", doc: "docs/subsystems/deployment.md" },
      { kind: "deep", text: "Pipeline de inferência e precisão do estado oculto", doc: "docs/subsystems/inference.md" },
    ],
  },
  {
    id: DEPLOY_SETTINGS_GUIDES.beta,
    label: "Configurações Beta",
    summary: "Experts do MoE na RAM, cache de experts na GPU e prefill automático",
    title: "Configurações Beta",
    blocks: [
      {
        kind: "lead",
        text: "Estas três configurações existem para rodar um modelo MoE que não cabe inteiro na VRAM. Elas funcionam, mas ainda estão sendo medidas: o melhor valor muda de modelo para modelo e de máquina para máquina.",
      },
      {
        kind: "p",
        text: "Num modelo MoE (mixture of experts), cada camada tem muitos experts e cada token usa só alguns. Os experts são a maior parte dos pesos. Por isso dá para deixá-los na RAM e manter na VRAM só o resto do modelo e os experts usados há pouco.",
      },
      {
        kind: "table",
        head: ["Propriedade", "O que faz"],
        rows: [
          ["Experts do MoE na RAM", "Mantém todos os experts roteados na RAM, calculados pela CPU (o --cpu-moe do llama.cpp), e deixa a VRAM para o resto do modelo. Desliga o ajuste de camadas."],
          ["Cache de experts na GPU (MiB)", "VRAM que guarda os experts da RAM usados há pouco: só as faltas atravessam o PCIe, em lotes de até 32 tokens (a geração; o prefill usa o descarregamento normal). 0 desliga. O valor é por GPU: com duas placas, cada uma guarda os experts das próprias camadas."],
          ["Pedaço de prefill automático", "O gateway escolhe o tamanho do pedaço de cada prompt, entre 32 tokens e o Chunk prefill, pelo que mediu na rota; com o cache de experts ligado, no mínimo 64. Desligado, todo prompt vai em pedaços de exatamente o Chunk prefill."],
        ],
      },
      {
        kind: "stats",
        items: [
          { value: "14,95 tok/s", label: "Qwen3.8-Flash-Next UD-IQ3_XXS (78 GB), uma GPU de 16 GB, todos os experts na RAM" },
          { value: "19,26 tok/s", label: "O mesmo, com 8000 MiB de cache de experts" },
        ],
      },
      { kind: "h", text: "Como combinar" },
      {
        kind: "table",
        head: ["Combinação", "Experts na RAM", "Cache (MiB)", "Ajustar à memória", "Quando usar"],
        rows: [
          ["Tudo na VRAM", "desligado", "0", "ligado", "O modelo cabe na VRAM. É a mais rápida."],
          ["O ajuste decide, com cache", "desligado", "4000 a 8000", "ligado", "O modelo quase cabe. O ajuste manda para a RAM só o que não couber, e o cache serve os experts usados há pouco. Se tudo couber, o nó desliga o cache sozinho."],
          ["Todos os experts na RAM, com cache", "ligado", "o maior que couber", "(desligado pelos experts na RAM)", "O modelo é muito maior que a VRAM. A VRAM fica para atenção, KV, MTP e o cache."],
          ["Todos os experts na RAM, sem cache", "ligado", "0", "(desligado pelos experts na RAM)", "Pouca VRAM. A CPU calcula todos os experts; é a mais lenta."],
        ],
      },
      {
        kind: "note",
        title: "Quanto de cache?",
        text: "Dê ao cache a VRAM que sobra depois dos pesos, do cache KV, dos buffers de cálculo e do MTP. Se o contexto não couber, o nó corta o cache pela metade e tenta de novo, até desligá-lo abaixo de 512 MiB. No log de carga, a linha MoE cache size = X for Y of host experts diz quanto dos experts da RAM o cache cobre; ao descarregar, as linhas llama_moe_cache mostram a taxa de acerto.",
      },
      {
        kind: "warn",
        title: "O cache só guarda experts que estão na RAM",
        text: "Sem experts na RAM (nem pelo ajuste, nem por Experts do MoE na RAM), o cache não tem o que guardar e não muda nada. E com os experts na RAM, o MTP pode deixar a geração mais lenta: meça antes de manter.",
      },
      { kind: "h", text: "Passo a passo" },
      {
        kind: "steps",
        items: [
          { title: "Comece pelo padrão", text: "Ajustar à memória ligado, Experts do MoE na RAM desligado e cache 0. Implante e veja na prévia e no log se o ajuste mandou experts para a RAM." },
          { title: "Ligue o cache", text: "Se mandou, defina o Cache de experts na GPU (algo entre 4000 e 8000 MiB por GPU) e reimplante. Baixar o Microlote para 256 libera VRAM para ele." },
          { title: "Modelo muito maior que a VRAM", text: "Ligue Experts do MoE na RAM e dê ao cache o maior valor que couber." },
          { title: "Compare", text: "Use a telemetria dos logs para comparar tok/s de geração e de prefill entre as versões. O Histórico guarda cada versão, com os parâmetros com que subiu." },
        ],
      },
      {
        kind: "faq",
        items: [
          { q: "Funciona com duas GPUs na mesma máquina?", a: "Sim. O cache é por GPU: as duas placas mantêm a parte delas das camadas, e cada uma guarda os experts das próprias camadas." },
          { q: "Por que o prefill continua lento com o cache?", a: "O cache serve lotes de até 32 tokens, que é a geração. O prefill passa pelo descarregamento normal dos experts da RAM." },
          { q: "Quando desligar o Pedaço de prefill automático?", a: "Para comparar medições com um pedaço fixo, ou numa rota em que a escolha automática não ajuda. Desligado, vale sempre o Chunk prefill." },
        ],
      },
      { kind: "deep", text: "Experts do MoE na RAM e cache de experts na GPU", doc: "docs/subsystems/deployment.md" },
    ],
  },
];

export const deploySettingsEn: Guide[] = [
  {
    id: DEPLOY_SETTINGS_GUIDES.defaults,
    label: "Default settings",
    summary: "Context, batch, sampling, KV cache and speculation",
    title: "Default deployment settings",
    blocks: [
      {
        kind: "lead",
        text: "These are the settings llama.cpp already has. POM only carries them to every machine of the deployment. They decide how much memory each slice reserves, how fast the prompt goes in and how the model picks each token.",
      },
      { kind: "h", text: "Memory and capacity" },
      {
        kind: "table",
        head: ["Property", "What it does", "When to change it"],
        rows: [
          ["Context", "Attention window of each session, in tokens. After the weights, it is the largest share of KV-cache VRAM.", "Raise it for long chats and prompts. Lower it only when the deployment cannot fit any other way."],
          ["Sessions", "Generations served at once. Each one reserves the whole context, so the KV cache costs Context × Sessions.", "Raise it to serve more people at once. Each extra session costs a whole context."],
          ["Max. tokens", "Ceiling of one leg of the gateway's continuation loop, not of the request. The client's max_tokens bounds the response.", "Rarely."],
          ["Batch", "Prompt tokens llama.cpp scores in one prefill pass. Larger fills the GPU better on long prompts and raises the compute buffer. It does not change generation speed.", "Lower it when VRAM is short. Raise it when there is room and prompts are long."],
          ["Micro-batch", "How many tokens the context decodes at once while the memory fit is on. The compute buffers in VRAM are sized for it. Never more than the Batch; with the fit off, the Batch applies.", "Drop it from 512 to 256 to free VRAM for weights, MTP or the expert cache. Prefill gets a little slower."],
          ["Prefill chunk", "Prompt tokens sent through the pipeline per round trip during prefill.", "Smaller starts streaming sooner on a slow link. Larger cuts round trips."],
          ["K cache / V cache", "KV cache type llama.cpp allocates (f16, q8_0, q4_0...). Quantized takes less VRAM. It applies to every context the node loads next.", "q8_0 or q4_0 for large contexts."],
        ],
      },
      {
        kind: "note",
        title: "The memory math",
        text: "Each machine needs: the slice's weights, the KV cache (Context × Sessions, in the K/V cache type) and the compute buffers (which grow with the Micro-batch). Two 32k sessions cost the same as one 64k session.",
      },
      { kind: "h", text: "Sampling" },
      {
        kind: "p",
        text: "Only the last peer of the pipeline samples. The order is llama.cpp's: first the candidate cuts, then the repetition penalties, then DRY. The seed comes last.",
      },
      {
        kind: "table",
        head: ["Property", "What it does", "Starting point"],
        rows: [
          ["Temperature", "Softens or sharpens the distribution before sampling. 0 always takes the most likely token; above 1 spreads probability.", "0.6 to 1.0, per model."],
          ["Top-k", "Keeps only the K most likely tokens. Applied before top-p.", "0 disables it."],
          ["Top-p", "Keeps the smallest set whose probabilities sum to this, and renormalizes.", "1 disables it."],
          ["Min-p", "Drops a candidate below this fraction of the most likely one, after top-k and top-p.", "0 is off; 0.05 is common."],
          ["Penalty", "Divides the logit of tokens seen in the recent window, against loops.", "1 is no penalty; above ~1.3 can distort the text."],
          ["Repeat window", "How many recent tokens the Penalty looks at.", "0 disables the window."],
          ["Presence", "Subtracted once from the logit of every token already seen in the window, pushing toward new words.", "0 is off; Qwen recommends up to 1.5 against endless repetition."],
          ["Frequency", "Subtracted once per appearance: the more a word repeats, the less likely it gets.", "0 is off."],
          ["DRY multiplier", "Turns DRY on, which punishes continuing a sequence already seen (not single words). The other DRY numbers appear only while it is on.", "0 is off; 0.8 is usual."],
          ["DRY base", "How fast the penalty grows with each extra token of the repeated sequence.", "1.75."],
          ["DRY allowed length", "Repeated sequences shorter than this are not punished.", "2."],
          ["DRY window", "How many recent tokens DRY searches.", "0 searches the whole history, up to 8192 tokens."],
          ["Seed", "Where the random generator starts when the deployment loads. The same seed, model and requests in the same order give the same answers.", "POM's default."],
          ["Reasoning budget", "How many tokens a thinking block may take; at the limit, the end of thinking is forced.", "Empty is no limit; 0 closes the block at once."],
          ["Disable thinking", "The model answers directly, without the preamble between tags.", "Turn on when only the answer matters."],
        ],
      },
      { kind: "h", text: "Speculation" },
      {
        kind: "p",
        text: "Speculation drafts the next tokens and checks them all in one batch. When the draft is right, one round through the pipeline yields several tokens instead of one. Only one mode runs at a time.",
      },
      {
        kind: "table",
        head: ["Property", "What it does"],
        rows: [
          ["Speculation", "None; MTP (draft), which drafts with the model's own trained head (nextn); or Speculative (lookup), which repeats text already in the context and only pays off on repetitive output."],
          ["MTP draft tokens", "How many tokens the nextn head guesses per round. More widens the batch on every hop; fewer wastes less when a guess is rejected."],
          ["MTP minimum probability", "Confidence floor for continuing the draft. Lower accepts more guesses, weak ones included; 0 accepts all."],
          ["Where MTP runs", "Automatic: VRAM only if the whole model still fits with it. VRAM: MTP takes its share first and the model uses the rest. RAM: MTP runs on the CPU and spends no VRAM."],
          ["MTP draft (tail)", "Shown when the model has no nextn head: pick the external draft (mtp-*.gguf) the last machine loads."],
          ["Draft width / Lookup window", "Lookup only: how many guesses per batch, and the longest n-gram searched for a prior repeat."],
        ],
      },
      {
        kind: "warn",
        title: "Measure MTP on a MoE model with experts in RAM",
        text: "The verification round processes several tokens at once. With the experts in RAM it fetches many more experts than a single token does, and generation can end up slower than without speculation. Compare tok/s in the telemetry before keeping it.",
      },
      { kind: "h", text: "Changing a property" },
      {
        kind: "steps",
        items: [
          { title: "Open the inspector", text: "In Deploy, pick the model and the machines. The inspector shows the values the deployment will load with. The [i] icon left of each name explains the field." },
          { title: "Change the value", text: "Type the number or tick the box. A field that depends on another (the DRY numbers, the MTP ones) appears once the other turns it on." },
          { title: "Check the allocation preview", text: "It redoes each machine's memory math with the new values. If a machine goes over, adjust before deploying." },
          { title: "Deploy or redeploy", text: "For a deployment already running, use Redeploy in History: the new version loads with the changed parameters and History keeps the previous one." },
        ],
      },
      {
        kind: "faq",
        items: [
          { q: "The deployment does not start for lack of VRAM. What do I change first?", a: "Turn on Fit to memory, under POM settings. Then drop the Micro-batch to 256 and use a quantized K/V cache. Lower Context or Sessions only as a last resort." },
          { q: "The client's max_tokens or Max. tokens: which wins?", a: "The client's max_tokens bounds the response. Max. tokens only splits generation into legs inside the gateway." },
          { q: "Does changing sampling require reloading the model?", a: "The values travel with the deployment, so the change goes into a new version (Redeploy)." },
        ],
      },
      { kind: "deep", text: "Deployment, memory fit and slice lifecycle", doc: "docs/subsystems/deployment.md" },
      { kind: "deep", text: "Inference pipeline, prefill and speculation", doc: "docs/subsystems/inference.md" },
    ],
  },
  {
    id: DEPLOY_SETTINGS_GUIDES.pom,
    label: "POM settings",
    summary: "What exists because inference is split across machines",
    title: "POM settings",
    blocks: [
      {
        kind: "lead",
        text: "llama.cpp runs a model on one machine. These settings exist because POM splits the model into slices on different machines, and has to decide how each slice fits in memory and how state passes from one to the next.",
      },
      {
        kind: "table",
        head: ["Property", "What it does", "When to change it"],
        rows: [
          ["Disable embeddings", "Normally automatic: only the last peer skips hidden-state extraction. Turned on by hand, it stops the first peer from extracting it, and the pipeline stops working.", "Almost never."],
          ["Fit to memory", "Before loading, the node reads each GPU's free memory, keeps a safety margin (256 MiB by default) and runs llama.cpp's fit for Context × Sessions. When it does not fit, it lowers the GPU layers or sends part to RAM instead of failing out of VRAM.", "Leave it on. GPU layers other than 99, or MoE experts in RAM, turn the layer fit off."],
          ["Chunks in flight", "How many prefill chunks travel the pipeline at once (1 to 8).", "Higher hides per-hop latency on a deep pipeline; 1 turns the overlap off."],
          ["Merge neighboring shards on the same machine", "Two adjacent ranges on one machine become one file before loading.", "Turn on when a machine got neighboring ranges: it saves a hop."],
          ["Hidden-state precision", "Bytes sent between peers per token. f16 is the default; f32 is lossless and doubles the volume; f8 and groupwise int4 are lossy and need every peer to support them.", "Shown only when the pipeline crosses machines. Try f8 on a slow link."],
        ],
      },
      {
        kind: "warn",
        title: "Without the memory fit",
        text: "With Fit to memory off, llama.cpp tries to put the whole slice, the KV cache and the compute buffers in VRAM. When that does not fit, the node fails to start and the supervisor retries in a loop. Off, the Micro-batch no longer applies either: the Batch does.",
      },
      { kind: "h", text: "Recipes" },
      {
        kind: "steps",
        items: [
          { title: "Fit a large slice", text: "Turn on Fit to memory, drop the Micro-batch to 256 and check the allocation preview. If the slice still does not fit and the model is MoE, see the Beta group right below." },
          { title: "Pipeline over a slow link", text: "Raise Chunks in flight (up to 8) to overlap prefill across hops. If every peer advertises support, switch Hidden-state precision to f8." },
          { title: "Two neighboring ranges on one machine", text: "Turn on Merge neighboring shards: the machine serves the union from one file, and the token stops crossing the network between the two ranges." },
        ],
      },
      {
        kind: "faq",
        items: [
          { q: "Does Fit to memory make inference slower?", a: "Only when it has to send layers or experts to RAM, because the alternative is a slice that does not start. When everything fits, it changes nothing." },
          { q: "Where do I see what the fit decided?", a: "In the allocation preview, before deploying, and in the node's load log, on the lines starting with llama.cpp fit." },
          { q: "What is the hidden state?", a: "The activation vector one slice hands to the next for every token. It is what crosses the network in the pipeline." },
        ],
      },
      { kind: "deep", text: "Memory fit, merge and the deployment plan", doc: "docs/subsystems/deployment.md" },
      { kind: "deep", text: "Inference pipeline and hidden-state precision", doc: "docs/subsystems/inference.md" },
    ],
  },
  {
    id: DEPLOY_SETTINGS_GUIDES.beta,
    label: "Beta settings",
    summary: "MoE experts in RAM, the GPU expert cache and automatic prefill",
    title: "Beta settings",
    blocks: [
      {
        kind: "lead",
        text: "These three settings exist to run a MoE model that does not fit whole in VRAM. They work, but they are still being measured: the best value changes from model to model and machine to machine.",
      },
      {
        kind: "p",
        text: "In a MoE (mixture of experts) model, each layer has many experts and each token uses only a few. The experts are most of the weights. That is why they can stay in RAM while VRAM keeps the rest of the model and the experts used recently.",
      },
      {
        kind: "table",
        head: ["Property", "What it does"],
        rows: [
          ["MoE experts in RAM", "Keeps every routed expert in RAM, computed by the CPU (llama.cpp's --cpu-moe), and gives VRAM to the rest of the model. Turns the layer fit off."],
          ["Expert cache on GPU (MiB)", "VRAM that keeps the recently used experts held in RAM: only misses cross PCIe, for batches of up to 32 tokens (generation; prefill keeps the regular offload). 0 turns it off. Per GPU: with two cards, each caches the experts of its own layers."],
          ["Automatic prefill chunk", "The gateway picks each prompt's chunk size, between 32 tokens and the Prefill chunk, from what it measured on the route; at least 64 while the expert cache is on. Off, every prompt goes in chunks of exactly the Prefill chunk."],
        ],
      },
      {
        kind: "stats",
        items: [
          { value: "14.95 tok/s", label: "Qwen3.8-Flash-Next UD-IQ3_XXS (78 GB), one 16 GB GPU, all experts in RAM" },
          { value: "19.26 tok/s", label: "The same, with an 8000 MiB expert cache" },
        ],
      },
      { kind: "h", text: "Combining them" },
      {
        kind: "table",
        head: ["Combination", "Experts in RAM", "Cache (MiB)", "Fit to memory", "When"],
        rows: [
          ["Everything in VRAM", "off", "0", "on", "The model fits in VRAM. The fastest."],
          ["The fit decides, with a cache", "off", "4000 to 8000", "on", "The model almost fits. The fit sends to RAM only what does not fit, and the cache serves the recently used experts. If everything fits, the node turns the cache off itself."],
          ["All experts in RAM, with a cache", "on", "the largest that fits", "(off because of experts in RAM)", "The model is much larger than VRAM. VRAM goes to attention, KV, MTP and the cache."],
          ["All experts in RAM, no cache", "on", "0", "(off because of experts in RAM)", "Little VRAM. The CPU computes every expert; the slowest."],
        ],
      },
      {
        kind: "note",
        title: "How much cache?",
        text: "Give the cache the VRAM left after the weights, the KV cache, the compute buffers and MTP. If the context does not fit, the node halves the cache and retries, turning it off below 512 MiB. In the load log, the line MoE cache size = X for Y of host experts tells how much of the experts in RAM the cache covers; on unload, the llama_moe_cache lines show the hit rate.",
      },
      {
        kind: "warn",
        title: "The cache only holds experts that are in RAM",
        text: "With no experts in RAM (neither from the fit nor from MoE experts in RAM), the cache has nothing to hold and changes nothing. And with the experts in RAM, MTP can make generation slower: measure before keeping it.",
      },
      { kind: "h", text: "Step by step" },
      {
        kind: "steps",
        items: [
          { title: "Start from the default", text: "Fit to memory on, MoE experts in RAM off and cache 0. Deploy and check in the preview and the log whether the fit sent experts to RAM." },
          { title: "Turn the cache on", text: "If it did, set the Expert cache on GPU (somewhere between 4000 and 8000 MiB per GPU) and redeploy. Dropping the Micro-batch to 256 frees VRAM for it." },
          { title: "Model much larger than VRAM", text: "Turn on MoE experts in RAM and give the cache the largest value that fits." },
          { title: "Compare", text: "Use the logs telemetry to compare generation and prefill tok/s across versions. History keeps every version with the parameters it loaded with." },
        ],
      },
      {
        kind: "faq",
        items: [
          { q: "Does it work with two GPUs in one machine?", a: "Yes. The cache is per GPU: both cards keep their share of the layers, and each caches the experts of its own layers." },
          { q: "Why is prefill still slow with the cache?", a: "The cache serves batches of up to 32 tokens, which is generation. Prefill goes through the regular offload of the experts in RAM." },
          { q: "When should I turn the Automatic prefill chunk off?", a: "To compare measurements with a fixed chunk, or on a route where the automatic choice does not help. Off, the Prefill chunk always applies." },
        ],
      },
      { kind: "deep", text: "MoE experts in RAM and the GPU expert cache", doc: "docs/subsystems/deployment.md" },
    ],
  },
];
