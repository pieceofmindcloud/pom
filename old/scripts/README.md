# Legacy shell scripts

These manual launch and request scripts predate the workspace layout. They are
kept for operator reference only and are not invoked by Cargo, the node binary,
or the release pipeline.

Use `cargo run --bin rdma-p2p -- <arguments>` during development, or the
released `rdma-p2p` binary. The supported release asset workflow remains
`scripts/cut-ui-bundle.sh` because it builds the embedded frontend bundle.
