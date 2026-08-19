/**
 * The human half of the documentation.
 *
 * The markdown under `src/docs/` is written for agents: exhaustive, file- and
 * function-level, and true to the code down to the constant. It is the wrong
 * thing to hand a person who has just opened the panel and wants to know what
 * this cluster does. These guides are the other half - a small, ordered set of
 * explained pages with diagrams, written to be read once, front to back.
 *
 * They are content, not prose embedded in components: a guide is data
 * (`Block[]`), so the renderer owns every spacing and colour decision and the
 * two locales cannot drift into different layouts. Every guide ends with
 * `deep` links into the markdown reference, because the reference is where the
 * detail lives and duplicating it here would guarantee it goes stale.
 *
 * The rule that keeps this honest: **nothing here may state a number or a
 * behaviour that the markdown does not.** When they disagree, the markdown
 * (which tracks the code) wins and this file is corrected.
 */
import type { Locale } from "../i18n/locales";

export type Block =
  /** Opening sentence, set larger than the body. One per guide. */
  | { kind: "lead"; text: string }
  | { kind: "p"; text: string }
  | { kind: "h"; text: string }
  /** Numbered sequence where the order is the point. */
  | { kind: "steps"; items: { title: string; text: string }[] }
  /** Peer concepts, no order implied. */
  | { kind: "cards"; items: { title: string; text: string }[] }
  | { kind: "note"; title: string; text: string }
  | { kind: "warn"; title: string; text: string }
  | { kind: "table"; head: string[]; rows: string[][] }
  | { kind: "code"; code: string }
  | { kind: "diagram"; id: DiagramId; caption: string }
  /** "The full detail is over there" - a link into the markdown reference. */
  | { kind: "deep"; text: string; doc: string };

export type DiagramId = "topology" | "request" | "slicing" | "lifecycle";

export type Guide = {
  id: string;
  /** Sidebar label. Short - it sits in a narrow column. */
  label: string;
  /** One line under the title, and the sidebar subtitle. */
  summary: string;
  title: string;
  blocks: Block[];
};

