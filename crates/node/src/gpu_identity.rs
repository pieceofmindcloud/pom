//! Which physical GPUs this node sees. Several nodes of one machine
//! (containers) may share a card; the GPU arbiter (`gpu_arbiter.rs`) groups
//! them by the card's identity: the NVIDIA UUID, or the PCI slot of an AMD
//! card. A node that reads no identity is never arbitrated.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GpuId {
    /// The index this node's processes see the card at.
    pub index: u32,
    pub id: String,
}

/// `nvidia-smi --query-gpu=index,uuid --format=csv,noheader` output.
pub(crate) fn parse_nvidia(csv: &str) -> Vec<GpuId> {
    csv.lines()
        .filter_map(|line| {
            let (index, uuid) = line.split_once(',')?;
            let uuid = uuid.trim();
            let index = index.trim().parse().ok()?;
            (!uuid.is_empty()).then(|| GpuId {
                index,
                id: uuid.to_owned(),
            })
        })
        .collect()
}

/// AMD cards from `/sys/class/drm/card*/device`: `(vendor, uevent)` per card,
/// in card order; only vendor `0x1002`, identified by `PCI_SLOT_NAME`.
pub(crate) fn parse_amd(cards: &[(String, String)]) -> Vec<GpuId> {
    cards
        .iter()
        .filter(|(vendor, _)| vendor.trim() == "0x1002")
        .filter_map(|(_, uevent)| {
            uevent
                .lines()
                .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                .map(|slot| slot.trim().to_owned())
        })
        .enumerate()
        .map(|(index, slot)| GpuId {
            index: index as u32,
            id: format!("pci:{slot}"),
        })
        .collect()
}

async fn probe() -> Vec<GpuId> {
    let mut found = Vec::new();
    if let Ok(output) = tokio::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,uuid", "--format=csv,noheader"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    {
        if output.status.success() {
            found.extend(parse_nvidia(&String::from_utf8_lossy(&output.stdout)));
        }
    }
    // AMD cards a ROCm process sees (`/dev/kfd`); a container with every DRI
    // card but no KFD runs nothing on AMD.
    if std::path::Path::new("/dev/kfd").exists() {
        let mut cards = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/sys/class/drm") {
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("card") && n[4..].bytes().all(|b| b.is_ascii_digit()))
                .collect();
            names.sort_by_key(|n| n[4..].parse::<u32>().unwrap_or(u32::MAX));
            for name in names {
                let device = std::path::Path::new("/sys/class/drm")
                    .join(name)
                    .join("device");
                let vendor = std::fs::read_to_string(device.join("vendor")).unwrap_or_default();
                let uevent = std::fs::read_to_string(device.join("uevent")).unwrap_or_default();
                cards.push((vendor, uevent));
            }
        }
        let offset = found.len() as u32;
        // A PCI slot repeats across machines: the kernel's boot id, the same
        // in every container of one host, makes it this host's card.
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
        found.extend(parse_amd(&cards).into_iter().map(|mut g| {
            g.index += offset;
            g.id = format!("{}@{}", g.id, boot.trim());
            g
        }));
    }
    found
}

/// This node's GPUs, read once (cards do not change while a node runs).
pub(crate) async fn gpu_ids() -> Vec<GpuId> {
    static IDS: tokio::sync::OnceCell<Vec<GpuId>> = tokio::sync::OnceCell::const_new();
    IDS.get_or_init(probe).await.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_uuids_are_read() {
        let ids = parse_nvidia("0, GPU-aaaa\n1, GPU-bbbb\n\n");
        assert_eq!(
            ids,
            vec![
                GpuId {
                    index: 0,
                    id: "GPU-aaaa".into()
                },
                GpuId {
                    index: 1,
                    id: "GPU-bbbb".into()
                },
            ]
        );
    }

    #[test]
    fn amd_cards_are_read_by_pci_slot() {
        let cards = vec![
            (
                "0x10de\n".to_owned(),
                "PCI_SLOT_NAME=0000:09:00.0\n".to_owned(),
            ),
            (
                "0x1002\n".to_owned(),
                "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:06:00.0\n".to_owned(),
            ),
        ];
        assert_eq!(
            parse_amd(&cards),
            vec![GpuId {
                index: 0,
                id: "pci:0000:06:00.0".into()
            }]
        );
    }

    #[test]
    fn other_vendors_are_skipped() {
        assert!(parse_amd(&[("0x8086".into(), "PCI_SLOT_NAME=0000:00:02.0".into())]).is_empty());
        assert!(parse_nvidia("garbage").is_empty());
    }

    #[test]
    fn yield_defaults_to_off() {
        let params: crate::webui::RuntimeParams =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!params.yield_gpu_to_images);
    }
}
