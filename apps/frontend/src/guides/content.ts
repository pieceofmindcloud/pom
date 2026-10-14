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
import { deploySettingsEn, deploySettingsPtBR } from "./deploySettings";

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
  /** Two to four headline numbers, each with what it measures. */
  | { kind: "stats"; items: { value: string; label: string }[] }
  /** Things to confirm, in any order; rendered with check marks. */
  | { kind: "checklist"; title: string; items: string[] }
  /** Questions a reader asks next; each answer opens on demand. */
  | { kind: "faq"; items: { q: string; a: string }[] }
  /** "The full detail is over there" - a link into the markdown reference. */
  | { kind: "deep"; text: string; doc: string };

export type DiagramId =
  | "topology"
  | "request"
  | "slicing"
  | "lifecycle"
  | "setup"
  | "profiles"
  | "network"
  | "api"
  | "plugins";

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
    id: "overview",
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
        text: "Todo nó roda o mesmo binário, todo nó serve esta interface e a API, e qualquer um deles pode receber a requisição de quem está perguntando. O que muda entre eles é a faixa de camadas que carregaram na memória e o papel que cada um tem na rede.",
      },
      { kind: "diagram", id: "topology", caption: "Três máquinas cobrindo um modelo de 32 camadas. Quem recebe a pergunta não precisa ser quem tem a camada 0." },
      { kind: "h", text: "As quatro peças" },
      {
        kind: "cards",
        items: [
          { title: "Nó", text: "Uma máquina no cluster. Anuncia quem é, quanta memória tem e qual faixa de camadas serve. Descobre os outros por UDP na rede local e por libp2p pela internet." },
          { title: "Fatia", text: "Um arquivo GGUF com uma faixa de camadas do modelo original. Fatiar é local: cada máquina extrai a própria faixa da própria cópia, nenhum peso viaja pela rede por causa disso." },
          { title: "Pipeline", text: "A cadeia ordenada de nós que cobre da camada 0 até a última. Sem cobertura completa não há resposta - a requisição falha com a explicação em vez de responder pior." },
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
      {
        kind: "p",
        text: "Se é a primeira vez aqui, siga os guias na ordem: primeiros passos, perfis, o caminho de uma pergunta e, por fim, do modelo ao cluster servindo.",
      },
      { kind: "deep", text: "Descoberta, endereços anunciados e identidade dos nós", doc: "docs/subsystems/p2p-network.md" },
      { kind: "deep", text: "Mapa do repositório e invariantes do projeto", doc: "AGENTS.md" },
    ],
  },
  {
    id: "getting-started",
    label: "Primeiros passos",
    summary: "Da primeira máquina a uma rede com duas",
    title: "Primeiros passos",
    blocks: [
      {
        kind: "lead",
        text: "Uma rede começa com uma máquina administradora. As outras entram por convite. Nenhum modelo é passado na linha de comando: tudo depois do primeiro arranque acontece por esta interface.",
      },
      { kind: "diagram", id: "setup", caption: "O administrador cria a rede e gera o convite. A máquina nova acha o administrador pela descoberta local, ou recebe o convite colado, e passa de entrando para conectada." },
      {
        kind: "steps",
        items: [
          { title: "Inicie o binário", text: "Rode o executável pom sem argumentos. Sem arquivo de configuração, o nó sobe em modo de configuração e espera pelo navegador." },
          { title: "Abra a interface", text: "Acesse http://<máquina>:8080/. A interface e a API sempre ficam na porta do gateway, que é 8080 por padrão." },
          { title: "Escolha o papel", text: "A primeira máquina escolhe Administrador. O assistente pede só o nome da máquina, o nome da rede e se ela é privada ou pública." },
          { title: "Guarde o convite", text: "No último passo o administrador recebe o convite: a chave de conexão (em rede privada) e o arquivo trust.json. É com eles que as outras máquinas entram." },
          { title: "Junte a segunda máquina", text: "Na outra máquina, rode o mesmo binário, abra a interface e escolha Nó. Na mesma rede local, o assistente acha o administrador sozinho; senão, cole o trust.json e a chave." },
        ],
      },
      {
        kind: "note",
        title: "Enquanto o assistente roda, o administrador vê quem chega",
        text: "No passo do convite, cada computador procurando a rede aparece com nome, sistema, IP, CPU, RAM e GPU, e passa de entrando para conectado quando recebe a raiz. Terminar sem ninguém também é normal.",
      },
      {
        kind: "warn",
        title: "Troque a senha padrão",
        text: "Depois da configuração, a senha do painel é admin e a interface mostra um aviso fixo até você trocá-la em Preferências. Pastas, reservas de memória e limite térmico vêm de padrões da máquina e também podem ser ajustados depois.",
      },
      {
        kind: "p",
        text: "Com as duas máquinas na rede, o próximo passo é ter um modelo: traga um GGUF para a pasta de modelos ou use Encontrar Modelos HF, e siga o guia Do modelo ao cluster servindo.",
      },
      {
        kind: "note",
        title: "Máquinas em redes diferentes",
        text: "Fora da mesma rede local, a descoberta automática não alcança o administrador. Inicie o nó com --bootstrap <administrador>:42777 e veja o guia Rede local e internet.",
      },
      { kind: "checklist", title: "Pronto para o próximo passo quando", items: ["As duas máquinas aparecem no Painel.", "A senha padrão do painel foi trocada em Preferências.", "Há um modelo GGUF na pasta de modelos, ou um download concluído em Transferências."] },
      { kind: "faq", items: [{q: "Preciso abrir portas no roteador?", a: "Na mesma rede local, não: a descoberta por UDP basta. Entre redes diferentes, veja o guia Rede local e internet, que lista as portas e o endereço anunciado."}, {q: "Posso rodar dois nós na mesma máquina?", a: "Sim, desde que cada um tenha o próprio diretório de trabalho (POM_HOME) e portas próprias, por exemplo --port 42778 --gateway-port 8091 no segundo."}, {q: "Perdi o convite. E agora?", a: "No administrador, Configurações > Compartilhamento mostra de novo a raiz de confiança e a chave de conexão."}] },
      { kind: "deep", text: "Subida de um cluster de duas máquinas e verificação", doc: "docs/subsystems/operations.md" },
      { kind: "deep", text: "O assistente de primeiro acesso na interface", doc: "docs/subsystems/web-ui.md" },
    ],
  },
  {
    id: "profiles",
    label: "Os três perfis",
    summary: "Administrador, nó e inferência",
    title: "Os três perfis",
    blocks: [
      {
        kind: "lead",
        text: "Cada máquina tem um perfil gravado na própria configuração. Ele decide o que a interface mostra e o que a máquina pode fazer pela rede.",
      },
      { kind: "diagram", id: "profiles", caption: "O administrador enxerga a rede inteira; o nó cuida da própria máquina; a inferência conversa com os modelos." },
      {
        kind: "cards",
        items: [
          { title: "Administrador", text: "Opera o cluster: vê a rede inteira, implanta e desimplanta modelos, adiciona máquinas e decide políticas globais como TLS entre os nós." },
          { title: "Nó", text: "Cuida da própria máquina: serve a faixa de camadas que recebeu, mostra o modelo e a carga locais e pode sair da rede quando quiser." },
          { title: "Inferência", text: "Uma vista reduzida para conversar com os modelos: Chat, Chat da rede, documentação, a própria chave de API e o pedido para entrar no processamento da rede." },
        ],
      },
      {
        kind: "table",
        head: ["Ação", "Administrador", "Nó", "Inferência"],
        rows: [
          ["Conversar no Chat", "sim", "sim", "sim"],
          ["Implantar e desimplantar modelos", "sim", "não", "não"],
          ["Ativar e instalar plugins na própria máquina", "sim", "sim", "sim"],
          ["Criar a própria chave de API", "sim", "sim", "sim"],
          ["Sair da rede", "sim", "sim", "sim"],
          ["Bloquear outra máquina", "sim, com autoridade", "não", "não"],
        ],
      },
      {
        kind: "note",
        title: "A tela esconde, o servidor decide",
        text: "Esconder um botão é só apresentação. A autorização de verdade é feita pelo servidor, que recusa uma rota fora do perfil mesmo que alguém a chame à mão.",
      },
      {
        kind: "p",
        text: "Uma máquina de inferência pode pedir para entrar no processamento pela tela de Rede; o pedido aparece no sino para qualquer administrador aprovar. Um nó também pode descer para inferência: as requisições em andamento terminam antes de a fatia ser liberada.",
      },
      { kind: "deep", text: "O que cada perfil vê na interface", doc: "docs/subsystems/web-ui.md" },
      { kind: "deep", text: "Autenticação e escopo de cada rota", doc: "docs/subsystems/http-api.md" },
    ],
  },
  {
    id: "request",
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
          { title: "A pergunta chega", text: "Qualquer nó aceita. Ele escolhe o caminho que entrega a resposta mais cedo entre as máquinas que cobrem o modelo. Se falta faixa, a requisição falha com a explicação - não existe um modo degradado que responda pior sem avisar." },
          { title: "A cabeça lê o prompt inteiro", text: "A máquina que tem a camada 0 tokeniza e processa o prompt em pedaços. É a etapa mais cara da requisição, e é por isso que a camada 0 deve ficar numa máquina com memória de sobra." },
          { title: "O estado oculto atravessa a cadeia", text: "Cada máquina roda as suas camadas e entrega o resultado à seguinte. Num modelo de dimensão 4096 são 8.192 bytes por token por salto, porque o vetor viaja em f16 e não em texto." },
          { title: "A cauda escolhe o token", text: "A última máquina da cadeia é a única que amostra: converte o estado oculto no token seguinte." },
          { title: "A resposta volta direto", text: "A cauda entrega o token diretamente a quem recebeu a pergunta, em vez de refazer a cadeia de volta. Quando isso não é possível, a resposta volta pelo anel." },
        ],
      },
      { kind: "h", text: "O que isso custa" },
      { kind: "stats", items: [{value: "8 KB", label: "estado oculto por token, por salto (f16, dimensão 4096)"}, {value: "16 KB", label: "o mesmo estado em f32, sem perda"}, {value: "~200 B", label: "por token amostrado de volta"}, {value: "+1 RTT", label: "por máquina extra na cadeia"}] },
      {
        kind: "warn",
        title: "O prompt tem orçamento, e passar dele é recusa",
        text: "Cada sessão tem a janela de contexto da implantação. Aumentar as sessões multiplica a memória reservada, não divide essa janela. O gateway mede o prompt antes de gerar e responde HTTP 413 quando não sobra espaço, em vez de derrubar o processo.",
      },
      {
        kind: "note",
        title: "Decodificação especulativa",
        text: "Quando o modelo permite, um rascunho de vários tokens é proposto de uma vez e a cauda confirma ou descarta o lote. Acertou, vários tokens saem pelo preço de uma travessia; errou, o cache volta atrás e nada de errado sai.",
      },
      { kind: "faq", items: [{q: "Dá para trafegar menos bytes por token?", a: "Sim. f16 é o padrão; f32 é sem perda e dobra o volume; f8 e i4 são formatos com perda opcionais, negociados por salto na implantação."}, {q: "E se duas máquinas servem a mesma faixa?", a: "São réplicas do mesmo estágio, não dois saltos. O gateway escolhe uma réplica por sessão e mantém essa escolha nas rodadas seguintes."}, {q: "Quem decide o caminho?", a: "O gateway estima, para cada caminho que cobre o modelo, o tempo de prefill, de decode e de rede, considerando a fila de cada máquina, e escolhe o que entrega mais cedo."}] },
      { kind: "deep", text: "Prefill, decode, formato do estado oculto e especulativa", doc: "docs/subsystems/inference.md" },
      { kind: "deep", text: "Pipeline e tensor parallelism: quando usar cada um", doc: "docs/subsystems/parallelism.md" },
      { kind: "deep", text: "Rotas, autenticação e a prova de execução distribuída", doc: "docs/subsystems/http-api.md" },
    ],
  },
  {
    id: "slice-and-deploy",
    label: "Do modelo ao cluster servindo",
    summary: "Fatiar por tamanho, distribuir, pôr em serviço",
    title: "Do modelo ao cluster servindo",
    blocks: [
      {
        kind: "lead",
        text: "São três atos distintos, e vale separá-los na cabeça: cortar o arquivo, levar os pedaços às máquinas certas, e mandar as máquinas carregarem o que receberam.",
      },
      {
        kind: "note",
        title: "Antes de tudo, um modelo",
        text: "Os modelos inteiros vêm das pastas de modelos da máquina. Para baixar um, use Encontrar Modelos HF: a tela mostra a licença antes do download e o progresso fica em Transferências. Um modelo que cabe inteiro numa máquina pode ser implantado sem fatiar.",
      },
      { kind: "diagram", id: "slicing", caption: "Um GGUF de 32 camadas cortado por alvo de tamanho, não em partes iguais." },
      { kind: "h", text: "1. Fatiar (Fatiar modelos)" },
      {
        kind: "p",
        text: "Você monta uma lista de fatias e dá a cada uma um alvo em GB; a última pode simplesmente levar o resto. A tela converte GB em faixa de camadas somando o tamanho real de cada bloco do arquivo - não uma divisão do total pelo número de camadas, porque os blocos não são iguais entre si e algumas partes do modelo se repetem em toda fatia.",
      },
      {
        kind: "note",
        title: "O arredondamento fica à vista",
        text: "Camada é unidade indivisível, então o que você pediu e o que saiu quase nunca batem exatamente. A tela mostra os números lado a lado: o alvo pedido, a faixa que saiu e o tamanho real do arquivo.",
      },
      { kind: "h", text: "2. Distribuir (Transferências)" },
      {
        kind: "p",
        text: "Uma fatia chega à outra máquina de dois jeitos: ela puxa, ou o dono do arquivo oferece. Uma oferta aparece como notificação do outro lado e só começa depois de aceita - nunca se escreve no disco alheio sem convite.",
      },
      {
        kind: "cards",
        items: [
          { title: "Retomada", text: "O arquivo baixa como .part e continua de onde parou. Interromper não significa recomeçar." },
          { title: "Conferência", text: "O arquivo é conferido antes de tomar o nome final. Não bateu, o parcial é apagado." },
          { title: "Espaço em disco", text: "Verificado antes de começar, e a implantação conta a união das fatias mais o que ainda falta." },
        ],
      },
      { kind: "h", text: "3. Implantar (Implantar na rede)" },
      {
        kind: "p",
        text: "Aqui você escolhe qual máquina serve qual faixa e manda o cluster carregar. O nó sugere uma distribuição; a sua escolha viaja na requisição e o cluster é revarrido na hora de aplicar, em vez de confiar na prévia. Os cartões do mapa mostram quanto de memória cada máquina vai usar depois do deploy.",
      },
      {
        kind: "table",
        head: ["Regra", "Por quê"],
        rows: [
          ["A ordem do pipeline é a ordem das camadas", "quem fica com a camada 0 recebe o prompt inteiro de cada requisição; escolher a ordem e escolher as faixas são o mesmo ato"],
          ["Várias faixas na mesma máquina são válidas", "desde que não se sobreponham e a cobertura global seja completa; a fusão só junta faixas vizinhas"],
          ["Sobreposição é recusa", "a mesma camada rodando duas vezes corrompe a resposta"],
          ["Buraco é recusa", "todas as camadas exigidas precisam estar cobertas antes de formar o pipeline"],
        ],
      },
      {
        kind: "note",
        title: "Contexto e batch valem para o cluster; GPU vale por máquina",
        text: "A janela de contexto e o tamanho do lote governam a cadeia inteira. Já quantas camadas vão para a GPU e onde fica o cache de atenção são propriedade do hardware - uma máquina com 16 GB de VRAM e outra sem GPU não podem estar as duas certas sobre isso, então cada uma tem o seu campo.",
      },
      { kind: "checklist", title: "Antes de clicar em implantar", items: ["Todas as camadas estão cobertas, sem buraco.", "Nenhuma faixa se sobrepõe a outra.", "A máquina com a camada 0 tem memória de sobra para o prompt e o cache.", "Os cartões do mapa não mostram aviso de memória."] },
      {
        kind: "p",
        text: "Para conferir, abra Implantações: a versão ativa mostra os parâmetros usados, e Reimplantar esta versão volta a qualquer versão anterior.",
      },
      { kind: "faq", items: [{q: "Preciso fatiar sempre?", a: "Não. Um modelo que cabe inteiro numa máquina é implantado como modelo inteiro, numa máquina só. Mais máquinas podem entrar no mapa, desde que as faixas delas cubram todas as camadas."}, {q: "Pipeline ou tensor parallelism?", a: "Pipeline é a opção recomendada entre máquinas pela internet: uma travessia por fronteira de camadas por token. No tensor parallelism cada camada é repartida e as máquinas somam resultados várias vezes por token, então ele pede uma rede rápida."}] },
      { kind: "deep", text: "Fatiamento, tabela de tipos e planejamento por tamanho", doc: "docs/subsystems/slicing.md" },
      { kind: "deep", text: "Implantação, fusão e ciclo de vida da fatia", doc: "docs/subsystems/deployment.md" },
      { kind: "deep", text: "Pipeline e tensor parallelism", doc: "docs/subsystems/parallelism.md" },
      { kind: "deep", text: "Transferência entre máquinas", doc: "docs/subsystems/file-transfer.md" },
    ],
  },
  {
    id: "using-the-api",
    label: "Usar a API",
    summary: "Chaves de API, rotas compatíveis e harnesses",
    title: "Usar a API",
    blocks: [
      {
        kind: "lead",
        text: "Qualquer nó fala a API compatível com OpenAI e também a de mensagens da Anthropic. Apontar uma ferramenta para o cluster é escolher a URL de um nó e mandar uma chave de API.",
      },
      { kind: "diagram", id: "api", caption: "O cliente chama qualquer gateway com uma chave de API; a resposta volta com x_pom.source contando quais máquinas trabalharam." },
      {
        kind: "steps",
        items: [
          { title: "Crie uma chave de API", text: "Em Configurações > API keys. Cada perfil cria a sua; a chave de conexão do cluster não serve para inferência." },
          { title: "Descubra o modelo e o limite", text: "GET /v1/models lista os modelos servidos agora pela rede, com o contexto disponível e o máximo de tokens de saída." },
          { title: "Faça a chamada", text: "Mande a chave em Authorization: Bearer ou em x-api-key. O pedido entra no pipeline distribuído; não há outra rota escondida." },
        ],
      },
      {
        kind: "code",
        code: 'curl -s http://<gateway>:8080/v1/chat/completions \\\n  -H "Authorization: Bearer $POM_API_KEY" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"messages":[{"role":"user","content":"Diga oi em uma frase."}],"max_tokens":32}\'',
      },
      {
        kind: "note",
        title: "A prova de que a resposta foi distribuída",
        text: "A resposta traz x_pom.source. Um valor começando por pipeline: e com dois ou mais saltos é a prova de que mais de uma máquina trabalhou. Um salto só, ou none, significa que a cobertura não fechou.",
      },
      {
        kind: "cards",
        items: [
          { title: "max_tokens", text: "É o teto da sua requisição e é respeitado à risca. Atingi-lo encerra a resposta com finish_reason length." },
          { title: "Prompt grande demais", text: "Se o prompt não deixa espaço na janela de contexto, a resposta é HTTP 413 antes de qualquer geração." },
          { title: "Streaming", text: "Com stream: true a resposta chega por SSE, e os argumentos de uma chamada de ferramenta saem em fragmentos conforme o modelo gera." },
        ],
      },
      {
        kind: "note",
        title: "Claude Code e outros clientes",
        text: "Para Claude Code, aponte ANTHROPIC_BASE_URL para a origem do gateway (por exemplo http://<gateway>:8080) e use uma chave de API ativa em ANTHROPIC_API_KEY. Clientes compatíveis com OpenAI usam a base /v1.",
      },
      { kind: "faq", items: [{q: "Qual a diferença entre max_tokens e o max_new_tokens da implantação?", a: "O max_new_tokens da implantação só limita uma perna interna de geração: o gateway continua na mesma sessão sem o cliente perceber. Quem limita a sua requisição é o max_tokens."}, {q: "Posso usar a chave de conexão no meu cliente?", a: "Não. As rotas de inferência aceitam somente uma chave de API ativa."}] },
      { kind: "deep", text: "Todas as rotas, autenticação e limites de saída", doc: "docs/subsystems/http-api.md" },
      { kind: "deep", text: "Orçamentos de saída e o laço de continuação", doc: "docs/subsystems/inference.md" },
    ],
  },
  {
    id: "network-and-internet",
    label: "Rede local e internet",
    summary: "Como as máquinas se acham e quais portas abrir",
    title: "Rede local e internet",
    blocks: [
      {
        kind: "lead",
        text: "Na mesma rede local as máquinas se acham sozinhas. Pela internet elas precisam de um ponto de partida e de um endereço que o outro lado consiga discar.",
      },
      {
        kind: "cards",
        items: [
          { title: "Rede local", text: "Descoberta por UDP com broadcast e troca periódica de anúncios. Funciona sem infraestrutura nenhuma e anuncia mesmo sem modelo carregado." },
          { title: "Internet", text: "Um swarm libp2p sobre TCP, com relay para atravessar NAT e hole punching. Os endereços também são publicados numa DHT." },
        ],
      },
      { kind: "diagram", id: "network", caption: "Na rede local a descoberta é por broadcast UDP. Pela internet, o libp2p usa relé e hole punching para alcançar uma máquina atrás de NAT." },
      { kind: "h", text: "As portas" },
      {
        kind: "table",
        head: ["Uso", "Padrão", "Protocolo"],
        rows: [
          ["Descoberta P2P (--port)", "42777", "UDP"],
          ["libp2p", "P2P + 10000 (52777)", "TCP"],
          ["Interface e API (--gateway-port)", "8080", "TCP"],
          ["Servidor de inferência", "8181", "TCP"],
          ["QUIC da inferência", "P2P + 1000 (43777)", "UDP"],
          ["QUIC do gateway", "P2P + 2000 (44777)", "UDP"],
        ],
      },
      {
        kind: "steps",
        items: [
          { title: "Dê um ponto de partida", text: "Inicie o nó com --bootstrap <máquina>:42777 apontando para uma máquina que já está na rede." },
          { title: "Fixe o endereço anunciado, se precisar", text: "Em Minha máquina, Endereço anunciado. Com um endereço fixado, ele é o único anunciado e discado pelos outros nós. Limpar o campo volta ao endereço detectado." },
          { title: "Confira a travessia", text: "A tela de Rede mostra o estado do NAT, os endereços diretos, os relés e os hole punches - os três degraus onde dois nós atrás de NAT costumam falhar." },
        ],
      },
      {
        kind: "note",
        title: "Rede privada ou pública",
        text: "Uma rede privada exige a chave de conexão e o trust.json para entrar. Uma rede pública aparece no diretório de Redes públicas com nome, país e cidade, e exige o trust.json.",
      },
      {
        kind: "note",
        title: "Criptografia entre os nós",
        text: "O administrador principal liga ou desliga TLS entre os nós em Preferências administrativas, sem reiniciar. Os outros nós adotam a política em alguns segundos. O certificado de cada nó é derivado da sua identidade.",
      },
      { kind: "deep", text: "Descoberta, endereços, NAT e TLS", doc: "docs/subsystems/p2p-network.md" },
      { kind: "deep", text: "Portas e pegadinhas de campo", doc: "docs/subsystems/operations.md" },
    ],
  },
  {
    id: "plugins",
    label: "Plugins",
    summary: "Licenças e repositórios, máquina por máquina",
    title: "Plugins",
    blocks: [
      {
        kind: "lead",
        text: "Plugins acrescentam telas e funções ao painel. Eles são locais: cada máquina ativa e instala os seus, e uma licença de um plugin não liga nem desliga outro.",
      },
      { kind: "diagram", id: "plugins", caption: "Cada plugin roda num processo próprio ao lado do nó. A licença ou o repositório GitHub vale só para esta máquina." },
      {
        kind: "steps",
        items: [
          { title: "Abra Plugins", text: "Em Configurações > Plugins aparece o catálogo, com o que está ativo nesta máquina." },
          { title: "Ative por licença", text: "Cole a chave de licença no formulário. A resposta diz quais plugins aquela chave concedeu e instalou." },
          { title: "Ou instale do GitHub", text: "Adicione a URL de um repositório público em Adicionar do GitHub e escolha uma release compatível com esta máquina. Instalar e atualizar pedem confirmação." },
        ],
      },
      {
        kind: "cards",
        items: [
          { title: "Qualquer perfil", text: "Administrador, nó e inferência podem ativar, instalar, atualizar e usar plugins na própria máquina." },
          { title: "Telas descobríveis", text: "Logs de requisições e Estatísticas aparecem no menu mesmo sem licença; os dados só chegam quando o plugin correspondente está ativo." },
          { title: "Processo separado", text: "Cada plugin é carregado por um processo próprio, o pom-plugin-host, e não dentro do binário principal. Plugins são independentes entre si." },
        ],
      },
      {
        kind: "note",
        title: "Atualizar o pom não é um plugin",
        text: "A verificação de nova versão do próprio binário fica em Configurações > Sobre, separada da tela de Plugins.",
      },
      { kind: "deep", text: "Licenças, repositórios GitHub e a fronteira de processo", doc: "docs/subsystems/enterprise-licensing.md" },
      { kind: "deep", text: "Menus, telas e rotas fornecidos por plugins", doc: "docs/subsystems/plugins-ui.md" },
    ],
  },
  {
    id: "operating",
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
        text: "Toda resposta carrega a cadeia que a produziu em x_pom.source. Uma origem começando por pipeline: e listando dois ou mais saltos é a única prova de que a inferência foi conjunta. Um salto só, ou none, significa que o planejador não fechou a cobertura das camadas.",
      },
      { kind: "code", code: 'curl -s localhost:8080/peers | python3 -m json.tool\ncurl -sf localhost:8181/health' },
      { kind: "checklist", title: "Conferência rápida", items: ["/peers lista todas as máquinas esperadas.", "/health do servidor de inferência responde em cada máquina com fatia.", "Uma chamada de teste volta com x_pom.source começando por pipeline: e com dois ou mais saltos."] },
      { kind: "h", text: "Sintomas e onde olhar" },
      {
        kind: "table",
        head: ["O que você vê", "Causa provável", "Onde confirmar"],
        rows: [
          ["As máquinas não se enxergam", "descoberta não saiu do nó, ou endereço anunciado errado", "a tela de Rede e a lista de nós do painel"],
          ["Origem “none” na resposta", "a cobertura de camadas tem buraco", "a tela Implantar na rede mostra a faixa que falta"],
          ["Um nó aparece sem fatia", "o modelo não carregou, ou a porta de inferência está ocupada", "o card da máquina no painel diz o motivo"],
          ["A resposta demora muito para começar", "a máquina com a camada 0 está processando o prompt", "mova a camada 0 para uma máquina com mais memória"],
          ["Uma fatia não volta depois de reiniciar", "ela nunca chegou a carregar antes de cair", "o card mostra por que a retomada foi recusada"],
          ["O nó aparece como idle", "o modelo foi descarregado por ociosidade", "nada a fazer: a próxima requisição recarrega"],
        ],
      },
      {
        kind: "warn",
        title: "Duas máquinas, dois lares",
        text: "Rodar dois nós no mesmo diretório faz os dois carregarem a mesma identidade e o cluster os trata como uma máquina só. Portas diferentes não bastam: o segundo nó precisa de um diretório de trabalho próprio (POM_HOME).",
      },
      {
        kind: "note",
        title: "Logs da implantação",
        text: "Implantações > Logs reúne os logs de processo de todas as máquinas na ordem em que chegaram, e pode ser filtrada por implantação.",
      },
      { kind: "deep", text: "Subida, verificação e pegadinhas de campo", doc: "docs/subsystems/operations.md" },
      { kind: "deep", text: "Build, features de GPU e variáveis de ambiente", doc: "docs/subsystems/build-and-run.md" },
    ],
  },
  {
    id: "access",
    label: "Acesso e segurança",
    summary: "Senha, chaves e o que cada uma abre",
    title: "Acesso e segurança",
    blocks: [
      {
        kind: "lead",
        text: "Há três credenciais, e cada uma abre uma porta diferente. Saber qual é qual evita entregar a chave errada para a pessoa errada.",
      },
      {
        kind: "cards",
        items: [
          { title: "Senha do painel", text: "Credencial humana para abrir a interface. Fica só na memória da aba; o nó guarda apenas um hash dela. A sessão vale para as rotas administrativas." },
          { title: "Chave de conexão", text: "A mesma no cluster inteiro. Identifica o operador e o canal entre os nós: autentica as rotas administrativas e de implantação. Não serve para inferência." },
          { title: "Chave de API", text: "A única credencial aceita em /v1 e /api/generate. Cada uma tem uma identidade não secreta, então os registros mostram quem pediu sem expor o segredo." },
        ],
      },
      {
        kind: "note",
        title: "A porta de inferência não aceita chaves de cliente",
        text: "O servidor de inferência entre as máquinas só aceita a credencial interna do cluster. Chaves de API de clientes são recusadas ali, e só o /health fica público para conferir prontidão.",
      },
      {
        kind: "warn",
        title: "Pense na rede antes de expor uma porta",
        text: "Sem TLS entre os nós, o estado oculto trafega em claro. Ligue o TLS em Preferências administrativas quando as máquinas conversam por redes que você não controla.",
      },
      {
        kind: "note",
        title: "Bloquear uma máquina",
        text: "Um nó pode ser bloqueado pela identidade, e a identidade dele é derivada de uma chave que vive no diretório de trabalho da máquina. Apagar essa chave cria uma máquina nova aos olhos das outras.",
      },
      { kind: "faq", items: [{q: "Onde vejo a chave de conexão?", a: "Em Configurações > Chaves de conexão. Trocá-la corta todos os outros nós, por isso o botão só aparece para quem tem autoridade."}, {q: "Uma chave de API vazada compromete o cluster?", a: "Ela só abre as rotas de inferência. Revogue a chave em API keys e gere outra; a chave de conexão continua valendo."}] },
      { kind: "deep", text: "Autenticação, perfis e todas as rotas", doc: "docs/subsystems/http-api.md" },
      { kind: "deep", text: "TLS entre peers e limitações para a internet", doc: "docs/subsystems/p2p-network.md" },
    ],
  },
];

