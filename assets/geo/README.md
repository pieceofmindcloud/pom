# assets/geo

`csv-cities.csv.gz` is the cities export of the
[Countries States Cities Database](https://github.com/dr5hn/countries-states-cities-database),
release `v3.2-export.31`, byte for byte as published:

```
sha256  8d72a3f8c30edf237e37c20a90c14b1a4da498835a2acde6c3b6b1bbd2e39baa
source  https://github.com/dr5hn/countries-states-cities-database/releases/download/v3.2-export.31/csv-cities.csv.gz
```

It is made available under the Open Database License 1.0
(https://opendatacommons.org/licenses/odbl/1-0/); the upstream repository's
`LICENSE` is the authoritative text. Attribution: "Data by Countries States
Cities Database", with a link to the repository. The product shows it on the
About screen and in the location dialog.

The file is not modified. `crates/node/src/utils/geo.rs` embeds it in the
binary, decodes it on first use and keeps only the columns it reads in memory;
its test `the_embedded_file_is_the_pinned_release` fails when the file and the
checksum above disagree.

## Updating

1. Download the `csv-cities.csv.gz` asset of the new release over this file.
2. Put the release tag and the new `sha256sum` here and in `RELEASE` / `SHA256`
   of `crates/node/src/utils/geo.rs`.
3. Run `cargo test geo::`.

Each update adds about 5 MB to the repository history, so do it when the data
is needed, not on every upstream release. A network stores the `id` of its
city: a release that drops or renumbers a city makes that network's settings
refuse to save until its admin picks the city again.
