import { Bot, ChevronDown } from "lucide-react";
import { useI18n } from "../../i18n/provider";
import { chatAgentKey, type ChatAgent } from "../../lib/types";

/**
 * Beside the model picker: talk to the node's model (the default) or to an
 * ACP agent a plugin of this machine serves. Hidden when no plugin serves one.
 */
export function AgentPicker({
  agents,
  value,
  onChange,
}: {
  agents: ChatAgent[];
  value: string | null;
  onChange: (next: string | null) => void;
}) {
  const { t } = useI18n();
  if (agents.length === 0) return null;
  return (
    <div className="group relative inline-flex h-8 min-w-0 items-center rounded-lg border border-line bg-raised transition-colors duration-150 hover:border-line-strong focus-within:border-iris focus-within:ring-2 focus-within:ring-iris/25">
      <Bot className="pointer-events-none absolute left-2.5 h-3.5 w-3.5 text-faint" />
      <select
        value={value ?? ""}
        aria-label={t("chat.agent.pickerLabel")}
        onChange={(event) => onChange(event.target.value || null)}
        className="h-8 w-full min-w-0 max-w-[14rem] cursor-pointer appearance-none truncate bg-transparent pl-8 pr-7 text-[0.78rem] text-fg focus:outline-none"
      >
        <option value="">{t("chat.agent.model")}</option>
        {agents.map((agent) => (
          <option key={chatAgentKey(agent)} value={chatAgentKey(agent)}>
            {agent.name}
          </option>
        ))}
      </select>
      <ChevronDown className="pointer-events-none absolute right-2 h-3.5 w-3.5 text-faint" />
    </div>
  );
}