const en: Guide[] = [
  {
    id: "overview",
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
        text: "Every node runs the same binary, every node serves this interface and the API, and any of them can take the incoming request. What differs between them is the range of layers they hold in memory and the role each one has in the network.",
      },
      { kind: "diagram", id: "topology", caption: "Three machines covering a 32-layer model. Whoever takes the question need not be the one holding layer 0." },
      { kind: "h", text: "The four pieces" },
      {
        kind: "cards",
        items: [
          { title: "Node", text: "One machine in the cluster. It announces who it is, how much memory it has and which layer range it serves. It finds the others over UDP on the local network and over libp2p across the internet." },
          { title: "Slice", text: "A GGUF file holding a range of layers of the original model. Slicing is local: each machine extracts its own range from its own copy, and no weights travel the network because of it." },
          { title: "Pipeline", text: "The ordered chain of nodes covering layer 0 through the last one. Without full coverage there is no answer - the request fails with the reason instead of answering worse." },
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
      {
        kind: "p",
        text: "If this is your first time here, read the guides in order: getting started, the three profiles, the path of a question and, last, from model to serving cluster.",
      },
      { kind: "deep", text: "Discovery, advertised addresses and node identity", doc: "docs/subsystems/p2p-network.md" },
      { kind: "deep", text: "Repository map and project invariants", doc: "AGENTS.md" },
    ],
  },
  {
    id: "getting-started",
    label: "Getting started",
    summary: "From the first machine to a network of two",
    title: "Getting started",
    blocks: [
      {
        kind: "lead",
        text: "A network starts with one administrator machine. The others join by invitation. No model is passed on the command line: everything after the first start happens through this interface.",
      },
      { kind: "diagram", id: "setup", caption: "The administrator creates the network and the invitation. The new machine finds the administrator through local discovery, or gets the pasted invitation, and goes from joining to connected." },
      {
        kind: "steps",
        items: [
          { title: "Start the binary", text: "Run the pom executable with no arguments. Without a configuration file, the node comes up in setup mode and waits for the browser." },
          { title: "Open the interface", text: "Go to http://<machine>:8080/. The interface and the API always live on the gateway port, which is 8080 by default." },
          { title: "Pick the role", text: "The first machine picks Administrator. The wizard only asks for the machine name, the network name and whether it is private or public." },
          { title: "Keep the invitation", text: "On the last step the administrator gets the invitation: the connection key (on a private network) and the trust.json file. The other machines join with them." },
          { title: "Bring in the second machine", text: "On the other machine, run the same binary, open the interface and pick Node. On the same local network the wizard finds the administrator on its own; otherwise, paste the trust.json and the key." },
        ],
      },
      {
        kind: "note",
        title: "While the wizard runs, the administrator sees who arrives",
        text: "On the invitation step, each computer looking for the network shows up with its name, system, IP, CPU, RAM and GPU, and goes from joining to connected when it receives the root. Finishing with nobody is normal too.",
      },
      {
        kind: "warn",
        title: "Change the default password",
        text: "After setup, the panel password is admin and the interface shows a fixed banner until you change it in Preferences. Folders, memory reserves and the thermal limit come from the machine's defaults and can also be adjusted later.",
      },
      {
        kind: "p",
        text: "With both machines in the network, the next step is a model: bring a GGUF into the models folder or use Find HF models, then follow the guide From model to serving cluster.",
      },
      {
        kind: "note",
        title: "Machines on different networks",
        text: "Outside the same local network, automatic discovery does not reach the administrator. Start the node with --bootstrap <administrator>:42777 and see the guide Local network and internet.",
      },
      { kind: "checklist", title: "Ready for the next step when", items: ["Both machines show up on the Dashboard.", "The default panel password was changed in Preferences.", "There is a GGUF model in the models folder, or a finished download in Transfers."] },
      { kind: "faq", items: [{q: "Do I need to open router ports?", a: "Not on the same local network: UDP discovery is enough. Across different networks, see the guide Local network and internet, which lists the ports and the advertised address."}, {q: "Can I run two nodes on one machine?", a: "Yes, as long as each has its own working directory (POM_HOME) and its own ports, for example --port 42778 --gateway-port 8091 on the second one."}, {q: "I lost the invitation. Now what?", a: "On the administrator, Settings > Sharing shows the trust root and the connection key again."}] },
      { kind: "deep", text: "Bringing up a two-machine cluster and checking it", doc: "docs/subsystems/operations.md" },
      { kind: "deep", text: "The first access wizard in the interface", doc: "docs/subsystems/web-ui.md" },
    ],
  },
  {
    id: "profiles",
    label: "The three profiles",
    summary: "Administrator, node and inference",
    title: "The three profiles",
    blocks: [
      {
        kind: "lead",
        text: "Each machine has a profile saved in its own configuration. It decides what the interface shows and what the machine can do in the network.",
      },
      { kind: "diagram", id: "profiles", caption: "The administrator sees the whole network; the node takes care of its own machine; inference talks to the models." },
      {
        kind: "cards",
        items: [
          { title: "Administrator", text: "Operates the cluster: sees the whole network, deploys and undeploys models, adds machines and decides global policies such as TLS between nodes." },
          { title: "Node", text: "Takes care of its own machine: serves the layer range it received, shows the local model and load, and can leave the network whenever it wants." },
          { title: "Inference", text: "A reduced view for talking to the models: Chat, Network Chat, documentation, its own API key and the request to join the network's processing." },
        ],
      },
      {
        kind: "table",
        head: ["Action", "Administrator", "Node", "Inference"],
        rows: [
          ["Talk in the Chat", "yes", "yes", "yes"],
          ["Deploy and undeploy models", "yes", "no", "no"],
          ["Activate and install plugins on its own machine", "yes", "yes", "yes"],
          ["Create its own API key", "yes", "yes", "yes"],
          ["Leave the network", "yes", "yes", "yes"],
          ["Block another machine", "yes, with authority", "no", "no"],
        ],
      },
      {
        kind: "note",
        title: "The screen hides, the server decides",
        text: "Hiding a button is only presentation. The real authorization happens on the server, which refuses a route outside the profile even when someone calls it by hand.",
      },
      {
        kind: "p",
        text: "An inference machine can ask to join the processing from the Network screen; the request shows up in the bell for any administrator to approve. A node can also step down to inference: requests in flight finish before the slice is released.",
      },
      { kind: "deep", text: "What each profile sees in the interface", doc: "docs/subsystems/web-ui.md" },
      { kind: "deep", text: "Authentication and the scope of each route", doc: "docs/subsystems/http-api.md" },
    ],
  },
  {
    id: "request",
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
          { title: "The question arrives", text: "Any node accepts it. It picks the path that delivers the answer earliest among the machines covering the model. If a range is missing the request fails with the reason - there is no degraded mode that quietly answers worse." },
          { title: "The head reads the whole prompt", text: "The machine holding layer 0 tokenizes and processes the prompt in chunks. It is the most expensive step of the request, which is why layer 0 belongs on a machine with memory to spare." },
          { title: "The hidden state crosses the chain", text: "Each machine runs its layers and hands the result to the next. On a 4096-dimension model that is 8,192 bytes per token per hop, because the vector travels as f16 rather than text." },
          { title: "The tail picks the token", text: "The last machine in the chain is the only one that samples: it turns the hidden state into the next token." },
          { title: "The answer goes straight back", text: "The tail delivers the token directly to whoever took the question instead of retracing the chain. When that is not possible, the answer goes back through the ring." },
        ],
      },
      { kind: "h", text: "What it costs" },
      { kind: "stats", items: [{value: "8 KB", label: "hidden state per token, per hop (f16, 4096 dimensions)"}, {value: "16 KB", label: "the same state in f32, lossless"}, {value: "~200 B", label: "per sampled token coming back"}, {value: "+1 RTT", label: "per extra machine in the chain"}] },
      {
        kind: "warn",
        title: "The prompt has a budget, and exceeding it is a refusal",
        text: "Each session gets the deployment's context window. Increasing sessions multiplies reserved memory; it does not divide that window. The gateway measures the prompt before generating and answers HTTP 413 when there is no room left, instead of taking the process down.",
      },
      {
        kind: "note",
        title: "Speculative decoding",
        text: "When the model allows it, a draft of several tokens is proposed at once and the tail confirms or discards the batch. On a hit, several tokens come out for the price of one crossing; on a miss, the cache rolls back and nothing wrong escapes.",
      },
      { kind: "faq", items: [{q: "Can fewer bytes travel per token?", a: "Yes. f16 is the default; f32 is lossless and doubles the volume; f8 and i4 are optional lossy formats, negotiated per hop in the deployment."}, {q: "What if two machines serve the same range?", a: "They are replicas of the same stage, not two hops. The gateway picks one replica per session and keeps that choice in the following rounds."}, {q: "Who decides the path?", a: "For each path that covers the model, the gateway estimates prefill, decode and network time, counting each machine's queue, and picks the one that delivers earliest."}] },
      { kind: "deep", text: "Prefill, decode, hidden-state format and speculation", doc: "docs/subsystems/inference.md" },
      { kind: "deep", text: "Pipeline and tensor parallelism: when to use each", doc: "docs/subsystems/parallelism.md" },
      { kind: "deep", text: "Routes, authentication and the proof of distributed execution", doc: "docs/subsystems/http-api.md" },
    ],
  },
  {
    id: "slice-and-deploy",
    label: "From model to serving cluster",
    summary: "Slice by size, distribute, put into service",
    title: "From model to serving cluster",
    blocks: [
      {
        kind: "lead",
        text: "These are three separate acts, and it helps to keep them apart: cutting the file, getting the pieces to the right machines, and telling those machines to load what they received.",
      },
      {
        kind: "note",
        title: "First of all, a model",
        text: "Full models come from the machine's model folders. To download one, use Find HF models: the screen shows the license before the download and the progress shows up in Transfers. A model that fits whole on one machine can be deployed without slicing.",
      },
      { kind: "diagram", id: "slicing", caption: "A 32-layer GGUF cut by size target, not into equal parts." },
      { kind: "h", text: "1. Slice (Slice models)" },
      {
        kind: "p",
        text: "You build a list of slices and give each a target in GB; the last one can simply take the remainder. The screen converts GB into a layer range by summing the real size of each block in the file - not the total divided by the layer count, because blocks are not equal to one another and some parts of the model repeat in every slice.",
      },
      {
        kind: "note",
        title: "The rounding stays visible",
        text: "A layer is indivisible, so what you asked for and what came out almost never match exactly. The screen shows the numbers side by side: the target, the resulting range and the real file size.",
      },
      { kind: "h", text: "2. Distribute (Transfers)" },
      {
        kind: "p",
        text: "A slice reaches another machine in one of two ways: it pulls, or the file's owner offers. An offer shows up as a notification on the other side and only starts once accepted - nothing is ever written to someone else's disk uninvited.",
      },
      {
        kind: "cards",
        items: [
          { title: "Resume", text: "The file downloads as .part and picks up where it stopped. Interrupting it does not mean starting over." },
          { title: "Verification", text: "The file is checked before it takes its final name. If it does not match, the partial file is deleted." },
          { title: "Disk space", text: "Checked before starting, and deployment counts the union of the slices plus what is still missing." },
        ],
      },
      { kind: "h", text: "3. Deploy (Deploy to network)" },
      {
        kind: "p",
        text: "Here you choose which machine serves which range and tell the cluster to load. The node suggests a distribution; your choice travels with the request and the cluster is re-scanned at apply time rather than trusting the preview. The map cards show how much memory each machine will use after the deploy.",
      },
      {
        kind: "table",
        head: ["Rule", "Why"],
        rows: [
          ["Pipeline order is layer order", "whoever holds layer 0 receives the full prompt of every request; choosing the order and choosing the ranges are the same act"],
          ["Several ranges on one machine are valid", "as long as they do not overlap and global coverage is complete; merge only joins neighboring ranges"],
          ["Overlap is a refusal", "the same layer running twice corrupts the answer"],
          ["A gap is a refusal", "every required layer must be covered before a pipeline forms"],
        ],
      },
      {
        kind: "note",
        title: "Context and batch are cluster-wide; GPU is per machine",
        text: "The context window and batch size govern the whole chain. How many layers go to the GPU and where the attention cache lives are properties of the hardware - a machine with 16 GB of VRAM and one without a GPU cannot both be right about that, so each has its own field.",
      },
      { kind: "checklist", title: "Before clicking deploy", items: ["Every layer is covered, with no gap.", "No range overlaps another.", "The machine holding layer 0 has memory to spare for the prompt and the cache.", "The map cards show no memory warning."] },
      {
        kind: "p",
        text: "To check, open Deployments: the active version shows the parameters used, and Redeploy this version goes back to any earlier version.",
      },
      { kind: "faq", items: [{q: "Do I always have to slice?", a: "No. A model that fits whole on one machine is deployed as a whole model, on that one machine. More machines can join the map, as long as their ranges cover every layer."}, {q: "Pipeline or tensor parallelism?", a: "Pipeline is the recommended option between machines across the internet: one crossing per layer boundary per token. In tensor parallelism each layer is split and the machines sum results several times per token, so it needs a fast network."}] },
      { kind: "deep", text: "Slicing, type table and size-based planning", doc: "docs/subsystems/slicing.md" },
      { kind: "deep", text: "Deployment, merge and slice lifecycle", doc: "docs/subsystems/deployment.md" },
      { kind: "deep", text: "Pipeline and tensor parallelism", doc: "docs/subsystems/parallelism.md" },
      { kind: "deep", text: "Transfers between machines", doc: "docs/subsystems/file-transfer.md" },
    ],
  },
  {
    id: "using-the-api",
    label: "Using the API",
    summary: "API keys, compatible routes and harnesses",
    title: "Using the API",
    blocks: [
      {
        kind: "lead",
        text: "Every node speaks the OpenAI-compatible API and also Anthropic's messages API. Pointing a tool at the cluster is choosing a node's URL and sending an API key.",
      },
      { kind: "diagram", id: "api", caption: "The client calls any gateway with an API key; the answer comes back with x_pom.source telling which machines did the work." },
      {
        kind: "steps",
        items: [
          { title: "Create an API key", text: "In Settings > API keys. Each profile creates its own; the cluster connection key does not work for inference." },
          { title: "Find the model and the limit", text: "GET /v1/models lists the models the network serves right now, with the available context and the maximum output tokens." },
          { title: "Make the call", text: "Send the key in Authorization: Bearer or in x-api-key. The request enters the distributed pipeline; there is no other hidden route." },
        ],
      },
      {
        kind: "code",
        code: 'curl -s http://<gateway>:8080/v1/chat/completions \\\n  -H "Authorization: Bearer $POM_API_KEY" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"messages":[{"role":"user","content":"Say hi in one sentence."}],"max_tokens":32}\'',
      },
      {
        kind: "note",
        title: "The proof that the answer was distributed",
        text: "The answer carries x_pom.source. A value starting with pipeline: and with two or more hops proves that more than one machine did the work. A single hop, or none, means coverage did not close.",
      },
      {
        kind: "cards",
        items: [
          { title: "max_tokens", text: "It is your request's ceiling and it is enforced strictly. Reaching it ends the answer with finish_reason length." },
          { title: "Prompt too large", text: "If the prompt leaves no room in the context window, the answer is HTTP 413 before any generation." },
          { title: "Streaming", text: "With stream: true the answer arrives over SSE, and a tool call's arguments come out in fragments as the model generates them." },
        ],
      },
      {
        kind: "note",
        title: "Claude Code and other clients",
        text: "For Claude Code, point ANTHROPIC_BASE_URL at the gateway's origin (for example http://<gateway>:8080) and use an active API key in ANTHROPIC_API_KEY. OpenAI-compatible clients use the /v1 base.",
      },
      { kind: "faq", items: [{q: "What is the difference between max_tokens and the deployment's max_new_tokens?", a: "The deployment's max_new_tokens only limits an internal generation leg: the gateway continues in the same session without the client noticing. What limits your request is max_tokens."}, {q: "Can I use the connection key in my client?", a: "No. The inference routes only accept an active API key."}] },
      { kind: "deep", text: "Every route, authentication and output limits", doc: "docs/subsystems/http-api.md" },
      { kind: "deep", text: "Output budgets and the continuation loop", doc: "docs/subsystems/inference.md" },
    ],
  },
  {
    id: "network-and-internet",
    label: "Local network and internet",
    summary: "How machines find each other and which ports to open",
    title: "Local network and internet",
    blocks: [
      {
        kind: "lead",
        text: "On the same local network the machines find each other on their own. Across the internet they need a starting point and an address the other side can dial.",
      },
      {
        kind: "cards",
        items: [
          { title: "Local network", text: "Discovery over UDP with broadcast and periodic exchange of announcements. It works with no infrastructure at all and announces even without a loaded model." },
          { title: "Internet", text: "A libp2p swarm over TCP, with relay to cross NAT and hole punching. Addresses are also published in a DHT." },
        ],
      },
      { kind: "diagram", id: "network", caption: "On the local network discovery is by UDP broadcast. Across the internet, libp2p uses a relay and hole punching to reach a machine behind NAT." },
      { kind: "h", text: "The ports" },
      {
        kind: "table",
        head: ["Use", "Default", "Protocol"],
        rows: [
          ["P2P discovery (--port)", "42777", "UDP"],
          ["libp2p", "P2P + 10000 (52777)", "TCP"],
          ["Interface and API (--gateway-port)", "8080", "TCP"],
          ["Inference server", "8181", "TCP"],
          ["Inference QUIC", "P2P + 1000 (43777)", "UDP"],
          ["Gateway QUIC", "P2P + 2000 (44777)", "UDP"],
        ],
      },
      {
        kind: "steps",
        items: [
          { title: "Give it a starting point", text: "Start the node with --bootstrap <machine>:42777 pointing at a machine already in the network." },
          { title: "Pin the advertised address, if needed", text: "In My machine, Advertised address. With a pinned address, it is the only one announced and dialed by the other nodes. Clearing the field goes back to the detected address." },
          { title: "Check the traversal", text: "The Network screen shows the NAT state, direct addresses, relays and hole punches - the three steps where two nodes behind NAT usually fail." },
        ],
      },
      {
        kind: "note",
        title: "Private or public network",
        text: "A private network requires the connection key and the trust.json to join. A public network shows up in the Public networks directory with its name, country and city, and requires the trust.json.",
      },
      {
        kind: "note",
        title: "Encryption between nodes",
        text: "The principal administrator turns TLS between nodes on or off in Administrative preferences, without a restart. The other nodes adopt the policy within a few seconds. Each node's certificate is derived from its identity.",
      },
      { kind: "deep", text: "Discovery, addresses, NAT and TLS", doc: "docs/subsystems/p2p-network.md" },
      { kind: "deep", text: "Ports and field gotchas", doc: "docs/subsystems/operations.md" },
    ],
  },
  {
    id: "plugins",
    label: "Plugins",
    summary: "Licenses and repositories, machine by machine",
    title: "Plugins",
    blocks: [
      {
        kind: "lead",
        text: "Plugins add screens and features to the panel. They are local: each machine activates and installs its own, and one plugin's license never turns another one on or off.",
      },
      { kind: "diagram", id: "plugins", caption: "Each plugin runs in its own process next to the node. The license or GitHub repository only applies to this machine." },
      {
        kind: "steps",
        items: [
          { title: "Open Plugins", text: "Settings > Plugins shows the catalog, with what is active on this machine." },
          { title: "Activate by license", text: "Paste the license key in the form. The answer says which plugins that key granted and installed." },
          { title: "Or install from GitHub", text: "Add a public repository URL in Add from GitHub and pick a release compatible with this machine. Installing and updating ask for confirmation." },
        ],
      },
      {
        kind: "cards",
        items: [
          { title: "Any profile", text: "Administrator, node and inference can activate, install, update and use plugins on their own machine." },
          { title: "Discoverable screens", text: "Request logs and Statistics show up in the menu even without a license; the data only arrives once the matching plugin is active." },
          { title: "Separate process", text: "Each plugin is loaded by its own process, pom-plugin-host, and not inside the main binary. Plugins are independent from one another." },
        ],
      },
      {
        kind: "note",
        title: "Updating pom is not a plugin",
        text: "Checking for a new version of the binary itself lives in Settings > About, separate from the Plugins screen.",
      },
      { kind: "deep", text: "Licenses, GitHub repositories and the process boundary", doc: "docs/subsystems/enterprise-licensing.md" },
      { kind: "deep", text: "Menus, screens and routes provided by plugins", doc: "docs/subsystems/plugins-ui.md" },
    ],
  },
  {
    id: "operating",
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
        text: "Every answer carries the chain that produced it in x_pom.source. A source starting with pipeline: and listing two or more hops is the only proof that inference was joint. A single hop, or none, means the planner did not close layer coverage.",
      },
      { kind: "code", code: 'curl -s localhost:8080/peers | python3 -m json.tool\ncurl -sf localhost:8181/health' },
      { kind: "checklist", title: "Quick check", items: ["/peers lists every expected machine.", "The inference server's /health answers on each machine with a slice.", "A test call comes back with x_pom.source starting with pipeline: and two or more hops."] },
      { kind: "h", text: "Symptoms and where to look" },
      {
        kind: "table",
        head: ["What you see", "Likely cause", "Where to confirm"],
        rows: [
          ["The machines do not see each other", "discovery never left the node, or the advertised address is wrong", "the Network screen and the dashboard node list"],
          ["Source “none” in the answer", "layer coverage has a gap", "the Deploy to network screen shows the missing range"],
          ["A node shows up with no slice", "the model did not load, or the inference port is taken", "the machine's card on the dashboard states the reason"],
          ["The answer takes long to start", "the machine holding layer 0 is chewing the prompt", "move layer 0 to a machine with more memory"],
          ["A slice does not come back after a restart", "it never finished loading before it died", "the card shows why the resume was refused"],
          ["The node shows as idle", "the model was unloaded for inactivity", "nothing to do: the next request reloads it"],
        ],
      },
      {
        kind: "warn",
        title: "Two machines, two homes",
        text: "Running two nodes from the same directory makes both load the same identity, and the cluster treats them as one machine. Different ports are not enough: the second node needs its own working directory (POM_HOME).",
      },
      {
        kind: "note",
        title: "Deployment logs",
        text: "Deployments > Logs gathers the process logs of every machine in arrival order, and can be filtered by deployment.",
      },
      { kind: "deep", text: "Bring-up, verification and field gotchas", doc: "docs/subsystems/operations.md" },
      { kind: "deep", text: "Build, GPU features and environment variables", doc: "docs/subsystems/build-and-run.md" },
    ],
  },
  {
    id: "access",
    label: "Access and security",
    summary: "Password, keys and what each one opens",
    title: "Access and security",
    blocks: [
      {
        kind: "lead",
        text: "There are three credentials, and each one opens a different door. Knowing which is which keeps you from handing the wrong key to the wrong person.",
      },
      {
        kind: "cards",
        items: [
          { title: "Panel password", text: "A human credential to open the interface. It only lives in the tab's memory; the node keeps just a hash of it. The session covers the administrative routes." },
          { title: "Connection key", text: "The same across the whole cluster. It identifies the operator and the channel between nodes: it authenticates the administrative and deployment routes. It does not work for inference." },
          { title: "API key", text: "The only credential accepted on /v1 and /api/generate. Each one has a non-secret identity, so the records show who asked without exposing the secret." },
        ],
      },
      {
        kind: "note",
        title: "The inference port does not accept client keys",
        text: "The inference server between machines only accepts the cluster's internal credential. Client API keys are refused there, and only /health stays public to check readiness.",
      },
      {
        kind: "warn",
        title: "Think about the network before exposing a port",
        text: "Without TLS between nodes, the hidden state travels in clear text. Turn TLS on in Administrative preferences when the machines talk over networks you do not control.",
      },
      {
        kind: "note",
        title: "Blocking a machine",
        text: "A node can be blocked by identity, and that identity derives from a key living in the machine's working directory. Deleting that key creates a new machine in the eyes of the others.",
      },
      { kind: "faq", items: [{q: "Where do I see the connection key?", a: "In Settings > Connection keys. Rotating it cuts off every other node, so the button only shows up for whoever has authority."}, {q: "Does a leaked API key compromise the cluster?", a: "It only opens the inference routes. Revoke it in API keys and create another; the connection key stays valid."}] },
      { kind: "deep", text: "Authentication, profiles and every route", doc: "docs/subsystems/http-api.md" },
      { kind: "deep", text: "TLS between peers and limitations for the internet", doc: "docs/subsystems/p2p-network.md" },
    ],
  },
];

// The inspector category guides close the list; each one also opens in a
// dialog from its category in the deployment inspector.
const GUIDES: Record<Locale, Guide[]> = {
  en: [...en, ...deploySettingsEn],
  "pt-BR": [...ptBR, ...deploySettingsPtBR],
};

export function guidesFor(locale: Locale): Guide[] {
  return GUIDES[locale] ?? en;
}
