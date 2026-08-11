import { describe, expect, it } from "vitest";
import { graphFromCluster } from "./networkGraph";

describe("graphFromCluster", () => {
  it("keeps overlapping model ranges instead of collapsing them into one pipeline", () => {
    const graph = graphFromCluster({
      self_id: "no1", active_model: "qwen35", total_layers: 7, covered_layers: 7,
      coverage: [], complete: true, routing_edges: [], node: null,
      peers: [
        { node_id:"no1", short_id:"no1", addr:"", model:"qwen35", start_layer:0,end_layer:2,total_layers:7,inference_url:"",gateway_url:"",load:1,is_self:true,node:null,unreachable:null,capabilities:[{model_id:"qwen35",start_layer:0,end_layer:2,supports_inference:true},{model_id:"ornith",start_layer:0,end_layer:2,supports_inference:true}] },
        { node_id:"no2", short_id:"no2", addr:"", model:"qwen35", start_layer:2,end_layer:6,total_layers:7,inference_url:"",gateway_url:"",load:2,is_self:false,node:null,unreachable:null,capabilities:[{model_id:"qwen35",start_layer:2,end_layer:6,supports_inference:true}] },
      ],
    });
    expect(graph.models).toEqual(["ornith", "qwen35"]);
    expect(graph.byModel.qwen35).toHaveLength(2);
    expect(graph.byModel.ornith).toHaveLength(1);
  });
});
