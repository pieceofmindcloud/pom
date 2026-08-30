# Proteção térmica da GPU

## Contexto

`utils::sysmon` já é a fonte única de inventário de GPU do nó. Ele enumera
NVIDIA por `nvidia-smi`, Radeon por `amdgpu` no sysfs e Apple Silicon como GPU
unificada. A leitura `temperature_c` permanece opcional por placa: um driver
que devolve `N/A`, uma GPU que não expõe temperatura ou uma ferramenta ausente
produz `null`, nunca zero.

O runtime de inferência é o dono da memória e já possui parada coordenada por
geração. A proteção térmica reutiliza essa parada, retirando a assignment do
anúncio antes de encerrar a geração atual, e guarda a configuração pretendida
em memória para reimplantá-la somente depois de um período seguro.

## Decisões

- A política vive dentro de `Config`, persistida no mesmo `pom.json` das demais
  configurações do nó. Os campos são `thermal.limit_c`,
  `thermal.hysteresis_c` e `thermal.cooldown_secs`.
- Os padrões são 85 °C para suspensão, 8 °C de margem e 30 s de temperatura
  segura contínua. Assim, a retomada exige todas as GPUs conhecidas em no
  máximo 77 °C durante todo o cooldown.
- Uma amostra conhecida acima do limite suspende imediatamente. Temperatura
  desconhecida nunca suspende, mas também não conta como prova de segurança
  para retomar uma implantação suspensa.
- A máquina de histerese é lógica pura e recebe uma lista de `Option<f64>`.
  Ela não conhece fornecedor, processo, modelo ou transporte.
- O monitor lê `sysmon::sample()` a cada 5 s e lê a política da configuração
  compartilhada a cada ciclo. A amostra de sistema continua com o cache de
  1,2 s existente.
- `RuntimeStatus::Thermal` e uma mensagem com temperatura, limite e limiar de
  retomada distinguem espera térmica de queda ou desimplantação manual. O nó
  retira a assignment da rede enquanto aguarda.
- `NodeRuntime::stop` é sempre uma decisão manual e limpa a configuração de
  recuperação térmica. Aplicações feitas enquanto o hardware está bloqueado
  viram a nova intenção pendente, mas não carregam pesos.
- A API de política é `GET/POST /api/ui/thermal`. O painel de Rede permite
  editar limite, margem e cooldown; os cards usam a política devolvida pelo
  próprio peer para classificar normal, atenção e crítico.

## Fluxo

1. O monitor captura a temperatura máxima para registrar o evento, mas a
   decisão usa todas as GPUs.
2. Ao detectar calor, o runtime salva a configuração ativa como intenção,
   incrementa a geração para que o servidor encerre de modo coordenado, limpa
   assignment e cache, anuncia ausência de camadas e escreve log com os
   valores observados.
3. Durante o bloqueio, `apply` não inicia servidor: mantém a intenção e o
   status térmico.
4. Depois de todas as temperaturas conhecidas abaixo de `limit - hysteresis`
   por `cooldown_secs`, o monitor limpa o bloqueio e chama `apply` com a
   intenção ainda presente. Se o operador usou `stop`, ela não existe mais e
   nada é reimplantado.
5. O endpoint de nó continua servindo a amostra atual. O card de Fluxo mostra
   a maior temperatura conhecida; Detalhes mostra uma leitura por GPU e um
   estado explícito de desconhecido quando a leitura falta.

## Fora de escopo

- Não há inventário ou API de fornecedor nova. Apple Silicon continua com
  temperatura desconhecida enquanto o sistema não expuser uma leitura segura.
- Não há política de desligamento da máquina, controle de fan ou migração
  automática da faixa para outro peer.
- Não se altera o protocolo de inferência nem se introduz compatibilidade com
  versões antigas.

