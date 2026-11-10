// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { deleteModel } from "./deleteModel";
import { api } from "./api";
import { ApiError } from "./api";

afterEach(() => vi.restoreAllMocks());

describe("deleteModel", () => {
  it("does not call the node when the operator gives up at the first question", async () => {
    const post = vi.spyOn(api, "post");
    const out = await deleteModel("/m/f.gguf", "f.gguf", () => false);

    expect(out.status).toBe("cancelled");
    expect(post).not.toHaveBeenCalled();
  });

  it("deletes right away when there is no warning at all", async () => {
    const post = vi.spyOn(api, "post").mockResolvedValue({ freed_bytes: 42 } as never);
    const out = await deleteModel("/m/f.gguf", "f.gguf", () => true);

    expect(out).toEqual({ status: "deleted", freedBytes: 42 });
    expect(post).toHaveBeenCalledTimes(1);
    expect(post).toHaveBeenCalledWith("/api/ui/models/delete", { path: "/m/f.gguf" });
  });

  /// The 409 carries the reason. Asking again with the reason in view is the
  /// difference between warning and simply obeying.
  it("shows the 409 warnings and only forces after confirming", async () => {
    const post = vi
      .spyOn(api, "post")
      .mockRejectedValueOnce(
        new ApiError(409, "conflito", {
          warnings: [{ kind: "serving", message: "servindo as camadas 0-15" }],
        }),
      )
      .mockResolvedValueOnce({ freed_bytes: 7 } as never);

    const questions: string[] = [];
    const out = await deleteModel("/m/f.gguf", "f.gguf", async (m) => {
      await Promise.resolve();
      questions.push(m);
      return true;
    });

    expect(out).toEqual({ status: "deleted", freedBytes: 7 });
    expect(questions[1]).toContain("servindo as camadas 0-15");
    expect(post).toHaveBeenLastCalledWith("/api/ui/models/delete", {
      path: "/m/f.gguf",
      force: true,
    });
  });

  it("does not force when the operator declines in the face of the warnings", async () => {
    const post = vi.spyOn(api, "post").mockRejectedValueOnce(
      new ApiError(409, "conflito", {
        warnings: [{ kind: "transfer", message: "transferência em curso" }],
      }),
    );

    let first = true;
    const out = await deleteModel("/m/f.gguf", "f.gguf", () => {
      const response = first;
      first = false;
      return response;
    });

    expect(out.status).toBe("cancelled");
    expect(post).toHaveBeenCalledTimes(1);
  });

  /// An error without warnings is a real error and has to surface to the toast,
  /// instead of becoming a silent "cancelled".
  it("propagates an error that is not the 409 of warnings", async () => {
    vi.spyOn(api, "post").mockRejectedValue(new ApiError(500, "disco morreu"));

    await expect(deleteModel("/m/f.gguf", "f.gguf", () => true)).rejects.toThrow(
      "disco morreu",
    );
  });
});
