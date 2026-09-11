// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { useState } from "react";
import { LocaleProvider } from "../i18n/provider";
import { TrustScreen, type SetupState } from "./Setup";

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

function mount() {
  function Harness() {
    const [setup, setSetup] = useState(initialSetup);
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
});