const ptBR: Guide[] = [
  {
    id: "visao-geral",
    label: "O que é esta plataforma",
    summary: "Máquinas comuns servindo um modelo grande, juntas",
    title: "O que é esta plataforma",
    blocks: [
      {
        kind: "lead",
        text: "Um modelo de linguagem que não cabe numa máquina é cortado em fatias de camadas, e cada máquina serve a sua. As máquinas se acham sozinhas na rede, combinam quem fica com qual faixa, e passam a responder como se fossem uma só.",
      },
      {
        kind: "p",
        text: "Não há servidor central. Todo nó roda o mesmo binário, todo nó serve esta interface e a API, e qualquer um deles pode receber a requisição de quem está perguntando. O que muda entre eles é a faixa de camadas que carregaram na memória.",
      },
      { kind: "diagram", id: "topology", caption: "Três máquinas cobrindo um modelo de 32 camadas. Quem recebe a pergunta não precisa ser quem tem a camada 0." },
      { kind: "h", text: "As quatro peças" },
      {
        kind: "cards",
        items: [
          { title: "Nó", text: "Uma máquina no cluster. Anuncia quem é, quanta memória tem e qual faixa de camadas serve. Descobre os outros por UDP na rede local e por libp2p pela internet." },
          { title: "Fatia", text: "Um arquivo GGUF com uma faixa de camadas do modelo original. Fatiar é local: cada máquina extrai a própria faixa da própria cópia, nenhum peso viaja pela rede." },
          { title: "Pipeline", text: "A cadeia ordenada de nós que cobre da camada 0 até a última. Sem cobertura completa não há resposta - o planejador recusa em vez de inventar." },
          { title: "Gateway", text: "A porta de entrada HTTP, servida por todos os nós: a interface, a API compatível com OpenAI e a rota de geração." },
        ],
      },
      { kind: "h", text: "Por que fatiar por camadas" },
      {
        kind: "p",
        text: "As camadas de um modelo transformer rodam em sequência: a saída de uma é a entrada da seguinte. Isso permite cortar o modelo em pontos limpos e distribuir os pedaços, desde que os pedaços rodem na ordem certa. O que trafega entre as máquinas não são pesos nem texto, e sim o estado oculto - um vetor por token.",
      },
      {
        kind: "note",
        title: "O tamanho da fatia é decisão da máquina, não do modelo",
        text: "As fatias não têm o mesmo tamanho. Quem manda é a memória da máquina que vai recebê-la, e as máquinas de um cluster real não se parecem. A tela de fatiar trabalha com um alvo em GB por fatia e converte isso em faixa de camadas.",
      },
      { kind: "deep", text: "Descoberta, endereços anunciados e identidade dos nós", doc: "docs/subsystems/rede-p2p.md" },
      { kind: "deep", text: "Mapa do repositório e invariantes do projeto", doc: "AGENTS.md" },
    ],
  },
  {
    id: "requisicao",
    label: "O caminho de uma pergunta",
    summary: "Do prompt ao token, máquina por máquina",
    title: "O caminho de uma pergunta",
    blocks: [
      {
        kind: "lead",
        text: "Entender esta página é entender de onde vem a latência do cluster, e por que a resposta chega no ritmo em que chega.",
      },
      { kind: "diagram", id: "request", caption: "O prompt entra por qualquer nó; a última máquina da cadeia responde direto a quem perguntou." },
      {
        kind: "steps",
        items: [
          { title: "A pergunta chega", text: "Qualquer nó aceita. Ele monta o plano do pipeline: quem tem a camada 0, quem tem a seguinte, até fechar o modelo. Se falta faixa, a requisição falha com a explicação - não existe um modo degradado que responda pior sem avisar." },
          { title: "A cabeça lê o prompt inteiro", text: "A máquina que tem a camada 0 tokeniza e processa o prompt em pedaços. É a etapa mais cara da requisição, e é por isso que a camada 0 deve ficar na máquina com mais memória." },
          { title: "O estado oculto atravessa a cadeia", text: "Cada máquina roda as suas camadas e entrega o resultado à seguinte. São 8.192 bytes por token por salto num modelo de dimensão 4096, porque o vetor viaja em f16 e não em texto." },
          { title: "A cauda escolhe o token", text: "A última máquina da cadeia é a única que amostra: converte o estado oculto no token seguinte." },
          { title: "A resposta volta direto", text: "A cauda entrega o token diretamente a quem recebeu a pergunta, em vez de refazer a cadeia de volta. São N+1 travessias por token em vez de 2N - o motivo de a ordem das máquinas ter deixado de importar para o desempenho." },
        ],
      },
      { kind: "h", text: "O que isso custa, medido" },
      {
        kind: "table",
        head: ["Grandeza", "Valor medido", "De onde vem"],
        rows: [
          ["Estado oculto por token, por salto", "8.192 bytes", "4096 dimensões × 2 bytes (f16)"],
          ["Tokens amostrados de volta", "~200 bytes por token", "só o token, não o vetor"],
          ["Tempo até a primeira palavra", "290 ms", "medido no par MacBook + GPU, prompt curto"],
          ["Salto de rede a mais por token", "1 RTT", "cada máquina extra na cadeia"],
        ],
      },
      {
        kind: "warn",
        title: "O prompt tem orçamento, e passar dele é recusa",
        text: "O limite é a janela de contexto dividida pelas sessões, menos os tokens que a resposta pode gastar. O gateway mede o prompt antes e recusa com uma mensagem citando os parâmetros - antes disso, um prompt grande demais derrubava o processo inteiro.",
      },
      {
        kind: "note",
        title: "Decodificação especulativa",
        text: "Quando o modelo permite, a cabeça arrisca vários tokens de uma vez e a cauda confirma ou descarta o lote. Acertou, vários tokens saem pelo preço de uma travessia; errou, o cache volta atrás e nada de errado sai. Recusada uma vez, o gateway para de pedir pelo resto da requisição.",
      },
      { kind: "deep", text: "Prefill, decode, wire format e especulativa em detalhe", doc: "docs/subsystems/inferencia.md" },
      { kind: "deep", text: "Pipeline e tensor parallelism: quando usar cada um", doc: "docs/subsystems/paralelismo.md" },
      { kind: "deep", text: "Rotas, autenticação e a prova de execução distribuída", doc: "docs/subsystems/api-http.md" },
    ],
  },
  {
    id: "fatiar-implantar",
    label: "Do modelo ao cluster servindo",
    summary: "Fatiar por tamanho, distribuir, pôr em serviço",
    title: "Do modelo ao cluster servindo",
    blocks: [
      {
        kind: "lead",
        text: "São três atos distintos, e vale separá-los na cabeça: cortar o arquivo, levar os pedaços às máquinas certas, e mandar as máquinas carregarem o que receberam.",
      },
      { kind: "diagram", id: "slicing", caption: "Um GGUF de 32 camadas cortado por alvo de tamanho, não em partes iguais." },
      { kind: "h", text: "1. Fatiar (aba Modelos)" },
      {
        kind: "p",
        text: "Você monta uma lista de fatias e dá a cada uma um alvo em GB; a última pode simplesmente levar o resto. A tela converte GB em faixa de camadas somando o tamanho real de cada bloco do arquivo - não uma divisão do total pelo número de camadas, porque os blocos não são iguais entre si e o embedding viaja em toda fatia.",
      },
      {
        kind: "note",
        title: "O arredondamento fica à vista",
        text: "Camada é unidade indivisível, então o que você pediu e o que saiu quase nunca batem exatamente. A tela mostra os três números lado a lado: o alvo pedido, a faixa que saiu e o tamanho real do arquivo.",
      },
      { kind: "h", text: "2. Distribuir (aba Transferências)" },
      {
        kind: "p",
        text: "Uma fatia chega à outra máquina de dois jeitos: ela puxa, ou o dono do arquivo oferece. Uma oferta aparece como notificação do outro lado e só começa depois de aceita - nunca se escreve no disco alheio sem convite.",
      },
      {
        kind: "cards",
        items: [
          { title: "Retomada", text: "O arquivo baixa como .part e continua de onde parou. Uma fatia de 10 GB leva por volta de 17 minutos num enlace comum; interromper não significa recomeçar." },
          { title: "Conferência", text: "O MD5 é verificado antes de o arquivo tomar o nome final. Não bateu, o parcial é apagado." },
          { title: "Espaço em disco", text: "Verificado antes de começar, e a implantação conta a união das fatias mais o que ainda falta." },
        ],
      },
      { kind: "h", text: "3. Implantar (tela Implantar na rede)" },
      {
        kind: "p",
        text: "Aqui você escolhe qual máquina serve qual faixa e manda o cluster carregar. O nó sugere uma distribuição; a sua escolha viaja na requisição e o cluster é revarrido na hora de aplicar, em vez de confiar na prévia.",
      },
      {
        kind: "table",
        head: ["Regra", "Por quê"],
        rows: [
          ["A ordem do pipeline é a ordem das camadas", "quem fica com a camada 0 recebe o prompt inteiro de cada requisição; escolher a ordem e escolher as faixas são o mesmo ato"],
          ["Duas faixas na mesma máquina só com fusão", "com fusão ligada, faixas vizinhas viram um arquivo só e a máquina serve a união"],
          ["Sobreposição é recusa", "a mesma camada rodando duas vezes corrompe a resposta"],
          ["Buraco é aviso, não erro", "implantar meio modelo é legítimo; ele só não forma pipeline até fechar"],
        ],
      },
      {
        kind: "note",
        title: "Contexto e batch valem para o cluster; GPU vale por máquina",
        text: "A janela de contexto e o tamanho do lote governam a cadeia inteira e precisam ser iguais em todos. Já quantas camadas vão para a GPU e onde fica o cache de atenção são propriedade do hardware - uma máquina com 16 GB de VRAM e outra sem GPU não podem estar as duas certas sobre isso, então cada uma tem o seu campo.",
      },
      { kind: "deep", text: "Fatiamento, tabela de tipos e planejamento por tamanho", doc: "docs/subsystems/fatiamento.md" },
      { kind: "deep", text: "Implantação, fusão e ciclo de vida da fatia", doc: "docs/subsystems/implantacao.md" },
      { kind: "deep", text: "Pipeline e tensor parallelism", doc: "docs/subsystems/paralelismo.md" },
      { kind: "deep", text: "Transferência entre máquinas", doc: "docs/subsystems/transferencia.md" },
    ],
  },
  {
    id: "operar",
    label: "Operar e diagnosticar",
    summary: "Subir, conferir e achar o que quebrou",
    title: "Operar e diagnosticar",
    blocks: [
      {
        kind: "lead",
        text: "Um cluster distribuído falha de formas que uma máquina só não conhece: metade da rede enxerga a outra, uma fatia não carregou, um endereço mudou. Esta página é o roteiro de conferência.",
      },
      { kind: "diagram", id: "lifecycle", caption: "O ciclo de vida de uma fatia: só o estado que o próprio nó confirmou autoriza retomá-la depois de um reinício." },
      { kind: "h", text: "A prova de que está funcionando" },
      {
        kind: "p",
        text: "Toda resposta carrega a cadeia que a produziu. Uma origem começando por pipeline: e listando dois ou mais saltos é a única prova de que a inferência foi conjunta. Um salto só, ou nenhuma origem, significa que o planejador não fechou a cobertura das camadas.",
      },
      { kind: "code", code: 'curl -s localhost:8080/peers | python3 -m json.tool\ncurl -sf localhost:8181/health' },
      { kind: "h", text: "Sintomas e onde olhar" },
      {
        kind: "table",
        head: ["O que você vê", "Causa provável", "Onde confirmar"],
        rows: [
          ["As máquinas não se enxergam", "descoberta não saiu do nó, ou endereço anunciado errado", "a tela de Rede e a lista de nós do painel"],
          ["Origem “none” na resposta", "a cobertura de camadas tem buraco", "a tela Implantar mostra a faixa que falta"],
          ["Um nó aparece sem fatia", "o modelo não carregou, ou a porta de inferência está ocupada", "o card da máquina no painel diz o motivo"],
          ["A resposta demora muito para começar", "a máquina com a camada 0 está processando o prompt", "mova a camada 0 para a máquina com mais memória"],
          ["Uma fatia não volta depois de reiniciar", "ela nunca chegou a carregar antes de cair", "o card mostra por que a retomada foi recusada"],
        ],
      },
      {
        kind: "warn",
        title: "Duas máquinas, dois lares",
        text: "Rodar dois nós no mesmo diretório faz os dois carregarem a mesma identidade e o cluster os trata como uma máquina só. Portas diferentes não bastam: o segundo nó precisa de um diretório de trabalho próprio.",
      },
      {
        kind: "note",
        title: "A interface é lida do disco",
        text: "Editar a interface exige reconstruí-la; recompilar o binário não reconstrói a tela. Vale o inverso também: atualizar o binário não exige tocar na interface.",
      },
      { kind: "deep", text: "Subida, verificação e pegadinhas de campo", doc: "docs/subsystems/operacao.md" },
      { kind: "deep", text: "Build, features de GPU e variáveis de ambiente", doc: "docs/subsystems/build-execucao.md" },
    ],
  },
  {
    id: "acesso",
    label: "Acesso e o que fica exposto",
    summary: "Chaves, papéis e os limites honestos",
    title: "Acesso e o que fica exposto",
    blocks: [
      {
        kind: "lead",
        text: "O cluster foi desenhado para uma rede confiável. Vale saber exatamente o que isso quer dizer antes de apontar uma porta para fora.",
      },
      { kind: "h", text: "As duas credenciais" },
      {
        kind: "cards",
        items: [
          { title: "Chave de conexão", text: "É a mesma no cluster inteiro e identifica o operador. Autentica as rotas administrativas, as de implantação e também as de inferência. É ela que o nó novo recebe para entrar na rede." },
          { title: "Chave de API", text: "Emitida para quem só vai consumir o modelo. Aparece nos logs por token, então dá para ver quanto cada cliente pediu e a partir de qual nó." },
        ],
      },
      {
        kind: "p",
        text: "As rotas compatíveis com OpenAI aceitam qualquer uma das duas: apontar um harness para o cluster é escolher a URL de um nó e mandar a chave no cabeçalho.",
      },
      { kind: "h", text: "O que ainda não está protegido" },
      {
        kind: "warn",
        title: "A porta entre as máquinas não tem autenticação nem TLS",
        text: "O tráfego de estado oculto entre os nós roda em HTTP simples. Numa LAN confiável é aceitável; numa rede aberta, não. Mantenha essa porta fechada para fora e trate o cluster como um segmento privado.",
      },
      {
        kind: "note",
        title: "Bloquear uma máquina",
        text: "Um nó pode ser bloqueado pela identidade, e a identidade dele é derivada de uma chave que vive no diretório de trabalho da máquina. Apagar essa chave cria uma máquina nova aos olhos das outras.",
      },
      { kind: "deep", text: "Autenticação, papéis e todas as rotas", doc: "docs/subsystems/api-http.md" },
      { kind: "deep", text: "Limitações conhecidas para a internet", doc: "docs/subsystems/rede-p2p.md" },
    ],
  },
];

