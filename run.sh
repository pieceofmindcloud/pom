./target/release/rdma-p2p --model gemma3 --memory 1GB
./target/release/rdma-p2p --model gemma3 --memory 1GB --start-layer 10 --port 42778 
./target/release/rdma-p2p --model llama3:8b --memory 10GB --gateway --gateway-port 8080