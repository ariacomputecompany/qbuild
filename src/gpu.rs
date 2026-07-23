use crate::protocol::{BindMountSpec, GpuRequest};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct GpuRuntimeSpec {
    pub mounts: Vec<BindMountSpec>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalGpuDevice {
    id: String,
    vendor: GpuVendor,
    index: usize,
    device_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuVendor {
    Nvidia,
    Amd,
}

pub fn prepare_gpu_runtime(request: &GpuRequest) -> Result<GpuRuntimeSpec, String> {
    request.validate()?;
    if !request.is_enabled() {
        return Ok(GpuRuntimeSpec {
            mounts: Vec::new(),
            env: HashMap::new(),
        });
    }

    let selected = select_devices(discover_local_gpus(), request)?;
    let vendor = selected
        .first()
        .map(|device| device.vendor)
        .ok_or_else(|| "gpu_count requested but no local GPU devices are available".to_string())?;

    if selected.iter().any(|device| device.vendor != vendor) {
        return Err("a single qbuild container cannot mix NVIDIA and AMD GPU devices".to_string());
    }

    match vendor {
        GpuVendor::Nvidia => prepare_nvidia_runtime(&selected),
        GpuVendor::Amd => prepare_amd_runtime(&selected),
    }
}

fn discover_local_gpus() -> Vec<LocalGpuDevice> {
    let mut devices = discover_nvidia_devices();
    devices.extend(discover_amd_devices());
    devices
}

fn discover_nvidia_devices() -> Vec<LocalGpuDevice> {
    let mut devices = Vec::new();
    let Ok(entries) = fs::read_dir("/dev") else {
        return devices;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(index) = name
            .strip_prefix("nvidia")
            .and_then(|value| value.parse::<usize>().ok())
        else {
            continue;
        };
        devices.push(LocalGpuDevice {
            id: format!("nvidia{}", index),
            vendor: GpuVendor::Nvidia,
            index,
            device_path: entry.path(),
        });
    }
    devices.sort_by_key(|device| device.index);
    devices
}

fn discover_amd_devices() -> Vec<LocalGpuDevice> {
    if Path::new("/dev/dxg").exists() {
        return vec![LocalGpuDevice {
            id: "amd0".to_string(),
            vendor: GpuVendor::Amd,
            index: 0,
            device_path: PathBuf::from("/dev/dxg"),
        }];
    }

    let mut devices = Vec::new();
    let dri = Path::new("/dev/dri");
    let Ok(entries) = fs::read_dir(dri) else {
        return devices;
    };
    let mut render_nodes = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let index = name.strip_prefix("renderD")?.parse::<usize>().ok()?;
            Some((index, entry.path()))
        })
        .collect::<Vec<_>>();
    render_nodes.sort_by_key(|(index, _)| *index);
    for (ordinal, (_render_index, path)) in render_nodes.into_iter().enumerate() {
        devices.push(LocalGpuDevice {
            id: format!("amd{}", ordinal),
            vendor: GpuVendor::Amd,
            index: ordinal,
            device_path: path,
        });
    }
    devices
}

fn select_devices(
    devices: Vec<LocalGpuDevice>,
    request: &GpuRequest,
) -> Result<Vec<LocalGpuDevice>, String> {
    if !request.device_ids.is_empty() {
        let mut selected = Vec::with_capacity(request.device_ids.len());
        for requested in &request.device_ids {
            let normalized = normalize_gpu_id(requested);
            let Some(device) = devices.iter().find(|device| {
                device.id == normalized
                    || device.id == *requested
                    || device.device_path.to_string_lossy() == requested.as_str()
                    || device.index.to_string() == requested.as_str()
            }) else {
                return Err(format!(
                    "requested gpu_id '{}' is not available on this host",
                    requested
                ));
            };
            selected.push(device.clone());
        }
        return Ok(selected);
    }

    let count = request.count as usize;
    let nvidia = devices
        .iter()
        .filter(|device| device.vendor == GpuVendor::Nvidia)
        .take(count)
        .cloned()
        .collect::<Vec<_>>();
    if nvidia.len() == count {
        return Ok(nvidia);
    }
    let amd = devices
        .iter()
        .filter(|device| device.vendor == GpuVendor::Amd)
        .take(count)
        .cloned()
        .collect::<Vec<_>>();
    if amd.len() == count {
        return Ok(amd);
    }

    Err(format!(
        "requested {} GPU(s), but only {} compatible local GPU device(s) are available",
        count,
        devices.len()
    ))
}

