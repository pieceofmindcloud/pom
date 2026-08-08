import { Construction } from "lucide-react";
import { Empty, PageHeader, Panel } from "../components/ui";

export function SendFile() {
  return (
    <>
      <PageHeader title="SendFile" />
      <Panel>
        <Empty icon={<Construction className="h-5 w-5" />} title="Em construção" />
      </Panel>
    </>
  );
}
