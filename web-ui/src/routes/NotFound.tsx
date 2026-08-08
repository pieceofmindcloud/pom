import { Compass } from "lucide-react";
import { Link } from "react-router-dom";
import { Button, Empty, Panel } from "../components/ui";

export function NotFound() {
  return (
    <Panel>
      <Empty
        icon={<Compass className="h-5 w-5" />}
        title="Esta tela não existe"
        action={
          <Link to="/">
            <Button variant="primary">Voltar ao painel</Button>
          </Link>
        }
      >
        O endereço digitado não corresponde a nenhuma tela do CODAI-SERVER.
      </Empty>
    </Panel>
  );
}
