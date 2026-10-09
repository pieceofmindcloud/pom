/**
 * Como uma transferência se apresenta na tela.
 *
 * O nome de uma fatia em disco é o **sha256** dela, e hash não identifica
 * nada que se leia: a notificação mostrava `df48d094c3d1023c0dab9e…` e o
 * operador não tinha como saber de que modelo era nem que camadas trazia. Quem
 * sabe as duas coisas é o `.pom`, que viaja com a oferta e é gravado antes do
 * primeiro byte de peso — o backend o lê e manda `model_label` e a faixa
 * junto com o job.
 *
 * O hash não desaparece: ele continua sendo o que identifica a parte entre
 * máquinas e o que nomeia o arquivo em disco. Só deixa de ser o título.
 */
import { layerRange } from "./format";

/** O que basta para nomear uma fatia — job ou oferta. */
export interface SliceNamed {
  name: string;
  model_label?: string | null;
  start_layer?: number | null;
  end_layer?: number | null;
}

/**
 * O título: modelo e faixa de camadas, com o nome do arquivo como recurso
 * final. Um modelo inteiro não tem faixa e continua se chamando pelo nome.
 */
export function transferTitle(item: SliceNamed): string {
  const range =
    item.start_layer != null && item.end_layer != null
      ? layerRange(item.start_layer, item.end_layer)
      : null;
  if (item.model_label && range) return `${item.model_label} · ${range}`;
  if (item.model_label) return item.model_label;
  if (range) return `${range} · ${item.name}`;
  return item.name;
}

/**
 * O hash curto, quando o nome do arquivo é um sha256 — a identificação
 * secundária que fica ao lado, discreta. `null` quando o nome já é legível,
 * porque aí repeti-lo não acrescenta nada.
 */
export function transferHash(name: string): string | null {
  const stem = name.replace(/\.gguf$/i, "");
  return /^[0-9a-f]{64}$/i.test(stem) ? stem.slice(0, 12) : null;
}
