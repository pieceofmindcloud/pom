// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { placeLabel, publishedPoint, type NetworkPlace } from "./geo";

const place: NetworkPlace = {
  city_id: 1,
  country_code: "BR",
  country_name: "Brazil",
  state: "Paraná",
  city: "Curitiba",
  location: { latitude: -25.4412, longitude: -49.2337 },
};

describe("publishedPoint", () => {
  it("keeps the pin only when the network publishes the exact point", () => {
    expect(publishedPoint({ ...place, location_precision: "exact" })).toEqual({ latitude: -25.4412, longitude: -49.2337 });
  });

  it("snaps the pin to the node's grid by default", () => {
    expect(publishedPoint(place)).toEqual({ latitude: -25.4, longitude: -49.2 });
    expect(publishedPoint({ ...place, location_precision: "approximate" })).toEqual({ latitude: -25.4, longitude: -49.2 });
  });

  it("has no point of its own when only the city is published", () => {
    expect(publishedPoint({ ...place, location_precision: "city" })).toBeNull();
    expect(publishedPoint({ ...place, location: null })).toBeNull();
  });
});

describe("placeLabel", () => {
  it("names the state, since city names repeat inside a country", () => {
    expect(placeLabel(place)).toBe("Curitiba, Paraná, Brazil");
    expect(placeLabel({ city: "", state: "", country_name: "", country_code: "" })).toBe("");
  });
});
