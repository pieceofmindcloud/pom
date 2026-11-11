// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { MemoryRouter } from "react-router-dom";
import { LocaleProvider } from "../i18n/provider";

const { post, rescanModels, extra } = vi.hoisted(() => ({ post: vi.fn(), rescanModels: vi.fn().mockResolvedValue({}), extra: { value: {} as Record<string, unknown> } }));

vi.mock("../lib/api", () => ({ api: { post }, rescanModels }));
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
      workspace_root: {
        path: "/data/workspace",
        exists: true,
        kind: "folder",
        used_bytes: 0,
        max_bytes: null,
        usage_percent: null,
      },
      using_defaults: true,
      detected: [],
      default_shard_dir: "/models",
      default_workspace_root: "/data/workspace",
      ...extra.value,
    },
    isLoading: false,
    isError: false,
    error: null,
    refetch: vi.fn(),
  }),
  useInvalidate: () => vi.fn(),
}));
vi.mock("../lib/toast", () => ({
  useToast: () => ({ success: vi.fn(), info: vi.fn(), fromError: vi.fn() }),
}));

import { ModelPaths } from "./ModelPaths";

describe("ModelPaths", () => {
  afterEach(() => {
    cleanup();
    extra.value = {};
  });

  it("asks this machine to walk its model folders again", async () => {
    render(
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="en">
          <ModelPaths />
        </LocaleProvider>
      </MemoryRouter>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Rescan folders" }));
    await waitFor(() => expect(rescanModels).toHaveBeenCalledWith());
  });

  it("has no slice folder: received files go to a model folder", () => {
    render(
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <ModelPaths />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("Armazenamento")).toBeTruthy();
    expect(screen.getByDisplayValue("/data/workspace")).toBeTruthy();
    expect(screen.queryByText("Pasta de fatias")).toBeNull();
    expect(screen.queryByRole("progressbar")).toBeNull();
  });

  it("persists the shared workspace path with the other storage settings", async () => {
    post.mockResolvedValue({
      model_dirs: [],
      shard_dir: {
        path: "/models/slices",
        exists: true,
        kind: "folder",
        used_bytes: 0,
        max_bytes: 100 * 1024 ** 3,
        usage_percent: 0,
      },
      workspace_root: {
        path: "/persisted/workspace",
        exists: true,
        kind: "folder",
        used_bytes: 0,
        max_bytes: null,
        usage_percent: null,
      },
      shard_max_gb: 100,
      using_defaults: true,
      detected: [],
      default_shard_dir: "/models",
      default_workspace_root: "/data/workspace",
    });
    render(
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <ModelPaths />
        </LocaleProvider>
      </MemoryRouter>,
    );

    fireEvent.change(screen.getByDisplayValue("/data/workspace"), {
      target: { value: "/persisted/workspace" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Salvar" }));

    await waitFor(() =>
      expect(post).toHaveBeenCalledWith("/api/ui/paths", expect.objectContaining({
        workspace_root: "/persisted/workspace",
      })),
    );
  });

  /**
   * With POM_MODEL_DIRS/POM_SHARD_DIR set, the screen shows the variable's
   * value, locks the fields and says where it comes from - editing there would
   * have no effect, and the backend does not write the field.
   */
  it("locks the folders set by environment variables and names the variable", () => {
    extra.value = {
      model_dirs: [
        { path: "/data/models", exists: true, kind: "folder", used_bytes: 0, max_bytes: null, usage_percent: null },
      ],
      using_defaults: false,
      model_dirs_env: "POM_MODEL_DIRS",
      shard_dir_env: "POM_SHARD_DIR",
    };
    render(
      <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
        <LocaleProvider initialLocale="pt-BR">
          <ModelPaths />
        </LocaleProvider>
      </MemoryRouter>,
    );

    expect(screen.getByText("Definido por POM_MODEL_DIRS")).toBeTruthy();
    expect(screen.getByText(/A variável de ambiente POM_MODEL_DIRS define este valor/)).toBeTruthy();
    expect((screen.getByDisplayValue("/data/models") as HTMLInputElement).disabled).toBe(true);
    expect((screen.getByDisplayValue("/data/workspace") as HTMLInputElement).disabled).toBe(false);
  });
});
