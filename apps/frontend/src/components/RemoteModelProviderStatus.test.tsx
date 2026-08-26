// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { RemoteModelProviderStatus } from "./RemoteModelProviderStatus";

afterEach(cleanup);

describe("RemoteModelProviderStatus", () => {
  it("explains when the CLI is absent instead of exposing a raw provider error", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <RemoteModelProviderStatus
          status={{
            cli: { installed: false, version: null, authenticated: false, username: null },
          }}
        />
      </LocaleProvider>,
    );

    expect(screen.getByText("O CLI do Hugging Face não está instalado nesta máquina.")).toBeTruthy();
  });

  it("distinguishes an installed CLI that still needs authentication", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <RemoteModelProviderStatus
          status={{
            cli: { installed: true, version: "1.2.3", authenticated: false, username: null },
          }}
        />
      </LocaleProvider>,
    );

    expect(screen.getByText("Autenticação do Hugging Face necessária.")).toBeTruthy();
    expect(screen.getByRole("link", { name: /configurações/i }).getAttribute("href")).toBe(
      "/config/rede#hugging-face",
    );
  });
});