const en: Guide[] = [
  {
    id: "visao-geral",
    label: "What this platform is",
    summary: "Ordinary machines serving one large model, together",
    title: "What this platform is",
    blocks: [
      {
        kind: "lead",
        text: "A language model that does not fit on one machine is cut into slices of layers, and each machine serves its own. The machines find each other on the network, agree on who takes which range, and start answering as if they were one.",
      },
      {
        kind: "p",
        text: "There is no central server. Every node runs the same binary, every node serves this interface and the API, and any of them can take the incoming request. What differs between them is the range of layers they hold in memory.",
      },
      { kind: "diagram", id: "topology", caption: "Three machines covering a 32-layer model. Whoever takes the question need not be the one holding layer 0." },
      { kind: "h", text: "The four pieces" },
      {
        kind: "cards",
        items: [
          { title: "Node", text: "One machine in the cluster. It announces who it is, how much memory it has and which layer range it serves. It finds the others over UDP on the local network and over libp2p across the internet." },
          { title: "Slice", text: "A GGUF file holding a range of layers of the original model. Slicing is local: each machine extracts its own range from its own copy, and no weights travel the network." },
          { title: "Pipeline", text: "The ordered chain of nodes covering layer 0 through the last one. Without full coverage there is no answer - the planner refuses rather than improvising." },
          { title: "Gateway", text: "The HTTP front door, served by every node: this interface, the OpenAI-compatible API and the generation route." },
        ],
      },
      { kind: "h", text: "Why slice by layers" },
      {
        kind: "p",
        text: "Transformer layers run in sequence: each one's output is the next one's input. That allows cutting the model at clean points and spreading the pieces, as long as the pieces run in the right order. What crosses the network is neither weights nor text, but the hidden state - one vector per token.",
      },
      {
        kind: "note",
        title: "Slice size is the machine's decision, not the model's",
        text: "Slices are not equal. What decides is the memory of the machine that will hold it, and the machines in a real cluster do not look alike. The slicing screen works from a size target in GB per slice and converts that into a layer range.",
      },
      { kind: "deep", text: "Discovery, advertised addresses and node identity", doc: "docs/subsystems/rede-p2p.md" },
      { kind: "deep", text: "Repository map and project invariants", doc: "AGENTS.md" },
    ],
  },
  {
    id: "requisicao",
    label: "The path of a question",
    summary: "From prompt to token, machine by machine",
    title: "The path of a question",
    blocks: [
      {
        kind: "lead",
        text: "Understanding this page is understanding where the cluster's latency comes from, and why the answer arrives at the pace it does.",
      },
      { kind: "diagram", id: "request", caption: "The prompt enters through any node; the last machine in the chain answers the asker directly." },
      {
        kind: "steps",
        items: [
          { title: "The question arrives", text: "Any node accepts it. It builds the pipeline plan: who holds layer 0, who holds the next range, until the model is covered. If a range is missing the request fails with the reason - there is no degraded mode that quietly answers worse." },
          { title: "The head reads the whole prompt", text: "The machine holding layer 0 tokenizes and processes the prompt in chunks. It is the most expensive step of the request, which is why layer 0 belongs on the machine with the most memory." },
          { title: "The hidden state crosses the chain", text: "Each machine runs its layers and hands the result to the next. That is 8,192 bytes per token per hop on a 4096-dimension model, because the vector travels as f16 rather than text." },
          { title: "The tail picks the token", text: "The last machine in the chain is the only one that samples: it turns the hidden state into the next token." },
          { title: "The answer goes straight back", text: "The tail delivers the token directly to whoever took the question instead of retracing the chain. That is N+1 crossings per token instead of 2N - the reason machine order stopped mattering for speed." },
        ],
      },
      { kind: "h", text: "What it costs, measured" },
      {
        kind: "table",
        head: ["Quantity", "Measured value", "Where it comes from"],
        rows: [
          ["Hidden state per token, per hop", "8,192 bytes", "4096 dimensions × 2 bytes (f16)"],
          ["Sampled tokens coming back", "~200 bytes per token", "the token only, not the vector"],
          ["Time to first word", "290 ms", "measured on the MacBook + GPU pair, short prompt"],
          ["Extra network hop per token", "1 RTT", "for each additional machine in the chain"],
        ],
      },
      {
        kind: "warn",
        title: "The prompt has a budget, and exceeding it is a refusal",
        text: "The limit is the context window divided by the sessions, minus the tokens the answer may spend. The gateway measures the prompt first and refuses with a message naming the settings - before that, an oversized prompt took the whole process down.",
      },
      {
        kind: "note",
        title: "Speculative decoding",
        text: "When the model allows it, the head guesses several tokens at once and the tail confirms or discards the batch. On a hit, several tokens come out for the price of one crossing; on a miss, the cache rolls back and nothing wrong escapes. Refused once, the gateway stops asking for the rest of the request.",
      },
      { kind: "deep", text: "Prefill, decode, wire format and speculation in detail", doc: "docs/subsystems/inferencia.md" },
      { kind: "deep", text: "Routes, authentication and the proof of distributed execution", doc: "docs/subsystems/api-http.md" },
    ],
  },
  {
    id: "fatiar-implantar",
    label: "From model to serving cluster",
    summary: "Slice by size, distribute, put into service",
    title: "From model to serving cluster",
    blocks: [
      {
        kind: "lead",
        text: "These are three separate acts, and it helps to keep them apart: cutting the file, getting the pieces to the right machines, and telling those machines to load what they received.",
      },
      { kind: "diagram", id: "slicing", caption: "A 32-layer GGUF cut by size target, not into equal parts." },
      { kind: "h", text: "1. Slice (Models tab)" },
      {
        kind: "p",
        text: "You build a list of slices and give each a target in GB; the last one can simply take the remainder. The screen converts GB into a layer range by summing the real size of each block in the file - not the total divided by the layer count, because blocks are not equal to one another and the embedding travels in every slice.",
      },
      {
        kind: "note",
        title: "The rounding stays visible",
        text: "A layer is indivisible, so what you asked for and what came out almost never match exactly. The screen shows all three numbers side by side: the target, the resulting range and the real file size.",
      },
      { kind: "h", text: "2. Distribute (Transfers tab)" },
      {
        kind: "p",
        text: "A slice reaches another machine in one of two ways: it pulls, or the file's owner offers. An offer shows up as a notification on the other side and only starts once accepted - nothing is ever written to someone else's disk uninvited.",
      },
      {
        kind: "cards",
        items: [
          { title: "Resume", text: "The file downloads as .part and picks up where it stopped. A 10 GB slice takes roughly 17 minutes on an ordinary link; interrupting it does not mean starting over." },
          { title: "Verification", text: "The MD5 is checked before the file takes its final name. If it does not match, the partial file is deleted." },
          { title: "Disk space", text: "Checked before starting, and deployment counts the union of the slices plus what is still missing." },
        ],
      },
      { kind: "h", text: "3. Deploy (Deploy to network screen)" },
      {
        kind: "p",
        text: "Here you choose which machine serves which range and tell the cluster to load. The node suggests a distribution; your choice travels with the request and the cluster is re-scanned at apply time rather than trusting the preview.",
      },
      {
        kind: "table",
        head: ["Rule", "Why"],
        rows: [
          ["Pipeline order is layer order", "whoever holds layer 0 receives the full prompt of every request; choosing the order and choosing the ranges are the same act"],
          ["Two ranges on one machine only with merge", "with merge on, adjacent ranges become a single file and the machine serves the union"],
          ["Overlap is a refusal", "the same layer running twice corrupts the answer"],
          ["A gap is a warning, not an error", "deploying half a model is legitimate; it just does not form a pipeline until it closes"],
        ],
      },
      {
        kind: "note",
        title: "Context and batch are cluster-wide; GPU is per machine",
        text: "The context window and batch size govern the whole chain and must match everywhere. How many layers go to the GPU and where the attention cache lives are properties of the hardware - a machine with 16 GB of VRAM and one without a GPU cannot both be right about that, so each has its own field.",
      },
      { kind: "deep", text: "Slicing, type table and size-based planning", doc: "docs/subsystems/fatiamento.md" },
      { kind: "deep", text: "Deployment, merge and slice lifecycle", doc: "docs/subsystems/implantacao.md" },
      { kind: "deep", text: "Transfers between machines", doc: "docs/subsystems/transferencia.md" },
    ],
  },
  {
    id: "operar",
    label: "Operate and diagnose",
    summary: "Bring it up, check it, find what broke",
    title: "Operate and diagnose",
    blocks: [
      {
        kind: "lead",
        text: "A distributed cluster fails in ways a single machine never does: half the network sees the other half, a slice did not load, an address changed. This page is the checklist.",
      },
      { kind: "diagram", id: "lifecycle", caption: "A slice's lifecycle: only the state the node itself confirmed may be resumed after a restart." },
      { kind: "h", text: "The proof that it works" },
      {
        kind: "p",
        text: "Every answer carries the chain that produced it. A source starting with pipeline: and listing two or more hops is the only proof that inference was joint. A single hop, or no source at all, means the planner did not close layer coverage.",
      },
      { kind: "code", code: 'curl -s localhost:8080/peers | python3 -m json.tool\ncurl -sf localhost:8181/health' },
      { kind: "h", text: "Symptoms and where to look" },
      {
        kind: "table",
        head: ["What you see", "Likely cause", "Where to confirm"],
        rows: [
          ["The machines do not see each other", "discovery never left the node, or the advertised address is wrong", "the Network screen and the dashboard node list"],
          ["Source “none” in the answer", "layer coverage has a gap", "the Deploy screen shows the missing range"],
          ["A node shows up with no slice", "the model did not load, or the inference port is taken", "the machine's card on the dashboard states the reason"],
          ["The answer takes long to start", "the machine holding layer 0 is chewing the prompt", "move layer 0 to the machine with the most memory"],
          ["A slice does not come back after a restart", "it never finished loading before it died", "the card shows why the resume was refused"],
        ],
      },
      {
        kind: "warn",
        title: "Two machines, two homes",
        text: "Running two nodes from the same directory makes both load the same identity, and the cluster treats them as one machine. Different ports are not enough: the second node needs its own working directory.",
      },
      {
        kind: "note",
        title: "The interface is read from disk",
        text: "Editing the interface requires rebuilding it; recompiling the binary does not rebuild the screen. The inverse holds too: updating the binary does not require touching the interface.",
      },
      { kind: "deep", text: "Bring-up, verification and field gotchas", doc: "docs/subsystems/operacao.md" },
      { kind: "deep", text: "Build, GPU features and environment variables", doc: "docs/subsystems/build-execucao.md" },
    ],
  },
  {
    id: "acesso",
    label: "Access and exposure",
    summary: "Keys, roles and the honest limits",
    title: "Access and exposure",
    blocks: [
      {
        kind: "lead",
        text: "The cluster was designed for a trusted network. It is worth knowing exactly what that means before pointing a port outward.",
      },
      { kind: "h", text: "The two credentials" },
      {
        kind: "cards",
        items: [
          { title: "Connection key", text: "The same across the whole cluster, and it identifies the operator. It authenticates the admin routes, the deployment routes and the inference ones too. It is what a new node receives to join." },
          { title: "API key", text: "Issued to whoever only consumes the model. It shows up in the per-token logs, so you can see how much each client asked for and through which node." },
        ],
      },
      {
        kind: "p",
        text: "The OpenAI-compatible routes accept either one: pointing a harness at the cluster is choosing a node's URL and sending the key in the header.",
      },
      { kind: "h", text: "What is not protected yet" },
      {
        kind: "warn",
        title: "The port between machines has neither authentication nor TLS",
        text: "Hidden-state traffic between nodes runs over plain HTTP. On a trusted LAN that is acceptable; on an open network it is not. Keep that port closed to the outside and treat the cluster as a private segment.",
      },
      {
        kind: "note",
        title: "Blocking a machine",
        text: "A node can be blocked by identity, and that identity derives from a key living in the machine's working directory. Deleting that key creates a new machine in the eyes of the others.",
      },
      { kind: "deep", text: "Authentication, roles and every route", doc: "docs/subsystems/api-http.md" },
      { kind: "deep", text: "Known limitations for the internet", doc: "docs/subsystems/rede-p2p.md" },
    ],
  },
];

const GUIDES: Record<Locale, Guide[]> = { en, "pt-BR": ptBR };

export function guidesFor(locale: Locale): Guide[] {
  return GUIDES[locale] ?? en;
}
