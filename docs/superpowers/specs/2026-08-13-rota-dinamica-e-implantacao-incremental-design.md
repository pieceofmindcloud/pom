# Rota dinâmica entre peers e implantação incremental

Data: 2026-08-13

Duas mudanças, na mesma entrega por decisão do captain: escolher sozinho o
melhor caminho entre dois nós, e poder somar ou tirar uma máquina de um modelo
que já está no ar. São subsistemas independentes; a spec os mantém em seções
separadas.

## Por que

Três defeitos do mesmo tipo apareceram em 2026-08-13, todos por um nó receber um
endereço que não funciona para ele:

1. O endereço anunciado por gossipsub é o literal `0.0.0.0:<porta>`, e
   `insert_peer` não reescreve nada (`network.rs`). Um peer que chegue só por
   esse caminho fica com `inference_url` apontando para o próprio nó.
2. A entrada do próprio nó em `all_peers` usava `detect_local_ip()`, o IP privado
   da LAN (`gateway.rs`). Essa entrada viaja dentro de `next_hops` para ser
   discada **por outra máquina**: no plano `[salem→ksia]` o salem recebia
   `http://192.168.100.138:8092` e a requisição morria calada. Com a ordem
   invertida funcionava, porque aí o gateway falava consigo mesmo pela LAN. Já
   corrigido preferindo `advertise_addr`, o que resolve o caso mas não o padrão.
3. `PeerInfo.addr` é **um** endereço, reescrito a partir do IP de origem do
   pacote UDP. Um nó tem mais de um endereço válido, e qual deles serve depende
   de quem pergunta.

O padrão comum: um endereço único, escolhido por quem não vai discá-lo.

Medições que balizam o desenho, feitas hoje entre o MacBook e a ksia:

| caminho | p50 | p95 | mín |
|---|---|---|---|
| IPv4 da LAN (`192.168.100.138`) | 9,6 ms | 10,7 ms | 8,0 ms |
| IPv6 global (mesmo `/64`) | 7,2 ms | 8,8 ms | 6,4 ms |

`traceroute6` deu **um salto**: no mesmo `/64` o IPv6 global é on-link e não sobe
para o provedor. Ou seja, os dois caminhos são equivalentes, e a diferença cabe
dentro do ruído do Wi-Fi (desvio de 1,0 e 2,3 ms no ICMP). **Este empate é o
requisito mais importante do desenho**: uma política ingênua trocaria de rota a
cada medição.

## A. Rota dinâmica

### A1. Candidatos e filtragem

`Announce` ganha `addr_candidates: Vec<String>` (`host:porta` do P2P). Fontes, em
ordem de prioridade:

1. `advertise_addr`, quando existe. É intenção explícita do operador.
2. Interfaces locais enumeradas.
3. O endereço observado pelo peer, **acrescentado** ao conjunto em vez de
   substituir. É o único que enxerga NAT, porque o nó atrás dele não conhece o
   próprio endereço externo.

A filtragem é explícita e não opcional, porque o macOS gera meia dúzia de IPv6
sozinho (cinco nesta máquina hoje, dois marcados `deprecated`):

- descarta `deprecated`;
- descarta link-local sem escopo;
- descarta loopback, exceto quando os dois nós são a mesma máquina;
- prefere o autoconf `secured` ao `temporary`, que roda sozinho e expira;
- **limita a 2 candidatos por família**.

O limite não é estética: `PeerList` cresce ~530 bytes por peer e já é fatiado por
MTU em `peer_list_chunks`. Sem teto, um cluster de 9 peers com 6 endereços cada
transforma descoberta em fragmentação.

### A2. Medição e troca

A sonda é `GET /health` **por HTTP na porta de inferência**, não ICMP: mede o
caminho que a inferência usa de verdade, com firewall e proxy no meio. ICMP
liberado onde o TCP não está mediria um caminho que ninguém percorre.

- na descoberta, mede todos os candidatos e escolhe o menor;
- remede a cada 5 minutos;
- **só troca com margem: 25% e pelo menos 2 ms**;
- falha de um endereço remede na hora e escolhe outro.

A margem é o que a tabela acima exige. Sem ela, 6,4 contra 7,2 ms viraria troca
de rota a cada ciclo.

Desempate determinístico, quando a margem não decide: ordem de preferência
estática (mesma máquina → mesmo prefixo → global), e ordem lexicográfica como
último critério. Isso é o que faz dois nós chegarem à mesma conclusão em vez de
divergirem por ruído - previsibilidade era o pedido, e ela vale mais aqui do que
o milissegundo.

A tabela vive em memória, chaveada por `node_id`: candidato escolhido, RTT,
quando foi medido, e falhas consecutivas. Não é publicada no gossip. Publicar
tabela de medição transformaria endereçamento em estado distribuído, e o
`PeerList` já está no limite.

### A3. Fixação por sessão

A rota é resolvida uma vez por `session_id` e **não muda no meio da sessão**. O
cache KV do outro lado é por sessão; trocar de endereço no meio é pior que a rota
lenta. A escolha nova vale para sessões novas.

### A4. Plano declarativo

Hoje `PipelineStep.inference_url` é uma URL já resolvida por quem planejou. Ela
passa a ser acompanhada de `inference_candidates: Vec<String>`, e **quem disca
resolve** pela própria tabela de medição. É a propriedade que faltava: a
informação passa a ser usada por quem a mediu.

`inference_url` continua preenchido. Um step sem candidatos (nó velho) cai nele;
um nó velho lendo um plano novo encontra o campo que conhece. Um cluster de
versões misturadas não parte ao meio.

