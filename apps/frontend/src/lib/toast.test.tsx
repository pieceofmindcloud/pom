// @vitest-environment jsdom
import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/provider";
import { ToastProvider, useToast } from "./toast";
import { useEffect } from "react";

function PushToast() {
  const toast = useToast();
  useEffect(() => {
    toast.success("saved");
  }, [toast]);
  return null;
}

describe("ToastProvider", () => {
  afterEach(() => {
    cleanup();
    vi.useRealTimers();
  });

  it("clears auto-dismiss timers when the provider unmounts", () => {
    vi.useFakeTimers();
    const view = render(
      <LocaleProvider initialLocale="en">
        <ToastProvider>
          <PushToast />
        </ToastProvider>
      </LocaleProvider>,
    );

    const timersBeforeUnmount = vi.getTimerCount();
    expect(timersBeforeUnmount).toBeGreaterThan(0);
    view.unmount();
    expect(vi.getTimerCount()).toBeLessThan(timersBeforeUnmount);
  });
});
