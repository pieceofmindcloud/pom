export interface ScrollMetrics {
  scrollTop: number;
  clientHeight: number;
  scrollHeight: number;
}

/** Whether a new streaming token may safely keep the conversation at the end. */
export function isNearBottom({ scrollTop, clientHeight, scrollHeight }: ScrollMetrics, threshold = 32) {
  return scrollHeight - (scrollTop + clientHeight) <= threshold;
}