### A5. Interface

O canvas (`ModelNetworkCanvas.tsx`) já distingue dois estados: `stroke-iris`
sólida com seta para a rota escolhida e `stroke-faint/70` tracejada para as
demais, com legenda. Com réplicas (B1) passam a existir **três**:

| estado | traço | significado |
|---|---|---|
| rota principal | verde, sólida, com seta | o caminho em uso agora |
| rotas paralelas | azul | alternativas viáveis, prontas para assumir |
| inativa | cinza, tracejada | não utilizável no momento |

A aresta leva rótulo de endereço e latência (`[2804:d55…]:8092 · 7 ms`), que é o
dado que A2 passa a produzir. O card de cada nó ganha um campo "Rota" ao lado de
"Build".

Alternar entre principal e paralelas conforme carga, latência ou paralelismo é
**trabalho futuro**: esta entrega desenha as rotas e as torna visíveis; a escolha
por requisição continua sendo a que `plan_pipeline` já faz por carga.

### A6. Testes

A parte que erra calado é a escolha, então ela é uma função pura e testável:

- `choose_route(candidatos, medições, preferências)` respeita a margem e **não
  troca** dentro dela (o caso 6,4 contra 7,2 ms, escrito com esses números);
- a filtragem descarta `deprecated`, link-local e loopback, e respeita o teto por
  família;
- um step sem `inference_candidates` cai em `inference_url` (compatibilidade);
- um candidato que falha é substituído sem esperar o ciclo de 5 minutos.

## B. Implantação incremental

Hoje a implantação é um ato **atômico sobre o conjunto todo**: `/api/ui/deploy`
re-varre o cluster e aplica um plano completo, e `launch` **para a inferência das
máquinas alvo antes** de aplicar. Somar ou tirar uma máquina são operações
incrementais sobre uma implantação viva, e é aí que está a dificuldade - não nos
botões.

### B1. Adicionar: sempre somar, nunca substituir

Botão "Adicionar" no painel abre um diálogo com três campos: **nó**, **fatia** (do
inventário, marcando quais o nó já tem no disco e quais precisariam ser
transferidas) e "Implantar".

**Só o nó alvo é tocado.** Esta é a regra central: aplicar o caminho de hoje
derrubaria a inferência de quem já está servindo, pelo `launch`. O caminho
incremental aplica em um nó e deixa os demais exatamente como estão.

**Sobreposição de faixa deixa de ser recusa neste caminho: ela é o objetivo.**
Adicionar uma máquina que serve 0-19 quando outra já serve 0-19 cria uma
*segunda rota* pela rede. Duas consequências, as duas desejadas: duas requisições
simultâneas param de disputar o mesmo hop, e a queda de uma máquina deixa de
derrubar o modelo.

A recusa não some - ela **muda de lugar**. O que nunca pode acontecer é a mesma
camada rodar duas vezes *dentro de um mesmo plano*; ter dois candidatos para a
mesma faixa *no cluster* é outra coisa. Essa separação já existe no código:
`plan_pipeline` varre camada a camada e escolhe **um** peer por posição,
pontuando `layers_count * 100 - current_load * 10`. Ou seja, o planejador já
alterna réplicas por carga hoje; o que bloqueia a leitura 2 é apenas a recusa de
sobreposição no caminho de implantação.

Portanto:

- no plano de uma requisição: uma camada, um nó. Invariante mantida.
- no cluster: N nós podem cobrir a mesma faixa. Passa a ser permitido.

### B2. Remover

Botão "Remover" no card tira aquele nó da implantação daquele modelo, reusando o
caminho de `ReleaseSlice`, que já existe: quem libera a memória é o dono dela,
nunca um coordenador alcançando de fora.

Com réplicas, remover tem dois desfechos diferentes, e o diálogo precisa
distinguir os dois em vez de avisar sempre:

- **outra máquina cobre a mesma faixa**: não fica buraco nenhum. A rede perde uma
  rota paralela e segue servindo. Aviso ameno, sem confirmação pesada.
- **era a última daquela faixa**: fica **buraco na cobertura**. Não é recusa -
  meia implantação é legítima e o projeto já trata buraco como aviso - mas é
  confirmação explícita, dizendo quais camadas ficam descobertas e que o pipeline
  para de planejar até alguém assumir a faixa.

Nos dois casos as sessões em voo naquele hop caem, e o aviso diz isso.

### B3. Testes

- adicionar um nó não altera a config dos demais;
- faixa sobreposta é **aceita** e passa a existir como segunda rota;
- com duas réplicas da mesma faixa, um plano nunca inclui as duas (a invariante
  "uma camada, um nó por plano" continua valendo);
- remover uma réplica não reporta buraco; remover a última daquela faixa reporta,
  com as camadas descobertas na mensagem;
- remover o último nó de um modelo não deixa a implantação em estado meio
  aplicado.

## Fora de escopo

- **Banda como critério de rota.** A telemetria de 2026-08-13 mostra o enlace
  ocioso a 135 KB/s com o pipeline gastando 74% do tempo em ida e volta: o
  gargalo é latência. Medir banda custa banda, para decidir o que já está
  decidido.
- **Publicar a tabela de medição no gossip.** Ver A2.
- **Reordenar uma implantação viva.** Ordem do pipeline é ordem das camadas;
  mudar a ordem é mudar quem serve qual faixa, o que já é remover e adicionar.
- **Política de alternância entre rotas paralelas.** Ver A5. Fica com o que
  `plan_pipeline` já faz (pontuação por carga); latência da tabela de A2 e
  paralelismo real de requisições são entrega futura.
