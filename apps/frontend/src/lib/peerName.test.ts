import { describe, expect, it } from "vitest";
import { peerDisplayName, uniquePeerName } from "./peerName";

/**
 * The "Send to..." selector on the slices screen showed the wrong label: it
 * read `/api/ui/cluster/models`, which answered with the operating system's
 * raw hostname instead of the name the node announces (`node_name` from
 * pom.json). On a machine with two nodes it was worse - both reported the same
 * OS hostname, the list saw a collision and broke the tie with the `node_id` prefix.
 */
describe("peerDisplayName", () => {
  it("uses the name the machine announces", () => {
    expect(peerDisplayName({ node_id: "83bee8670abc2155", hostname: "ksia" })).toBe("ksia");
  });

  /**
   * The shortened `node_id` is the last resort, not the name: it is a hexadecimal
   * prefix, and does not say which machine the file is going to.
   */
  it("falls back to the short id only when there is no name at all", () => {
    expect(peerDisplayName({ node_id: "83bee8670abc2155" })).toBe("83bee8670abc");
    expect(peerDisplayName({ node_id: "83bee8670abc2155", hostname: null })).toBe("83bee8670abc");
    expect(peerDisplayName({ node_id: "83bee8670abc2155", hostname: "  " })).toBe("83bee8670abc");
  });
});

describe("uniquePeerName", () => {
  it("does not clutter a name that is already unique", () => {
    const peers = [
      { node_id: "aaaa1111bbbb2222", hostname: "atena" },
      { node_id: "cccc3333dddd4444", hostname: "ksia" },
    ];
    expect(uniquePeerName(peers[0], peers)).toBe("atena");
    expect(uniquePeerName(peers[1], peers)).toBe("ksia");
  });

  /**
   * Two nodes on the same machine share a hostname on purpose - it is the way
   * to give a large window without risking the smaller one's RAM. Then only the
   * `node_id` tells one from the other.
   */
  it("tie-breaks two nodes of the same machine by id", () => {
    const peers = [
      { node_id: "aaaa1111bbbb2222", hostname: "ksia" },
      { node_id: "cccc3333dddd4444", hostname: "ksia" },
    ];
    expect(uniquePeerName(peers[0], peers)).toBe("ksia · aaaa11");
    expect(uniquePeerName(peers[1], peers)).toBe("ksia · cccc33");
  });
});
