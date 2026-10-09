import { useRef } from "react";

/** Two maps with the same keys and the same values by identity. */
export function sameRecord<T>(a: Record<string, T>, b: Record<string, T>): boolean {
  if (a === b) return true;
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  return keys.every((key) => key in b && Object.is(a[key], b[key]));
}

/**
 * The same map, with the **same identity**, while the content does not change.
 *
 * It exists because of a real defect: a map derived from `useQueries`
 * is born new on every render, because `useQueries` builds a new array on every
 * render. Whoever depends on that map in an effect's dependency list - or
 * compares it by reference inside the effect - writes state on every render, and
 * React aborts the tree with "Maximum update depth exceeded": the screen freezes
 * and not even a route change commits anymore. See `Chat.navigation.test.tsx`.
 *
 * The comparison is shallow on purpose: the values come from react-query, which already
 * keeps the identity of `data` stable between renders.
 */
export function useStableRecord<T>(next: Record<string, T>): Record<string, T> {
  const held = useRef(next);
  if (!sameRecord(held.current, next)) held.current = next;
  return held.current;
}
