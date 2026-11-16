/** Model ids that the chat presents as selectable targets. */
export function chatModelOptions(modelIds: Iterable<string>): string[] {
  const ids = Array.from(new Set(modelIds)).filter((id) => id && !id.includes("@d-"));
  const names = new Set(ids);
  // `<deployment_name>@<node_id>` pins one head: the Chat picks the head in its
  // own head control (Auto, or a machine), so only the deployment is listed.
  return ids
    .filter((id) => {
      const at = id.lastIndexOf("@");
      return at < 0 || !names.has(id.slice(0, at));
    })
    .sort();
}
