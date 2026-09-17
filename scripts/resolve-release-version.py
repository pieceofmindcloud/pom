#!/usr/bin/env python3
"""Choose the next release version from a release-list response."""

from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass


SEMVER_RE = re.compile(
    r"^(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)\."
    r"(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
BUILD_RE = re.compile(r"^build\.([0-9]+)$")


@dataclass(frozen=True)
class Version:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...]
    build: str | None

    @property
    def build_number(self) -> int:
        if self.build is None:
            return 0
        match = BUILD_RE.fullmatch(self.build)
        return int(match.group(1)) if match else 0

    def with_build_number(self, number: int) -> str:
        base = f"{self.major}.{self.minor}.{self.patch}"
        if self.prerelease:
            base += "-" + ".".join(self.prerelease)
        return f"{base}+build.{number}"


def parse_version(raw: object, context: str) -> Version:
    if not isinstance(raw, str):
        raise ValueError(f"{context} version must be a string")
    match = SEMVER_RE.fullmatch(raw)
    if not match:
        raise ValueError(f"{context} has invalid SemVer: {raw!r}")
    prerelease = tuple(match.group(4).split(".")) if match.group(4) else ()
    for identifier in prerelease:
        if identifier.isdigit() and len(identifier) > 1 and identifier[0] == "0":
            raise ValueError(f"{context} has invalid numeric prerelease identifier: {raw!r}")
    return Version(
        major=int(match.group(1)),
        minor=int(match.group(2)),
        patch=int(match.group(3)),
        prerelease=prerelease,
        build=match.group(5),
    )


def compare_core(left: Version, right: Version) -> int:
    numeric = (left.major, left.minor, left.patch)
    other_numeric = (right.major, right.minor, right.patch)
    if numeric != other_numeric:
        return 1 if numeric > other_numeric else -1
    if not left.prerelease and not right.prerelease:
        return 0
    if not left.prerelease:
        return 1
    if not right.prerelease:
        return -1
    for left_id, right_id in zip(left.prerelease, right.prerelease):
        if left_id == right_id:
            continue
        left_numeric = left_id.isdigit()
        right_numeric = right_id.isdigit()
        if left_numeric and right_numeric:
            return 1 if int(left_id) > int(right_id) else -1
        if left_numeric != right_numeric:
            return -1 if left_numeric else 1
        return 1 if left_id > right_id else -1
    if len(left.prerelease) == len(right.prerelease):
        return 0
    return 1 if len(left.prerelease) > len(right.prerelease) else -1


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} BASE_VERSION", file=sys.stderr)
        return 2

    try:
        base = parse_version(sys.argv[1], "base version")
        payload = json.load(sys.stdin)
        releases = payload.get("releases") if isinstance(payload, dict) else None
        if not isinstance(releases, list):
            raise ValueError("release response must contain a releases array")

        selected = base
        for index, release in enumerate(releases):
            if not isinstance(release, dict):
                raise ValueError(f"release {index} must be an object")
            candidate = parse_version(release.get("version"), f"release {index}")
            comparison = compare_core(candidate, selected)
            if comparison > 0 or (comparison == 0 and candidate.build_number > selected.build_number):
                selected = candidate

        print(selected.with_build_number(selected.build_number + 1))
        return 0
    except (json.JSONDecodeError, OSError, ValueError) as error:
        print(f"resolve-release-version: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
