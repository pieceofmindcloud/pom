/**
 * A random name made of two words and a number, like `quiet-harbor-42`: what
 * the first run wizard offers for a machine or a network whose operator has
 * no name in mind.
 */
const ADJECTIVES = [
  "amber", "bold", "brisk", "calm", "clever", "copper", "crisp", "dusty", "eager", "gentle",
  "golden", "hidden", "humble", "ivory", "jolly", "keen", "lively", "lucky", "mellow", "misty",
  "noble", "polar", "proud", "quiet", "rapid", "rustic", "silent", "silver", "solar", "steady",
  "sunny", "swift", "tidal", "vivid", "wild", "witty",
];
const NOUNS = [
  "anchor", "badger", "beacon", "birch", "canyon", "cedar", "comet", "coral", "delta", "ember",
  "falcon", "fjord", "forest", "garden", "glacier", "harbor", "heron", "island", "lagoon", "lantern",
  "maple", "meadow", "nebula", "orchard", "otter", "pebble", "prairie", "quartz", "raven", "reef",
  "river", "summit", "thicket", "tundra", "valley", "willow",
];

function pick<T>(items: T[], random: () => number): T {
  return items[Math.min(items.length - 1, Math.floor(random() * items.length))];
}

export function randomSlug(random: () => number = Math.random): string {
  const number = 10 + Math.min(89, Math.floor(random() * 90));
  return `${pick(ADJECTIVES, random)}-${pick(NOUNS, random)}-${number}`;
}
