use anyhow::{anyhow, Result};
use log::info;
use nix::dir::Dir;
use nix::fcntl::{FallocateFlags, OFlag};
use nix::mount;
use nix::sys::reboot::{reboot, RebootMode};
use nix::sys::stat::Mode;
use nix::unistd::{chroot, fchdir, sync};
use std::fmt::Write as FmtWrite;
use std::io::{prelude::*, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::prelude::{OsStrExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::{fs::File, path::Path};
use sysinfo::{System, SystemExt};

use crate::disks::{fstab_entries, is_efi_booted, Partition};
use crate::network;
use crate::parser::{list_mounts, list_zoneinfo, locale_names};

const BIND_MOUNTS: &[&str] = &["/dev", "/proc", "/sys", "/run/udev"];
const BUNDLED_LOCALE_GEN: &[u8] = include_bytes!("../res/locale.gen");
const SYSTEM_LOCALE_GEN_PATH: &str = "/etc/locale.gen";
const SYSTEM_ZONEINFO1970_PATH: &str = "/usr/share/zoneinfo/zone1970.tab";
const BUNDLED_ZONEINFO_LIST: &[u8] = include_bytes!("../res/zone1970.tab");

fn running_info(s: &str) {
    info!("Running {}", s);
}

fn run_successfully_info(s: &str) {
    info!("Run {} Successfully!", s);
}

fn no_need_to_run_info(s: &str, str_is_retro: bool) {
    if str_is_retro {
        info!("Retro system no need to run {}", s);
    } else {
        info!("Non retro system no need to run {}", s);
    }
}

fn read_system_locale_list() -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(SYSTEM_LOCALE_GEN_PATH)?;
    let mut data: Vec<u8> = Vec::new();
    data.reserve(8800);
    f.read_to_end(&mut data)?;

    Ok(data)
}

/// Get the list of available locales
pub fn get_locale_list() -> Result<Vec<String>> {
    let data = read_system_locale_list().unwrap_or_else(|_| BUNDLED_LOCALE_GEN.to_vec());
    let names = locale_names(&data)
        .map_err(|_| anyhow!("Installer failed to gather available locales."))?;
    let names = names.1.into_iter().map(|x| x.to_string()).collect();

    Ok(names)
}

fn read_system_zoneinfo_list() -> Result<Vec<u8>> {
    let mut f = std::fs::File::open(SYSTEM_ZONEINFO1970_PATH)?;
    let mut data: Vec<u8> = Vec::new();
    data.reserve(8800);
    f.read_to_end(&mut data)?;

    Ok(data)
}

/// Get the list of available timezone
pub fn get_zoneinfo_list() -> Result<Vec<String>> {
    let data = read_system_zoneinfo_list().unwrap_or_else(|_| BUNDLED_ZONEINFO_LIST.to_vec());
    let mut zoneinfo_list = list_zoneinfo(&data)
        .map_err(|_| anyhow!("Installer failed to gather available timezones."))?
        .1;

    if zoneinfo_list.is_empty() {
        return Err(anyhow!(
            "Installer could not parse the zoneinfo database! Empty timezone data (tzdata)?"
        ));
    }

    zoneinfo_list.sort();
    zoneinfo_list.insert(0, "UTC".to_string());

    Ok(zoneinfo_list)
}

/// Extract the given .tar.xz stream and preserve all the file attributes
pub fn extract_tar_xz<R: Read>(reader: R, path: &Path) -> Result<()> {
    let decompress = xz2::read::XzDecoder::new(reader);
    let mut tar_processor = tar::Archive::new(decompress);
    tar_processor.set_unpack_xattrs(true);
    tar_processor.set_preserve_permissions(true);
    tar_processor.set_preserve_ownerships(true);
    tar_processor.unpack(path)?;

    Ok(())
}

/// Mount the filesystem to a temporary directory
pub fn auto_mount_root_path(tmp_path: &Path, partition: &Partition) -> Result<PathBuf> {
    mount_root_path(partition, tmp_path)?;

    Ok(tmp_path.to_path_buf())
}

/// Sync the filesystem and then reboot IMMEDIATELY (ignores init)
pub fn sync_and_reboot() -> Result<()> {
    sync();
    reboot(RebootMode::RB_AUTOBOOT)?;

    Ok(())
}

/// Mount the filesystem
pub fn mount_root_path(partition: &Partition, target: &Path) -> Result<()> {
    if partition.fs_type.is_none() || partition.path.is_none() {
        return Err(anyhow!(
            "Installer failed to determine user-specified partition."
        ));
    }
    let source = partition.path.as_ref();
    let mut fs_type = partition.fs_type.as_ref().unwrap().as_str();
    if fs_type.starts_with("fat") {
        fs_type = "vfat";
    }
    // FIXME: due to an issue in `nix` and `libc`, `MS_LAZYTIME` is not supported atm
    mount::mount(
        source,
        target,
        Some(fs_type),
        mount::MsFlags::empty(),
        None::<&str>,
    )?;

    Ok(())
}

/// Gen fstab to /etc/fstab
pub fn genfstab_to_file(partition: &Partition, root_path: &Path, mount_path: &Path) -> Result<()> {
    if cfg!(debug_assertions) {
        return Ok(());
    }
    let fs_type = partition.fs_type.as_ref().ok_or_else(|| {
        anyhow!("Installer failed to detect filesystem type for the specified partition.")
    })?;
    let s = fstab_entries(partition.path.as_ref(), fs_type, Some(mount_path))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .append(true)
        .open(root_path.join("etc/fstab"))?;
    f.write_all(s.as_bytes())?;

    Ok(())
}

/// Unmount the filesystem given at `root` and then do a sync
pub fn umount_root_path(root: &Path) -> Result<()> {
    mount::umount2(root, mount::MntFlags::MNT_DETACH)?;
    sync();

    Ok(())
}

/// Get the open file descriptor to the specified path
pub fn get_dir_fd<P: nix::NixPath>(path: P) -> Result<Dir> {
    let fd = Dir::open(
        &path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NONBLOCK,
        Mode::empty(),
    )?;

    Ok(fd)
}

/// Escape the chroot context using the previously obtained `root_fd` as a trampoline
pub fn escape_chroot(root_fd: i32) -> Result<()> {
    fchdir(root_fd)?;
    chroot(".")?;
    std::env::set_current_dir("/")?; // reset cwd (on host)

    info!("Escaped chroot environment");

    Ok(())
}

/// Setup all the necessary bind mounts
pub fn setup_bind_mounts(root: &Path) -> Result<()> {
    for mount in BIND_MOUNTS {
        let mut root = root.to_owned();
        root.push(&mount[1..]);
        std::fs::create_dir_all(root.clone())?;
        mount::mount(
            Some(*mount),
            &root,
            None::<&str>,
            mount::MsFlags::MS_BIND,
            None::<&str>,
        )?;
    }

    Ok(())
}

/// Remove bind mounts
/// Note: This function should be called outside of the chroot context
pub fn remove_bind_mounts(root: &Path) -> Result<()> {
    for mount in BIND_MOUNTS {
        let mut root = root.to_owned();
        root.push(&mount[1..]);
        mount::umount2(&root, mount::MntFlags::MNT_DETACH)?;
    }

    Ok(())
}

/// Setup bind mounts and chroot into the guest system
/// Warning: This will make the program trapped in the new root directory
pub fn dive_into_guest(root: &Path) -> Result<()> {
    setup_bind_mounts(root)?;
    chroot(root)?;
    std::env::set_current_dir("/")?; // jump to the root directory after chroot

    Ok(())
}

/// Runs dracut
/// Must be used in a chroot context
#[cfg(not(feature = "is_retro"))]
pub fn execute_dracut() -> Result<()> {
    let cmd = "/usr/bin/update-initramfs";
    running_info(cmd);
    let output = Command::new(cmd).output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Installer failed to execute dracut: \n{}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ));
    }

    run_successfully_info(cmd);

    Ok(())
}

