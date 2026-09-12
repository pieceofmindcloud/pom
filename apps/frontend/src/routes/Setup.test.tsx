// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { useState } from "react";
import { LocaleProvider } from "../i18n/provider";
import { canContinue, isReadyToSave, TrustScreen, type SetupState } from "./Setup";

const initialSetup: SetupState = {
  profile: "infer",
  bootstrap: "",
  connectionKey: "",
  advertiseAddr: "",
  trustDocument: "",
  trustFingerprint: "",
  adminAccessToken: "",
  adminExposure: "localhost",
  tlsRequired: false,
  nodeName: "node",
  network: {
    public_name: "",
    organization: "",
    description: "",
    rules: "",
    channels: "",
    visibility: "private",
    access_policy: "authorized",
    country_code: "",
    country_name: "",
    city: "",
    geo_label: "",
    location: null,
  },
  ramReservePct: 20,
  cpuReservePct: 20,
  modelDirs: [],
  shardDir: "",
  shardMaxGb: 20,
  thermalLimitC: 85,
  setupModel: "",
};

function mount(value: SetupState = initialSetup) {
  function Harness() {
    const [setup, setSetup] = useState(value);
    return (
      <TrustScreen
        setup={setup}
        update={(patch) => setSetup((current) => ({ ...current, ...patch }))}
        updateNetwork={(patch) => setSetup((current) => ({ ...current, network: { ...current.network, ...patch } }))}
        t={(key) => key}
      />
    );
  }

  return render(
    <LocaleProvider initialLocale="pt-BR">
      <Harness />
    </LocaleProvider>,
  );
}

afterEach(() => cleanup());

describe("TrustScreen", () => {
  it("aceita trust.json por upload sem pedir confirmação manual", async () => {
    mount();
    const input = screen.getByTestId("trust-document-file") as HTMLInputElement;
    const document = JSON.stringify({ anchor: { principal: { pubkey: "admin-key" } } });

    fireEvent.change(input, {
      target: { files: [new File([document], "trust.json", { type: "application/json" })] },
    });

    expect(await screen.findByDisplayValue(document)).toBeTruthy();
    expect(screen.queryByRole("checkbox")).toBeNull();
  });

  it("pede o nome público no próprio passo de confiança", () => {
    mount({
      ...initialSetup,
      profile: "node",
      network: {
        ...initialSetup.network,
        visibility: "public",
        country_code: "BR",
        city: "Curitiba",
      },
    });

    expect(screen.getByTestId("public-network-name")).toBeTruthy();
  });

  it("não libera o último passo público sem nome da rede", () => {
    const setup: SetupState = {
      ...initialSetup,
      profile: "infer",
      trustDocument: "{}",
      trustFingerprint: "fingerprint",
      network: {
        ...initialSetup.network,
        visibility: "public",
        country_code: "BR",
        city: "Curitiba",
      },
    };

    expect(canContinue("setup-trust", setup)).toBe(false);
    expect(isReadyToSave(setup)).toBe(false);
    const named = { ...setup, network: { ...setup.network, public_name: "Rede Curitiba" } };
    expect(canContinue("setup-trust", named)).toBe(true);
    expect(isReadyToSave(named)).toBe(true);
  });
});