fn normalize_gpu_id(value: &str) -> String {
    if let Some(name) = value.strip_prefix("/dev/") {
        return normalize_gpu_id(name);
    }
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        return format!("nvidia{}", value);
    }
    value.to_string()
}

fn prepare_nvidia_runtime(devices: &[LocalGpuDevice]) -> Result<GpuRuntimeSpec, String> {
    let mut mounts = Vec::new();
    for path in trusted_existing_paths([
        "/dev/nvidiactl",
        "/dev/nvidia-uvm",
        "/dev/nvidia-uvm-tools",
        "/dev/nvidia-modeset",
    ]) {
        push_mount(&mut mounts, &path, false);
    }
    for device in devices {
        push_mount(&mut mounts, &device.device_path, false);
    }

    let visible = devices
        .iter()
        .map(|device| device.index.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut env = HashMap::new();
    env.insert("CUDA_VISIBLE_DEVICES".to_string(), visible.clone());
    env.insert("NVIDIA_VISIBLE_DEVICES".to_string(), visible);
    env.insert(
        "NVIDIA_DRIVER_CAPABILITIES".to_string(),
        "compute,utility".to_string(),
    );

    Ok(GpuRuntimeSpec { mounts, env })
}

fn prepare_amd_runtime(devices: &[LocalGpuDevice]) -> Result<GpuRuntimeSpec, String> {
    let mut mounts = Vec::new();
    for path in trusted_existing_paths(["/dev/kfd", "/dev/dxg"]) {
        push_mount(&mut mounts, &path, false);
    }
    if Path::new("/dev/dri").exists() {
        push_mount(&mut mounts, Path::new("/dev/dri"), false);
    } else {
        for device in devices {
            push_mount(&mut mounts, &device.device_path, false);
        }
    }
    for path in trusted_existing_paths(["/usr/lib/wsl/lib"]) {
        push_mount(&mut mounts, &path, true);
    }

    let visible = devices
        .iter()
        .map(|device| device.index.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut env = HashMap::new();
    env.insert("HIP_VISIBLE_DEVICES".to_string(), visible.clone());
    env.insert("ROCR_VISIBLE_DEVICES".to_string(), visible);
    if Path::new("/usr/lib/wsl/lib").exists() {
        env.insert(
            "LD_LIBRARY_PATH".to_string(),
            "/usr/lib/wsl/lib".to_string(),
        );
    }

    Ok(GpuRuntimeSpec { mounts, env })
}

fn trusted_existing_paths<const N: usize>(paths: [&str; N]) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect()
}

fn push_mount(mounts: &mut Vec<BindMountSpec>, path: &Path, readonly: bool) {
    let value = path.to_string_lossy().to_string();
    if !mounts
        .iter()
        .any(|mount| mount.source == value && mount.target == value)
    {
        mounts.push(BindMountSpec {
            source: value.clone(),
            target: value,
            readonly,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_numeric_gpu_ids_to_nvidia_ids() {
        assert_eq!(normalize_gpu_id("0"), "nvidia0");
        assert_eq!(normalize_gpu_id("/dev/nvidia1"), "nvidia1");
        assert_eq!(normalize_gpu_id("amd0"), "amd0");
    }

    #[test]
    fn select_devices_preserves_explicit_gpu_vendors_for_runtime_validation() {
        let devices = vec![
            LocalGpuDevice {
                id: "nvidia0".to_string(),
                vendor: GpuVendor::Nvidia,
                index: 0,
                device_path: PathBuf::from("/dev/nvidia0"),
            },
            LocalGpuDevice {
                id: "amd0".to_string(),
                vendor: GpuVendor::Amd,
                index: 0,
                device_path: PathBuf::from("/dev/dri/renderD128"),
            },
        ];
        let selected = select_devices(
            devices,
            &GpuRequest {
                count: 2,
                device_ids: vec!["nvidia0".to_string(), "amd0".to_string()],
            },
        )
        .unwrap();
        assert!(
            selected
                .iter()
                .any(|device| device.vendor == GpuVendor::Nvidia)
        );
        assert!(
            selected
                .iter()
                .any(|device| device.vendor == GpuVendor::Amd)
        );
    }
}