/// Runs dracut (dummy function for retro mode)
/// Must be used in a chroot context
#[cfg(feature = "is_retro")]
pub fn execute_dracut() -> Result<()> {
    no_need_to_run_info("dracut", true);

    Ok(())
}

/// Runs ssh-keygen -A (dummy function for non-retro mode)
/// Must be used in a chroot context
#[cfg(not(feature = "is_retro"))]
pub fn gen_ssh_key() -> Result<()> {
    no_need_to_run_info("ssh-keygen", true);

    Ok(())
}

/// Runs ssh-keygen -A
/// Must be used in a chroot context
#[cfg(feature = "is_retro")]
pub fn gen_ssh_key() -> Result<()> {
    running_info("ssh-keygen");
    let output = Command::new("ssh-keygen").arg("-A").output()?;

    if !output.status.success() {
        return Err(anyhow!(
            "Installer failed to generate SSH host keys: \n\n{}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ));
    }

    run_successfully_info("ssh-keygen");

    Ok(())
}

/// Sets hostname in the guest environment
/// Must be used in a chroot context
pub fn set_hostname(name: &str) -> Result<()> {
    let mut f = File::create("/etc/hostname")?;

    Ok(f.write_all(name.as_bytes())?)
}

/// Sets locale in the guest environment
/// Must be used in a chroot context
pub fn set_locale(locale: &str) -> Result<()> {
    let mut f = File::create("/etc/locale.conf")?;
    f.write_all(b"LANG=")?;

    Ok(f.write_all(locale.as_bytes())?)
}

/// Sets zoneinfo in the guest environment
/// Must be used in a chroot context
pub fn set_zoneinfo(zone: &str) -> Result<()> {
    if Path::new("/etc/localtime").exists() {
        std::fs::remove_file("/etc/localtime")?;
    }

    std::os::unix::fs::symlink(format!("/usr/share/zoneinfo/{}", zone), "/etc/localtime")?;

    Ok(())
}

/// Sets utc/rtc time in the guest environment
/// Must be used in a chroot context
pub fn set_hwclock_tc(utc: bool) -> Result<()> {
    let adjtime_file = std::fs::File::open("/etc/adjtime");
    let status_is_rtc = if let Ok(mut adjtime_file) = adjtime_file {
        let mut buf = String::new();
        adjtime_file.read_to_string(&mut buf)?;
        let line: Vec<&str> = buf.split('\n').collect();
        if line.len() < 3 || line.get(2) == Some(&"UTC") {
            false
        } else {
            line[2] == "LOCAL"
        }
    } else {
        false
    };

    info!("Status is rtc: {}", status_is_rtc);
    if utc {
        if !status_is_rtc {
            return Ok(());
        } else {
            running_info("hwclock -wu");
            let command = Command::new("hwclock").arg("-wu").output()?;
            if !command.status.success() {
                return Err(anyhow!(
                    "Installer failed to set local time as UTC: {}",
                    String::from_utf8_lossy(&command.stderr)
                ));
            }
            run_successfully_info("hwclock -wu");
        }
    } else if status_is_rtc {
        return Ok(());
    } else {
        running_info("hwclock -wl");
        let command = Command::new("hwclock").arg("-wl").output()?;
        if !command.status.success() {
            return Err(anyhow!(
                "Installer failed to set local time as RTC: {}",
                String::from_utf8_lossy(&command.stderr)
            ));
        }

        run_successfully_info("hwclock -wl");
    }

    Ok(())
}

/// Adds a new normal user to the guest environment
/// Must be used in a chroot context
pub fn add_new_user(name: &str, password: &str) -> Result<()> {
    running_info(&format!("useradd -m -s /bin/bash {}", name));
    let command = Command::new("useradd")
        .args(&["-m", "-s", "/bin/bash", name])
        .output()?;
    if !command.status.success() {
        return Err(anyhow!(
            "Installer failed to create the default user: {}",
            String::from_utf8_lossy(&command.stderr)
        ));
    }
    run_successfully_info(&format!("useradd -m -s /bin/bash {}", name));

    running_info(&format!(
        "usermod -aG audio,cdrom,video,wheel,plugdev {}",
        name
    ));
    let command = Command::new("usermod")
        .args(&["-aG", "audio,cdrom,video,wheel,plugdev", name])
        .output()?;
    if !command.status.success() {
        return Err(anyhow!(
            "Installer failed to configure the default user: {}",
            String::from_utf8_lossy(&command.stderr)
        ));
    }

    running_info("chpasswd");
    let command = Command::new("chpasswd").stdin(Stdio::piped()).spawn()?;

    let mut stdin = command.stdin.ok_or_else(|| {
        anyhow!("Installer can not get your stdin! please restart your environment")
    })?;

    stdin.write_all(format!("{}:{}\n", name, password).as_bytes())?;
    stdin.flush()?;

    run_successfully_info("chpasswd");

    Ok(())
}

/// Runs grub-install and grub-mkconfig
/// Must be used in a chroot context
pub fn execute_grub_install(mbr_dev: Option<&PathBuf>) -> Result<()> {
    let mut command = Command::new("grub-install");
    let mut s = String::new();
    let cmd = if let Some(mbr_dev) = mbr_dev {
        write!(s, "grub-install --target=i386-pc {}", mbr_dev.display())?;

        command.arg("--target=i386-pc").arg(mbr_dev)
    } else {
        let (target, is_efi) = match network::get_arch_name() {
            Some("amd64") => ("--target=x86_64-efi", true),
            Some("arm64") => ("--target=arm64-efi", true),
            Some("ppc64el") | Some("ppc64") | Some("powerpc") => {
                ("--target=powerpc-ieee1275", false)
            }
            Some("riscv64") => ("--target=riscv64-efi", true),
            _ => {
                info!("This architecture does not support grub");
                return Ok(());
            }
        };
        let efi = if is_efi { "--efi-directory=/efi" } else { "" };
        write!(s, "grub-install {} --bootloader-id=AOSC OS {}", target, efi)?;

        command.arg(target).arg("--bootloader-id=AOSC OS").arg(efi)
    };

    running_info(&s);
    let process = cmd.output()?;
    if !process.status.success() {
        return Err(anyhow!(
            "Installer failed to install GRUB: {}",
            String::from_utf8_lossy(&process.stderr)
        ));
    }
    run_successfully_info(&s);

    running_info("grub-mkconfig -o /boot/grub/grub.cfg");
    let process = Command::new("grub-mkconfig")
        .arg("-o")
        .arg("/boot/grub/grub.cfg")
        .output()?;
    if !process.status.success() {
        return Err(anyhow!(
            "Installer failed to generate GRUB menu: {}",
            String::from_utf8_lossy(&process.stderr)
        ));
    }

    run_successfully_info("grub-mkconfig -o /boot/grub/grub.cfg");

    Ok(())
}

pub fn prepare_try_umount() -> Result<()> {
    let mut mounts = std::fs::File::open("/proc/mounts")?;
    let mut buf = Vec::new();
    mounts.read_to_end(&mut buf)?;

    let mounts = list_mounts(&buf)
        .map_err(|e| anyhow!("Failed to get mounts, {}", e))?
        .1;

    let dk_mounts = mounts
        .iter()
        .filter(|(_, mount_path)| mount_path.starts_with("/tmp/.tmp"));

    for (_, mount_path) in dk_mounts {
        umount_root_path(Path::new(mount_path)).ok();
    }

    Ok(())
}

pub fn log_system_info() {
    let sys = System::new_all();

    info!("Deploykit version: {}", env!("CARGO_PKG_VERSION"));
    info!(
        "OS: {:?}",
        sys.name()
            .and_then(|s| sys.os_version().and_then(|y| Some(format!("{} {}", s, y))))
    );
    info!("Kernel: {:?}", sys.kernel_version());
    info!("CPU: {:?}", sys.cpus());
    info!("Memory: {:?}, Usage: {:?}", sys.total_memory(), sys.used_memory());
}

/// Create swapfile
pub fn create_swapfile(size: f64, use_swap: bool, tempdir: &Path) -> Result<()> {
    if !use_swap {
        return Ok(());
    }

    let swap_path = tempdir.join("swapfile");

    info!("Creating swapfile");
    let mut swapfile = std::fs::File::create(&swap_path)?;
    nix::fcntl::fallocate(
        swapfile.as_raw_fd(),
        FallocateFlags::empty(),
        0,
        (size as i32).into(),
    )?;
    swapfile.flush()?;

    info!("Set swapfile permission as 600");
    std::fs::set_permissions(&swap_path, std::fs::Permissions::from_mode(0o600))?;

    running_info(&format!("mkswap {}", swap_path.display()));
    let mkswap = Command::new("mkswap").arg(&swap_path).output()?;
    if !mkswap.status.success() {
        return Err(anyhow!(
            "Installer failed to create swap signature on swapfile: {}\n{}",
            String::from_utf8_lossy(&mkswap.stderr),
            String::from_utf8_lossy(&mkswap.stdout)
        ));
    }
    run_successfully_info(&format!("mkswap {}", swap_path.display()));

    running_info(&format!("swapon {}", swap_path.display()));
    Command::new("swapon").arg(swap_path).output().ok();

    Ok(())
}

pub fn swapoff(tempdir: &Path) -> Result<()> {
    running_info(&format!("swaponff {}/swapfile", tempdir.display()));
    Command::new("swapoff")
        .arg(tempdir.join("swapfile"))
        .output()?;
    run_successfully_info(&format!("swaponff {}/swapfile", tempdir.display()));

    Ok(())
}

/// Must be used in a chroot context
pub fn write_swap_entry_to_fstab() -> Result<()> {
    let s = "/swapfile none swap defaults,nofail 0 0\n";
    let mut fstab = std::fs::OpenOptions::new()
        .append(true)
        .open("/etc/fstab")?;
    fstab.write_all(s.as_bytes())?;

    Ok(())
}

pub fn disable_hibernate() -> Result<()> {
    let path = "/etc/systemd/system/hibernate.target";
    if Path::new(path).exists() {
        std::fs::remove_file(path)?;
    }
    std::os::unix::fs::symlink("/dev/null", path)?;

    Ok(())
}

/// Run umount -R
pub fn umount_all(mount_path: &Path, root_fd: i32) {
    info!("Cleaning up mount path ...");

    escape_chroot(root_fd).ok();
    let efi_path = mount_path.join("efi");
    if is_efi_booted() {
        umount_root_path(&efi_path).ok();
    }
    swapoff(mount_path).ok();
    umount_root_path(mount_path).ok();
}

pub fn is_valid_hostname(hostname: &str) -> bool {
    if hostname.is_empty() || hostname.starts_with('-') {
        return false;
    }
    for c in hostname.as_bytes() {
        if c.is_ascii_alphanumeric() || *c == b'-' {
            continue;
        } else {
            return false;
        }
    }

    true
}

pub fn is_acceptable_username(username: &str) -> bool {
    if username.is_empty() || username.starts_with('-') {
        return false;
    }

    if username == "root" {
        return false;
    }

    for c in username.as_bytes() {
        if c.is_ascii_whitespace()
            || !c.is_ascii_lowercase()
            || *c == b'/'
            || *c == b'\\'
            || *c == b':'
        {
            return false;
        }
    }

    true
}

#[test]
fn test_hostname_validation() {
    assert_eq!(is_valid_hostname("foo"), true);
    assert_eq!(is_valid_hostname("foo-2e10"), true);
    assert_eq!(is_valid_hostname("jeffbai-device"), true);
    assert_eq!(is_valid_hostname("invalid_host"), false);
    assert_eq!(is_valid_hostname("-invalid"), false);
    assert_eq!(is_valid_hostname("+invalid"), false);
    assert_eq!(is_valid_hostname("JellyDimension"), true);
    assert_eq!(is_valid_hostname("Jelly_Dimension"), false);
}

#[test]
fn test_username_validation() {
    assert_eq!(is_acceptable_username("foo"), true);
    assert_eq!(is_acceptable_username("老白"), true);
    assert_eq!(is_acceptable_username("BAIMINGCONG"), false);
    assert_eq!(is_acceptable_username("root"), false);
    assert_eq!(is_acceptable_username("/root"), false);
    assert_eq!(is_acceptable_username("root:root"), false);
    assert_eq!(is_acceptable_username("root\n"), false);
    assert_eq!(is_acceptable_username("root\t"), false);
    assert_eq!(is_acceptable_username("ro ot"), false);
}

#[test]
fn test_path_strip() {
    for mount in BIND_MOUNTS {
        println!("{}", &mount[1..]);
    }
}
