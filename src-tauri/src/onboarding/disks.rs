//! Disk discovery for the onboarding flow.
//!
//! Lists **all** block devices on the host — including unmounted/unformatted
//! ones — so the user can pick a disk before initialization. Per-platform
//! implementations shell out to OS tools because `sysinfo` only enumerates
//! mounted volumes.

use serde::{Deserialize, Serialize};

use super::{OnboardingError, OnboardingResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    /// Stable per-host id used by `format_disk`. Disk-number on Windows,
    /// `/dev/diskN` on macOS, `/dev/sdX` on Linux.
    pub id: String,
    pub name: String,
    pub size_bytes: u64,
    pub filesystem: Option<String>,
    pub volume_label: Option<String>,
    pub is_system: bool,
    pub is_removable: bool,
    /// First mounted partition path (`E:\`, `/Volumes/BRAIN`, `/mnt/brain`).
    /// `None` for unformatted/unmounted disks.
    pub mount_path: Option<String>,
}

pub fn list_disks() -> OnboardingResult<Vec<DiskInfo>> {
    #[cfg(target_os = "windows")]
    {
        return windows::list();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::list();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::list();
    }
    #[allow(unreachable_code)]
    {
        Ok(Vec::new())
    }
}

fn run_command(program: &str, args: &[&str]) -> OnboardingResult<String> {
    let output = crate::proc::no_window(program)
        .args(args)
        .output()
        .map_err(OnboardingError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(OnboardingError::DiskNotFound(format!(
            "{program} failed: {stderr}"
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;

    const PS_LIST_DISKS: &str = r#"
$ErrorActionPreference = 'Stop'
$disks = Get-Disk | ForEach-Object {
  $disk = $_
  $partitions = @()
  try { $partitions = @(Get-Partition -DiskNumber $_.Number -ErrorAction Stop) } catch { $partitions = @() }
  $volumes = @()
  foreach ($p in $partitions) {
    try {
      $v = Get-Volume -Partition $p -ErrorAction Stop
      if ($v) { $volumes += $v }
    } catch {}
  }
  [PSCustomObject]@{
    DiskNumber   = [int]$disk.Number
    FriendlyName = "$($disk.FriendlyName)"
    BusType      = "$($disk.BusType)"
    Size         = [long]$disk.Size
    IsSystem     = [bool]$disk.IsSystem
    IsBoot       = [bool]$disk.IsBoot
    Volumes      = @($volumes | ForEach-Object {
      [PSCustomObject]@{
        DriveLetter     = if ($_.DriveLetter) { "$($_.DriveLetter):\" } else { '' }
        FileSystem      = "$($_.FileSystem)"
        FileSystemLabel = "$($_.FileSystemLabel)"
        Size            = [long]$_.Size
      }
    })
  }
}
ConvertTo-Json -Depth 5 -Compress -InputObject @($disks)
"#;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct PsDisk {
        disk_number: i32,
        friendly_name: String,
        bus_type: String,
        size: i64,
        is_system: bool,
        is_boot: bool,
        volumes: Vec<PsVolume>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct PsVolume {
        drive_letter: String,
        file_system: String,
        file_system_label: String,
        #[allow(dead_code)]
        size: i64,
    }

    pub fn list() -> OnboardingResult<Vec<DiskInfo>> {
        let raw = run_command(
            "powershell",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                PS_LIST_DISKS,
            ],
        )?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: Vec<PsDisk> = serde_json::from_str(trimmed)
            .or_else(|_| serde_json::from_str::<PsDisk>(trimmed).map(|d| vec![d]))
            .map_err(|err| OnboardingError::DiskNotFound(format!("powershell json: {err}")))?;
        Ok(parsed
            .into_iter()
            .map(|d| {
                let primary_volume = d.volumes.first();
                let mount_path = primary_volume
                    .filter(|v| !v.drive_letter.is_empty() && v.drive_letter != ":\\")
                    .map(|v| v.drive_letter.clone());
                let filesystem = primary_volume
                    .filter(|v| !v.file_system.is_empty())
                    .map(|v| v.file_system.clone());
                let volume_label = primary_volume
                    .filter(|v| !v.file_system_label.is_empty())
                    .map(|v| v.file_system_label.clone());
                let is_removable = matches!(
                    d.bus_type.to_uppercase().as_str(),
                    "USB" | "SD" | "MMC" | "1394"
                );
                DiskInfo {
                    id: d.disk_number.to_string(),
                    name: if d.friendly_name.is_empty() {
                        format!("Disk {}", d.disk_number)
                    } else {
                        d.friendly_name.clone()
                    },
                    size_bytes: d.size.max(0) as u64,
                    filesystem,
                    volume_label,
                    is_system: d.is_system || d.is_boot,
                    is_removable,
                    mount_path,
                }
            })
            .collect())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub fn list() -> OnboardingResult<Vec<DiskInfo>> {
        let raw = run_command("diskutil", &["list", "-plist", "external", "physical"])?;
        // Fall back to a parser that tolerates either physical-only or a full list.
        parse_diskutil_plist(&raw).or_else(|_| {
            let raw_all = run_command("diskutil", &["list", "-plist"])?;
            parse_diskutil_plist(&raw_all)
        })
    }

    fn parse_diskutil_plist(plist: &str) -> OnboardingResult<Vec<DiskInfo>> {
        // Minimal plist parser tailored to diskutil's output structure.
        // We pick out <key>AllDisksAndPartitions</key> -> array of dicts.
        let mut result = Vec::new();
        let mut i = 0;
        let bytes = plist.as_bytes();
        let needle = b"<key>AllDisksAndPartitions</key>";
        let start = match plist.find(std::str::from_utf8(needle).unwrap()) {
            Some(p) => p + needle.len(),
            None => 0,
        };
        i = start;
        while let Some(disk_pos) = plist[i..].find("<dict>") {
            let abs = i + disk_pos;
            let id = extract_string(&plist[abs..], "DeviceIdentifier").unwrap_or_default();
            let size = extract_int(&plist[abs..], "Size").unwrap_or(0);
            let content = extract_string(&plist[abs..], "Content").unwrap_or_default();
            let label = extract_string(&plist[abs..], "VolumeName");
            let is_internal = extract_bool(&plist[abs..], "Internal").unwrap_or(false);
            let mount = extract_string(&plist[abs..], "MountPoint");
            i = abs + 6;
            let _ = bytes;
            if id.is_empty() {
                continue;
            }
            result.push(DiskInfo {
                id: format!("/dev/{id}"),
                name: if content.is_empty() {
                    id.clone()
                } else {
                    format!("{id} ({content})")
                },
                size_bytes: size as u64,
                filesystem: if content.is_empty() { None } else { Some(content) },
                volume_label: label,
                is_system: is_internal,
                is_removable: !is_internal,
                mount_path: mount.filter(|s| !s.is_empty()),
            });
        }
        Ok(result)
    }

    fn extract_string(slice: &str, key: &str) -> Option<String> {
        let needle = format!("<key>{key}</key>");
        let start = slice.find(&needle)? + needle.len();
        let s = &slice[start..];
        let open = s.find("<string>")? + "<string>".len();
        let close = s[open..].find("</string>")?;
        Some(s[open..open + close].to_string())
    }

    fn extract_int(slice: &str, key: &str) -> Option<i64> {
        let needle = format!("<key>{key}</key>");
        let start = slice.find(&needle)? + needle.len();
        let s = &slice[start..];
        let open = s.find("<integer>")? + "<integer>".len();
        let close = s[open..].find("</integer>")?;
        s[open..open + close].trim().parse::<i64>().ok()
    }

    fn extract_bool(slice: &str, key: &str) -> Option<bool> {
        let needle = format!("<key>{key}</key>");
        let start = slice.find(&needle)? + needle.len();
        let s = slice[start..].trim_start();
        if s.starts_with("<true/>") {
            Some(true)
        } else if s.starts_with("<false/>") {
            Some(false)
        } else {
            None
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct LsblkRoot {
        blockdevices: Vec<LsblkBlock>,
    }

    #[derive(Debug, Deserialize)]
    struct LsblkBlock {
        name: String,
        #[serde(default)]
        size: Option<serde_json::Value>,
        #[serde(rename = "tran", default)]
        tran: Option<String>,
        #[serde(default)]
        rm: Option<bool>,
        #[serde(default)]
        fstype: Option<String>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        mountpoint: Option<String>,
        #[serde(default, rename = "type")]
        kind: Option<String>,
        #[serde(default)]
        children: Option<Vec<LsblkBlock>>,
    }

    pub fn list() -> OnboardingResult<Vec<DiskInfo>> {
        let raw = run_command(
            "lsblk",
            &["-J", "-b", "-o", "NAME,SIZE,TYPE,TRAN,RM,FSTYPE,LABEL,MOUNTPOINT"],
        )?;
        let parsed: LsblkRoot = serde_json::from_str(&raw)
            .map_err(|err| OnboardingError::DiskNotFound(format!("lsblk json: {err}")))?;
        let mut out = Vec::new();
        for dev in parsed.blockdevices {
            if dev.kind.as_deref() != Some("disk") {
                continue;
            }
            let size = dev
                .size
                .as_ref()
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok())))
                .unwrap_or(0);
            let removable = dev.rm.unwrap_or(false)
                || matches!(dev.tran.as_deref(), Some("usb") | Some("mmc"));
            let primary_child = dev
                .children
                .as_ref()
                .and_then(|cs| cs.iter().find(|c| c.fstype.is_some()))
                .or_else(|| dev.children.as_ref().and_then(|cs| cs.first()));
            let filesystem = primary_child.and_then(|c| c.fstype.clone()).or(dev.fstype);
            let label = primary_child.and_then(|c| c.label.clone()).or(dev.label);
            let mount = primary_child
                .and_then(|c| c.mountpoint.clone())
                .or(dev.mountpoint)
                .filter(|s| !s.is_empty());
            let is_system = mount.as_deref() == Some("/")
                || mount.as_deref().map(|m| m.starts_with("/boot")).unwrap_or(false);
            out.push(DiskInfo {
                id: format!("/dev/{}", dev.name),
                name: format!("/dev/{}", dev.name),
                size_bytes: size,
                filesystem,
                volume_label: label,
                is_system,
                is_removable: removable && !is_system,
                mount_path: mount,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_disks_does_not_panic_on_a_real_host() {
        // CI may have no removable media or no PowerShell/diskutil/lsblk;
        // we tolerate either Ok or Err and just assert no panic.
        let _ = list_disks();
    }

    #[test]
    fn disk_info_serializes_into_camel_case_compatible_keys_for_frontend() {
        let d = DiskInfo {
            id: "1".into(),
            name: "Test".into(),
            size_bytes: 1024,
            filesystem: Some("exFAT".into()),
            volume_label: Some("BRAIN".into()),
            is_system: false,
            is_removable: true,
            mount_path: Some("E:\\".into()),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"size_bytes\":1024"));
        assert!(json.contains("\"volume_label\":\"BRAIN\""));
        assert!(json.contains("\"mount_path\":\"E:\\\\\""));
    }
}
