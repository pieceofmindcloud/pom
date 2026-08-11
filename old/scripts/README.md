# Legacy shell scripts

These manual launch and request scripts predate the workspace layout. They are
kept for operator reference only and are not invoked by Cargo, the node binary,
or the release pipeline.

Use `cargo run --bin rdma-p2p -- <arguments>` during development, or the
released `rdma-p2p` binary. Cargo builds the embedded frontend directly from
`apps/frontend`; Node and npm are needed to compile, never to run the binary.
