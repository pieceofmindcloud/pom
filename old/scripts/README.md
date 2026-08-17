# Legacy shell scripts

These manual launch and request scripts predate the workspace layout. They are
kept for operator reference only and are not invoked by Cargo, the node binary,
or the release pipeline.

Use `cargo run --bin pom -- <arguments>` during development, or the
released `pom` binary. The node reads the frontend from
`apps/frontend/dist` at runtime (`POM_UI_DIST` overrides the path); build
it with `npm run build` in `apps/frontend`. Node and npm are only needed to
build the interface, never to run the binary.
