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