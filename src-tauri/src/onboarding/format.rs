//! Per-platform exFAT formatting with `BRAIN` volume label.
//!
//! All three implementations require elevated privileges. We elevate via
//! the OS-native mechanism — UAC `runas` on Windows, `osascript` admin
//! prompt on macOS, `pkexec` on Linux — so the user sees the standard
//! authentication dialog.
//!
//! The format step waits for the new volume to mount and returns its
//! mount path, so onboarding can continue with the freshly initialized
//! disk without an extra discovery round-trip.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{OnboardingError, OnboardingResult};

pub const BRAIN_LABEL: &str = "BRAIN";

#[derive(Debug)]
pub struct FormatResult {
    pub mount_path: PathBuf,
}

pub fn format_as_brain(disk_id: &str) -> OnboardingResult<FormatResult> {
    if disk_id.trim().is_empty() {
        return Err(OnboardingError::DiskNotFound("empty disk id".into()));
    }
    #[cfg(target_os = "windows")]
    {
        return windows::format(disk_id);
    }
    #[cfg(target_os = "macos")]
    {
        return macos::format(disk_id);
    }
    #[cfg(target_os = "linux")]
    {
        return linux::format(disk_id);
    }
    #[allow(unreachable_code)]
    {
        Err(OnboardingError::UnsupportedOnPlatform(
            "no format implementation for this platform",
        ))
    }
}

fn wait_for_label(label: &str, timeout: Duration) -> OnboardingResult<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(path) = locate_label(label)? {
            return Ok(path);
        }
        if Instant::now() >= deadline {
            return Err(OnboardingError::DiskNotFound(format!(
                "volume with label {label} did not appear within {} s",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn locate_label(label: &str) -> OnboardingResult<Option<PathBuf>> {
    for d in super::disks::list_disks()?.into_iter() {
        if d.volume_label.as_deref() == Some(label) {
            if let Some(mp) = d.mount_path {
                return Ok(Some(PathBuf::from(mp)));
            }
        }
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
mod windows {
    use super::*;

    pub fn format(disk_id: &str) -> OnboardingResult<FormatResult> {
        let n: u32 = disk_id
            .trim()
            .parse()
            .map_err(|_| OnboardingError::DiskNotFound(format!("invalid disk id: {disk_id}")))?;

        // Inner script that does the actual work (runs elevated).
        let inner = format!(
            r#"$ErrorActionPreference = 'Stop'
$N = {n}
$d = Get-Disk -Number $N
if ($d.IsSystem -or $d.IsBoot) {{ Write-Error 'refuse-system-disk'; exit 2 }}
try {{ Get-Partition -DiskNumber $N -ErrorAction Stop | Remove-Partition -Confirm:$false -ErrorAction SilentlyContinue }} catch {{}}
try {{ Clear-Disk -Number $N -RemoveData -RemoveOEM -Confirm:$false -ErrorAction Stop }} catch {{}}
Initialize-Disk -Number $N -PartitionStyle MBR -ErrorAction Stop | Out-Null
$p = New-Partition -DiskNumber $N -UseMaximumSize -AssignDriveLetter -ErrorAction Stop
Format-Volume -Partition $p -FileSystem exFAT -NewFileSystemLabel '{label}' -Confirm:$false -Force -ErrorAction Stop | Out-Null
Set-Volume -DriveLetter $p.DriveLetter -NewFileSystemLabel '{label}' -ErrorAction SilentlyContinue
"#,
            n = n,
            label = super::BRAIN_LABEL
        );

        // Outer script: re-spawn elevated and wait.
        //
        // The OUTER PowerShell runs invisibly (CREATE_NO_WINDOW below), but
        // `Start-Process -Verb RunAs` launches a *new* elevated PowerShell
        // process that the OS controls — UAC consent is required and
        // visible (cannot be suppressed). After consent, we want the
        // elevated PowerShell window itself hidden so the user only sees
        // the UAC prompt + our progress UI, not a flashing console.
        //
        // `-WindowStyle Hidden` on the inner Start-Process hides the
        // elevated child's window. The format work runs to completion
        // headlessly; output is not user-facing — errors propagate via
        // exit code (handled by `if (!status.success())` below).
        let outer = format!(
            r#"$ErrorActionPreference = 'Stop'
$inner = @'
{inner}
'@
$tmp = [System.IO.Path]::GetTempFileName() + '.ps1'
[System.IO.File]::WriteAllText($tmp, $inner, [System.Text.Encoding]::UTF8)
$p = Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-WindowStyle','Hidden','-File',$tmp) -Verb RunAs -WindowStyle Hidden -Wait -PassThru
Remove-Item $tmp -Force -ErrorAction SilentlyContinue
exit $p.ExitCode
"#,
            inner = inner
        );

        let status = crate::proc::no_window("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &outer,
            ])
            .status()
            .map_err(OnboardingError::Io)?;

        if !status.success() {
            return Err(OnboardingError::DiskNotFound(format!(
                "format failed (powershell exit code {:?})",
                status.code()
            )));
        }

        let mount_path = wait_for_label(super::BRAIN_LABEL, Duration::from_secs(30))?;
        Ok(FormatResult { mount_path })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;

    pub fn format(disk_id: &str) -> OnboardingResult<FormatResult> {
        let dev = disk_id.trim();
        if !dev.starts_with("/dev/") {
            return Err(OnboardingError::DiskNotFound(format!(
                "macOS expects /dev/diskN, got: {dev}"
            )));
        }
        let script = format!(
            "do shell script \"diskutil eraseDisk ExFAT {label} {dev}\" with administrator privileges",
            label = super::BRAIN_LABEL,
            dev = dev
        );
        let status = crate::proc::no_window("osascript")
            .args(["-e", &script])
            .status()
            .map_err(OnboardingError::Io)?;
        if !status.success() {
            return Err(OnboardingError::DiskNotFound(format!(
                "diskutil eraseDisk failed (exit {:?})",
                status.code()
            )));
        }
        let mount_path = wait_for_label(super::BRAIN_LABEL, Duration::from_secs(20))?;
        Ok(FormatResult { mount_path })
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    pub fn format(disk_id: &str) -> OnboardingResult<FormatResult> {
        let dev = disk_id.trim();
        if !dev.starts_with("/dev/") {
            return Err(OnboardingError::DiskNotFound(format!(
                "linux expects /dev/sdX or /dev/nvmeXnY, got: {dev}"
            )));
        }
        let part = format!("{dev}1");
        let script = format!(
            "set -e; \
             wipefs -af {dev}; \
             parted -s {dev} mklabel msdos; \
             parted -s {dev} mkpart primary 1MiB 100%; \
             udevadm settle; \
             mkfs.exfat -n {label} {part};",
            dev = dev,
            part = part,
            label = super::BRAIN_LABEL
        );
        let status = crate::proc::no_window("pkexec")
            .args(["sh", "-c", &script])
            .status()
            .map_err(OnboardingError::Io)?;
        if !status.success() {
            return Err(OnboardingError::DiskNotFound(format!(
                "pkexec format failed (exit {:?})",
                status.code()
            )));
        }
        let mount_path = wait_for_label(super::BRAIN_LABEL, Duration::from_secs(20))?;
        Ok(FormatResult { mount_path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_rejects_empty_disk_id_with_disk_not_found_error() {
        let err = format_as_brain("").unwrap_err();
        assert!(matches!(err, OnboardingError::DiskNotFound(_)));
    }

    #[test]
    fn brain_label_constant_is_uppercase_brain() {
        assert_eq!(BRAIN_LABEL, "BRAIN");
    }
}
