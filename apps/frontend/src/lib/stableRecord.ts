import { useRef } from "react";

/** Dois mapas com as mesmas chaves e os mesmos valores por identidade. */
export function sameRecord<T>(a: Record<string, T>, b: Record<string, T>): boolean {
  if (a === b) return true;
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  return keys.every((key) => key in b && Object.is(a[key], b[key]));
}

/**
 * O mesmo mapa, com a **mesma identidade**, enquanto o conteúdo não mudar.
 *
 * Existe por causa de um defeito de verdade: um mapa derivado de `useQueries`
 * nasce novo a cada render, porque `useQueries` monta um array novo a cada
 * render. Quem depende desse mapa numa lista de dependências de efeito - ou o
 * compara por referência dentro do efeito - grava estado em todo render, e o
 * React aborta a árvore com "Maximum update depth exceeded": a tela congela e
 * nem a troca de rota commita mais. Ver `Chat.navigation.test.tsx`.
 *
 * A comparação é rasa de propósito: os valores vêm do react-query, que já
 * mantém a identidade de `data` estável entre renders.
 */
export function useStableRecord<T>(next: Record<string, T>): Record<string, T> {
  const held = useRef(next);
  if (!sameRecord(held.current, next)) held.current = next;
  return held.current;
}
