# Legacy shell scripts

These manual launch and request scripts predate the workspace layout. They are
kept for operator reference only and are not invoked by Cargo, the node binary,
or the release pipeline.

Use `cargo run --bin rdma-p2p -- <arguments>` during development, or the
released `rdma-p2p` binary. The node reads the frontend from
`apps/frontend/dist` at runtime (`RDMA_P2P_UI_DIST` overrides the path); build
it with `npm run build` in `apps/frontend`. Node and npm are only needed to
build the interface, never to run the binary.
