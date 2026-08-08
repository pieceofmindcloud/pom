import { Construction } from "lucide-react";
import { Empty, PageHeader, Panel } from "../components/ui";

export function TokenKeys() {
  return (
    <>
      <PageHeader title="API keys" />
      <Panel>
        <Empty icon={<Construction className="h-5 w-5" />} title="Em construção" />
      </Panel>
    </>
  );
}

export function ConnectionKeys() {
  return (
    <>
      <PageHeader title="Chaves de conexão" />
      <Panel>
        <Empty icon={<Construction className="h-5 w-5" />} title="Em construção" />
      </Panel>
    </>
  );
}
