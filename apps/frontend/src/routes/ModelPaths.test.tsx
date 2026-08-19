// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";

const { post } = vi.hoisted(() => ({ post: vi.fn() }));

vi.mock("../lib/api", () => ({ api: { post } }));
vi.mock("../lib/queries", () => ({
  usePaths: () => ({
    data: {
      model_dirs: [],
      shard_dir: {
        path: "/models/slices",
        exists: true,
        kind: "folder",
        used_bytes: 25 * 1024 ** 3,
        max_bytes: 100 * 1024 ** 3,
        usage_percent: 25,
      },
      shard_max_gb: 100,
      using_defaults: true,
      detected: [],
      default_shard_dir: "/models",
    },
    isLoading: false,
    isError: false,
    error: null,
    refetch: vi.fn(),
  }),
  useInvalidate: () => vi.fn(),
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), fromError: vi.fn() }),
}));

import { ModelPaths } from "./ModelPaths";

describe("ModelPaths", () => {
  afterEach(cleanup);

  it("shows the slice storage limit and current usage", () => {
    render(
      <MemoryRouter>
        <LocaleProvider initialLocale="pt-BR">
          <ModelPaths />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("Armazenamento")).toBeTruthy();
    expect((screen.getByRole("spinbutton") as HTMLInputElement).value).toBe("100");
    expect(screen.getByText("25 GB usados de 100 GB")).toBeTruthy();
    expect(screen.getByRole("progressbar").getAttribute("aria-valuenow")).toBe("25");
  });
});
