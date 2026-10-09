/// The plan route reading the real inventory of real peers.
///
/// The `plan` tests build `Machine` by hand, which proves the decision but does
/// not prove where it reads from. Here the answer comes over HTTP, from the
/// same `/api/ui/models` each machine serves, and that is why "the whole model
/// shows up" and "the slice of the same name does not take its place" are
/// facts and not assumptions.
#[cfg(test)]
mod whole_route_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::{get as axum_get, post as axum_post};

    /// A machine serving the given catalog, with memory and disk to spare.
    async fn catalog_machine(catalog: serde_json::Value) -> u16 {
        let metadata_catalog = catalog.clone();
        let app = Router::new()
            .route(
                "/api/ui/models",
                axum_get(move || {
                    let catalog = catalog.clone();
                    async move { Json(catalog) }
                }),
            )
            .route(
                "/api/ui/node",
                axum_get(|| async {
                    Json(json!({
                        "system": {
                            "ram": { "available_mb": 64_000.0 },
                            "disk": { "free_mb": 900_000.0 },
                        }
                    }))
                }),
            )
            .route(
                "/api/ui/model-metadata",
                axum_post(move |Json(request): Json<serde_json::Value>| {
                    let catalog = metadata_catalog.clone();
                    async move {
                        let path = request.get("path").and_then(|value| value.as_str());
                        let mut model = catalog
                            .get("models")
                            .and_then(|value| value.as_array())
                            .into_iter()
                            .flatten()
                            .find(|model| {
                                model.get("path").and_then(|value| value.as_str()) == path
                            })
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        if model
                            .get("total_layers")
                            .is_some_and(serde_json::Value::is_null)
                        {
                            model["total_layers"] = json!(32);
                        }
                        Json(model)
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// The catalog of a machine that has the whole GGUF **and** slices of
    /// another model, which is the normal state of a cluster machine.
    fn catalog() -> serde_json::Value {
        json!({
            "hostname": "ksia",
            "models": [
                {
                    "id": "1",
                    "name": "qwen3.5-9b",
                    "source": "lmstudio",
                    "source_label": "LM Studio",
                    "path": "/lm/qwen3.5-9b.gguf",
                    "size_bytes": 5_368_709_120u64,
                    "architecture": "qwen3",
                    "total_layers": 32,
                    "quant": "Q4_K_M",
                    "shard_range": serde_json::Value::Null,
                    "shard_group": serde_json::Value::Null,
                    "capabilities": {
                        "thinking": true,
                        "mtp": false,
                    },
                },
                {
                    "id": "2",
                    "name": "qwen_0_15.gguf",
                    "source": "folder",
                    "source_label": "Pasta",
                    "path": "/models/qwen/qwen_0_15.gguf",
                    "size_bytes": 2_000_000_000u64,
                    "architecture": "qwen3",
                    "total_layers": 32,
                    "quant": serde_json::Value::Null,
                    "shard_range": [0, 15],
                    "shard_group": "qwen",
                }
            ],
            "shard_sets": [{
                "group": "qwen",
                "total_layers": 32,
                "total_layers_inferred": false,
                "complete": false,
                "shards": [{
                    "name": "qwen_0_15.gguf",
                    "path": "/models/qwen/qwen_0_15.gguf",
                    "size_bytes": 2_000_000_000u64,
                    "total_layers": 32,
                    "shard_range": [0, 15],
                }],
            }],
            "scanned": [],
            "metadata_complete": true,
        })
    }

    /// A state whose only inventory is the fake machine.
    ///
    /// `all_peers` always includes this machine, and the testkit's `gateway_port`
    /// is 8080 - the same one a real node listens on in the captain's machine.
    /// Without pointing it at a dead port, the sweep leaves the suite and reads
    /// the real cluster's catalog: the test passes or fails depending on what is
    /// on the disk of whoever runs it.
    async fn state_seeing_only(port: u16) -> GatewayState {
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let mut state = state_with(
            "eu",
            vec![crate::gateway::testkit::peer("ksia", "ksia", port)],
        )
        .await;
        state.gateway_port = dead;
        state
    }

    async fn plan_body(state: GatewayState, req: serde_json::Value) -> serde_json::Value {
        body_of(
            plan_endpoint(
                ConnectionAuth,
                State(state),
                Json(serde_json::from_value(req).expect("DeployRequest")),
            )
            .await,
        )
        .await
    }

    async fn plan_for(kind: &str, group: &str) -> serde_json::Value {
        let port = catalog_machine(catalog()).await;
        let state = state_seeing_only(port).await;
        plan_body(state, json!({ "group": group, "kind": kind })).await
    }

    /// The whole path: the route asks the machine, finds the whole GGUF and
    /// returns a one-step plan covering the entire model.
    #[tokio::test]
    async fn the_route_plans_a_whole_model_from_the_machines_own_catalog() {
        let body = plan_for("whole", "qwen3.5-9b").await;

        assert_eq!(body["kind"], "whole", "the plan says which question it answered");
        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
        assert_eq!(body["complete"], true);
        assert_eq!(body["total_layers"], 32);

        let steps = body["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["start_layer"], 0);
        assert_eq!(steps[0]["end_layer"], 31);
        assert_eq!(steps[0]["target_node_id"], "ksia");
        assert_eq!(steps[0]["ready"], true);
        assert_eq!(steps[0]["primary"], true);

        // The slice on the same machine did not enter this question's inventory.
        let available = body["available"].as_array().unwrap();
        assert_eq!(available.len(), 1, "only the whole model: {available:?}");
        assert_eq!(available[0]["name"], "qwen3.5-9b");
    }

    /// Reproduces the listing observed on Salem with the two projectors beside
    /// the models. Exercises the planning HTTP route through the local testkit,
    /// without credentials, Docker or sending a deployment to another peer.
    #[tokio::test]
    async fn salem_ornith_plan_keeps_its_own_projector_beside_qwen() {
        let mut catalog = catalog();
        catalog["models"] = json!([
            {
                "id": "ornith",
                "name": "Ornith 1.5",
                "source": "folder",
                "source_label": "Pasta",
                "path": "/data/source-models/Ornith-1.5-Q4_K_M.gguf",
                "mmproj_path": "/data/source-models/mmproj-Ornith-1.5-F16.gguf",
                "size_bytes": 5_368_709_120u64,
                "architecture": "llama",
                "total_layers": 32,
                "quant": "Q4_K_M",
                "shard_range": null,
                "shard_group": null,
            },
            {
                "id": "qwen",
                "name": "Qwen3.8 Flash Next",
                "source": "folder",
                "source_label": "Pasta",
                "path": "/data/source-models/Qwen3.8-Flash-Next-Q4_K_M.gguf",
                "mmproj_path": "/data/source-models/mmproj-Qwen3.8-Flash-Next-Q4_K_M.gguf",
                "size_bytes": 5_368_709_120u64,
                "architecture": "qwen3",
                "total_layers": 32,
                "quant": "Q4_K_M",
                "shard_range": null,
                "shard_group": null,
            }
        ]);
        let port = catalog_machine(catalog).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "Ornith 1.5",
                "kind": "whole",
                "selection": [{
                    "start_layer": 0,
                    "end_layer": 31,
                    "target_node_id": "ksia"
                }]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
        assert_eq!(
            body["mmproj_path"],
            "/data/source-models/mmproj-Ornith-1.5-F16.gguf"
        );
        assert_ne!(
            body["mmproj_path"],
            "/data/source-models/mmproj-Qwen3.8-Flash-Next-Q4_K_M.gguf"
        );
    }

    #[tokio::test]
    async fn the_plan_ignores_unrelated_catalog_enrichment() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen3.5-9b",
                "kind": "whole",
                "selection": [{ "start_layer": 0, "end_layer": 31, "target_node_id": "ksia" }]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
    }

    #[tokio::test]
    async fn the_plan_reads_selected_metadata_without_waiting_for_neighbors() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        pending["models"][0]["total_layers"] = serde_json::Value::Null;
        assert_eq!(
            selected_model_metadata_path(&pending, "qwen3.5-9b", DeployKind::Whole).as_deref(),
            Some("/lm/qwen3.5-9b.gguf")
        );
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen3.5-9b",
                "kind": "whole",
                "selection": [{ "start_layer": 0, "end_layer": 31, "target_node_id": "ksia" }]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
        assert_eq!(body["total_layers"], 32);
    }

    #[tokio::test]
    async fn the_plan_does_not_wait_for_an_old_peer_without_catalog_state() {
        let mut legacy = catalog();
        legacy.as_object_mut().unwrap().remove("metadata_complete");
        let port = catalog_machine(legacy).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen3.5-9b",
                "kind": "whole",
                "selection": [{ "start_layer": 0, "end_layer": 31, "target_node_id": "ksia" }]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
    }

    #[tokio::test]
    async fn a_complete_selected_shard_set_is_ready_during_other_enrichment() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        pending["shard_sets"][0]["complete"] = json!(true);
        pending["shard_sets"][0]["shards"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "name": "qwen_16_31.gguf",
                "path": "/models/qwen/qwen_16_31.gguf",
                "size_bytes": 2_000_000_000u64,
                "total_layers": 32,
                "shard_range": [16, 31],
            }));
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen",
                "kind": "shards",
                "selection": [
                    { "start_layer": 0, "end_layer": 15, "target_node_id": "ksia" },
                    { "start_layer": 16, "end_layer": 31, "target_node_id": "ksia" }
                ]
            }),
        )
        .await;

        assert!(body["blocked"].is_null(), "{}", body["blocked"]);
    }

    #[tokio::test]
    async fn an_unresolved_selected_shard_names_the_missing_range() {
        let mut pending = catalog();
        pending["metadata_complete"] = json!(false);
        pending["shard_sets"][0]["complete"] = json!(true);
        let port = catalog_machine(pending).await;
        let body = plan_body(
            state_seeing_only(port).await,
            json!({
                "group": "qwen",
                "kind": "shards",
                "selection": [
                    { "start_layer": 0, "end_layer": 15 },
                    { "start_layer": 16, "end_layer": 31 }
                ]
            }),
        )
        .await;

        let blocked = body["blocked"].as_str().unwrap_or_default();
        assert!(blocked.contains("L16-31"), "{blocked}");
        assert!(blocked.contains("GGUF metadata"), "{blocked}");
    }

    /// The default question is still the one about slices, and it does not see
    /// the whole model - both lists come out of the same sweep, and swapping
    /// them would make an old `plan` change its answer.
    #[tokio::test]
    async fn the_slice_question_is_untouched_and_still_the_default() {
        let body = plan_for("shards", "qwen").await;
        let available = body["available"].as_array().unwrap();
        assert_eq!(available.len(), 1, "{available:?}");
        assert_eq!(available[0]["name"], "qwen_0_15.gguf");
        assert_eq!(available[0]["end_layer"], 15);
        assert_eq!(body["kind"], "shards");

        // With no `kind` at all, the same result: `shards` is the default.
        let port = catalog_machine(catalog()).await;
        let without_kind = plan_body(state_seeing_only(port).await, json!({ "group": "qwen" })).await;
        assert_eq!(without_kind["kind"], "shards");
        assert_eq!(without_kind["available"].as_array().unwrap().len(), 1);
    }

    /// A model no machine has is a refusal naming the model, not an empty list
    /// that reads as "everything is fine, but it cannot be done".
    #[tokio::test]
    async fn a_model_nobody_has_is_refused_by_name() {
        let body = plan_for("whole", "llama3-70b").await;
        let reason = body["blocked"].as_str().expect("refusal");
        assert!(reason.contains("llama3-70b"), "{reason}");
        assert!(!reason.contains("slices of"), "{reason}");
    }
}
