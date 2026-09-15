// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { probePublicAddress, resetPublicAddressReachabilityCache } from "./publicReachability";

const IPV6_PROBE_URL = "http://[2001:4860:4860::8888]/";

afterEach(() => {
  resetPublicAddressReachabilityCache();
});

describe("probePublicAddress", () => {
  it("marca um endereço IPv6 como ipv6-unavailable quando esta máquina não tem rota IPv6", async () => {
    const fetcher = (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === IPV6_PROBE_URL) return Promise.reject(new TypeError("network unreachable"));
      return Promise.reject(new TypeError("network unreachable"));
    };

    const status = await probePublicAddress("http://[2804:14c:87:8a11::1]:8080", fetcher);
    expect(status).toBe("ipv6-unavailable");
  });

  it("marca um endereço IPv6 como unreachable quando esta máquina tem rota IPv6 mas o peer não responde", async () => {
    const fetcher = (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === IPV6_PROBE_URL) return Promise.resolve(new Response("ok", { status: 200 }));
      return Promise.reject(new TypeError("network unreachable"));
    };

    const status = await probePublicAddress("http://[2804:14c:87:8a11::1]:8080", fetcher);
    expect(status).toBe("unreachable");
  });

  it("marca um endereço IPv4/hostname como unreachable sem consultar a sonda de IPv6", async () => {
    let ipv6ProbeCalled = false;
    const fetcher = (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === IPV6_PROBE_URL) {
        ipv6ProbeCalled = true;
        return Promise.resolve(new Response("ok", { status: 200 }));
      }
      return Promise.reject(new TypeError("network unreachable"));
    };

    const status = await probePublicAddress("http://192.0.2.10:8080", fetcher);
    expect(status).toBe("unreachable");
    expect(ipv6ProbeCalled).toBe(false);
  });

  it("marca um endereço alcançável como reachable", async () => {
    const fetcher = () => Promise.resolve(new Response("ok", { status: 200 }));
    const status = await probePublicAddress("http://198.51.100.4:8080", fetcher);
    expect(status).toBe("reachable");
  });
});
