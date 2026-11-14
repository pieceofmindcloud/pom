//! The record of an image deployment: its stages (text encoder, diffusion,
//! VAE, in that order), the machines that serve each stage (replicas) and the
//! versions the deployment has run. Kept in the deploying node's `pom.db`
//! setting `image_deployments`; a record from before the stages (one machine
//! per component, `components`) is read and turned into version 1.

use serde::{Deserialize, Serialize};

const DEPLOYMENTS_KEY: &str = "image_deployments";

/// The stages of an image model, in the order the data flows through them.
pub(crate) const STAGES: [&str; 3] = ["text_encoder", "diffusion", "vae"];

/// One component on one machine, the record's form before the stages. Still
/// written (the first replica of each stage) so an older node lists the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ComponentChoice {
    pub component: String,
    pub node_id: String,
    pub path: String,
}

/// The identity of a file across machines: its name and size, as the
/// sidecars use. No content hash in the scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileIdentity {
    pub name: String,
    #[serde(default)]
    pub size_bytes: u64,
}

/// A LoRA applied to the diffusion model, by file identity and strength
/// (`LoraLoaderModelOnly`'s `strength_model`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LoraChoice {
    pub file: FileIdentity,
    #[serde(default = "full_strength")]
    pub strength: f32,
}

fn full_strength() -> f32 {
    1.0
}

/// One machine serving a stage, with the path of the file on its disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Replica {
    pub node_id: String,
    #[serde(default)]
    pub hostname: String,
    pub path: String,
    /// The stage's LoRAs on this machine's disk, in the stage's order. Absent
    /// in a record written before LoRAs (and on a stage without them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lora_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ImageStage {
    pub component: String,
    pub file: FileIdentity,
    pub replicas: Vec<Replica>,
    /// The LoRAs applied over the diffusion model (the diffusion stage only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loras: Vec<LoraChoice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ImageDeploymentVersion {
    pub version: u32,
    pub created_at: String,
    #[serde(default)]
    pub created_by: String,
    pub stages: Vec<ImageStage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ImageDeployment {
    pub deploy_id: String,
    pub name: String,
    pub created_at: String,
    #[serde(default)]
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// The active version, flattened: the form older nodes read.
    #[serde(default)]
    pub components: Vec<ComponentChoice>,
    #[serde(default)]
    pub versions: Vec<ImageDeploymentVersion>,
    #[serde(default = "one")]
    pub active_version: u32,
}

