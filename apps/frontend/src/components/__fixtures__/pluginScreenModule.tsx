/**
 * A stand-in for a plugin's screen module, used by `PluginScreen.test.tsx`.
 *
 * `signalling` behaves like a plugin that knows the readiness signal: it draws
 * its content and calls `__POM_HOST__.ready()` once that content is on screen.
 * `silent` behaves like a plugin that does not know it yet - POM has to open it
 * by itself.
 */
import { useEffect } from "react";

type Host = { ready?: () => void } | undefined;

export function signalling() {
  useEffect(() => {
    const host: Host = (globalThis as { __POM_HOST__?: Host }).__POM_HOST__;
    host?.ready?.();
  }, []);
  return <div>plugin-screen:signalling</div>;
}

export function silent() {
  return <div>plugin-screen:silent</div>;
}
type Loader = {
  progress: (request: { progress: number; message?: string }) => Promise<unknown>;
  error: (request: { message: string; retry?: boolean }) => Promise<unknown>;
  success: () => Promise<unknown>;
};
type LoaderHost = { ready?: () => void; plugin?: (code: string) => { loader: Loader } } | undefined;

function loaderHost() {
  const host: LoaderHost = (globalThis as { __POM_HOST__?: LoaderHost }).__POM_HOST__;
  return { ready: () => host?.ready?.(), loader: host?.plugin?.("logs").loader };
}

/** Installs something: reports progress, and calls `ready()` too, which must not open it. */
export function installing() {
  useEffect(() => {
    const { ready, loader } = loaderHost();
    void loader?.progress({ progress: 30, message: "Installing Python packages" });
    ready();
  }, []);
  return <div>plugin-screen:installing</div>;
}

/** Opens at once, then its backend dies and it covers itself with an error. */
export function failsLater() {
  useEffect(() => {
    const { ready, loader } = loaderHost();
    ready();
    const later = setTimeout(() => void loader?.error({ message: "Hermes Agent stopped" }), 300);
    return () => clearTimeout(later);
  }, []);
  return <div>plugin-screen:failsLater</div>;
}

/** Installs, then reports success. */
export function succeeds() {
  useEffect(() => {
    const { loader } = loaderHost();
    void loader?.progress({ progress: 50 });
    const later = setTimeout(() => void loader?.success(), 100);
    return () => clearTimeout(later);
  }, []);
  return <div>plugin-screen:succeeds</div>;
}
