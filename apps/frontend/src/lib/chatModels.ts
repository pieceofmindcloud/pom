/** Model ids that the test chat presents as selectable targets. */
export function chatModelOptions(modelIds: Iterable<string>): string[] {
  return Array.from(new Set(modelIds))
    .filter((id) => id && !id.includes("@d-"))
    .sort();
}
