// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";

vi.mock("../lib/queries", () => ({ useCluster: () => ({ data: { peers: [] } }) }));
vi.mock("./Logs", () => ({ StatsView: () => <div>stats-content</div> }));

import { Statistics } from "./Statistics";

describe("Statistics", () => {
  afterEach(cleanup);

  it("renders the statistics screen outside Logs", () => {
    render(
      <LocaleProvider initialLocale="pt-BR">
        <Statistics />
      </LocaleProvider>,
    );

    expect(screen.getByText("Estatísticas")).toBeTruthy();
    expect(screen.getByText("stats-content")).toBeTruthy();
  });
});
