# Python inference legacy

This directory preserves the previous Python inference and GGUF utility
scripts for operators who still need them. The supported runtime path is the
Rust inference server in `src/infer_server.rs` and the Rust splitter in
`src/splitter.rs`.

The scripts are intentionally not imported or launched by the node binary.
`run-inf.sh` remains as an explicit compatibility launcher and invokes
`old/python-inference/inference_server.py`.

Legacy scripts:

- `inference_server.py`
- `split_model.py`
- `split.py`
- `inspect_model.py`
- `inspect_gguf.py`