fn one() -> u32 {
    1
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

fn stage_rank(component: &str) -> usize {
    STAGES
        .iter()
        .position(|stage| *stage == component)
        .unwrap_or(STAGES.len())
}

/// Stages in flow order: text encoder, diffusion, VAE.
pub(crate) fn sort_stages(stages: &mut [ImageStage]) {
    stages.sort_by_key(|stage| stage_rank(&stage.component));
}

impl ImageDeployment {
    /// A new deployment whose version 1 is `stages`.
    pub(crate) fn new(
        deploy_id: String,
        name: String,
        by: &str,
        family: Option<String>,
        stages: Vec<ImageStage>,
    ) -> Self {
        let mut deployment = Self {
            deploy_id,
            name,
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: by.to_owned(),
            family,
            components: Vec::new(),
            versions: Vec::new(),
            active_version: 1,
        };
        deployment.push_version(stages, by);
        deployment
    }

    /// A record from before the stages becomes version 1, one replica per
    /// component.
    pub(crate) fn ensure_versions(&mut self) {
        if !self.versions.is_empty() {
            return;
        }
        let mut stages: Vec<ImageStage> = Vec::new();
        for choice in &self.components {
            let replica = Replica {
                node_id: choice.node_id.clone(),
                hostname: String::new(),
                path: choice.path.clone(),
                lora_paths: Vec::new(),
            };
            match stages.iter_mut().find(|s| s.component == choice.component) {
                Some(stage) => stage.replicas.push(replica),
                None => stages.push(ImageStage {
                    component: choice.component.clone(),
                    file: FileIdentity {
                        name: file_name(&choice.path),
                        size_bytes: 0,
                    },
                    replicas: vec![replica],
                    loras: Vec::new(),
                }),
            }
        }
        sort_stages(&mut stages);
        self.versions.push(ImageDeploymentVersion {
            version: 1,
            created_at: self.created_at.clone(),
            created_by: self.created_by.clone(),
            stages,
        });
        self.active_version = 1;
    }

    /// The live version (the last one when `active_version` names none).
    pub(crate) fn active(&self) -> &ImageDeploymentVersion {
        self.versions
            .iter()
            .find(|v| v.version == self.active_version)
            .or_else(|| self.versions.last())
            .expect("a record has at least one version after ensure_versions")
    }

    /// Appends `stages` as a new version and makes it the live one.
    pub(crate) fn push_version(&mut self, mut stages: Vec<ImageStage>, by: &str) -> u32 {
        sort_stages(&mut stages);
        let version = self.versions.iter().map(|v| v.version).max().unwrap_or(0) + 1;
        self.versions.push(ImageDeploymentVersion {
            version,
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: by.to_owned(),
            stages,
        });
        self.active_version = version;
        self.sync_components();
        version
    }

    /// Writes the active version's first replica of each stage into
    /// `components`, the form older nodes read.
    pub(crate) fn sync_components(&mut self) {
        if self.versions.is_empty() {
            return;
        }
        self.components = self
            .active()
            .stages
            .iter()
            .filter_map(|stage| {
                stage.replicas.first().map(|replica| ComponentChoice {
                    component: stage.component.clone(),
                    node_id: replica.node_id.clone(),
                    path: replica.path.clone(),
                })
            })
            .collect();
    }
}

pub(crate) fn load_image_deployments(db: &crate::db::PomDb) -> Vec<ImageDeployment> {
    let mut list: Vec<ImageDeployment> = db
        .local_setting_json(DEPLOYMENTS_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    for deployment in &mut list {
        deployment.ensure_versions();
    }
    list
}

pub(crate) fn save_image_deployments(db: &crate::db::PomDb, deployments: &[ImageDeployment]) {
    let mut list = deployments.to_vec();
    for deployment in &mut list {
        deployment.sync_components();
    }
    if let Err(error) = db.save_local_setting(DEPLOYMENTS_KEY, &list) {
        tracing::warn!(%error, "could not store the image deployments");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(component: &str, nodes: &[&str]) -> ImageStage {
        ImageStage {
            component: component.into(),
            file: FileIdentity {
                name: format!("{component}.safetensors"),
                size_bytes: 10,
            },
            replicas: nodes
                .iter()
                .map(|n| Replica {
                    node_id: (*n).into(),
                    hostname: (*n).into(),
                    path: format!("/m/{component}.safetensors"),
                    lora_paths: Vec::new(),
                })
                .collect(),
            loras: Vec::new(),
        }
    }

    #[test]
    fn an_old_record_becomes_version_one() {
        let raw = r#"[{"deploy_id":"img-aaaaaaaaaaaa","name":"z","created_at":"t",
          "components":[{"component":"vae","node_id":"n2","path":"/m/ae.safetensors"},
                        {"component":"text_encoder","node_id":"n1","path":"/m/qwen.safetensors"}]}]"#;
        let mut list: Vec<ImageDeployment> = serde_json::from_str(raw).unwrap();
        list[0].ensure_versions();
        let v = list[0].active();
        assert_eq!(v.version, 1);
        assert_eq!(
            v.stages
                .iter()
                .map(|s| s.component.as_str())
                .collect::<Vec<_>>(),
            ["text_encoder", "vae"]
        );
        assert_eq!(v.stages[1].replicas[0].node_id, "n2");
        assert_eq!(v.stages[1].file.name, "ae.safetensors");
    }

    #[test]
    fn components_mirror_the_first_replica_of_each_stage() {
        let deployment = ImageDeployment::new(
            "img-x".into(),
            "z".into(),
            "me",
            None,
            vec![stage("vae", &["n1"]), stage("diffusion", &["n2", "n3"])],
        );
        assert_eq!(deployment.components.len(), 2);
        assert_eq!(deployment.components[0].component, "diffusion");
        assert_eq!(deployment.components[0].node_id, "n2");
        assert_eq!(deployment.components[1].component, "vae");
    }

    #[test]
    fn a_new_version_becomes_active() {
        let mut deployment = ImageDeployment::new(
            "img-x".into(),
            "z".into(),
            "me",
            None,
            vec![stage("diffusion", &["n1"])],
        );
        let v = deployment.push_version(vec![stage("diffusion", &["n1", "n2"])], "me");
        assert_eq!(v, 2);
        assert_eq!(deployment.active_version, 2);
        assert_eq!(deployment.versions.len(), 2);
        assert_eq!(deployment.active().stages[0].replicas.len(), 2);
    }

    #[test]
    fn records_round_trip_through_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::PomDb::open(dir.path().join("pom.db")).unwrap();
        let deployment = ImageDeployment::new(
            "img-x".into(),
            "z".into(),
            "me",
            Some("z_image".into()),
            vec![stage("text_encoder", &["n1"]), stage("diffusion", &["n2"])],
        );
        save_image_deployments(&db, std::slice::from_ref(&deployment));
        assert_eq!(load_image_deployments(&db), vec![deployment]);
    }
}
