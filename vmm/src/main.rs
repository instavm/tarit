//! vmm — a blazing-fast microVM manager built on rust-vmm.
//!
//! Designed for AI/RL sandbox workloads: sub-15ms cold boot, live snapshot,
//! clone fan-out, host-enforced egress, and a JSON control API over Unix sockets.
//!
//! # Quick Start
//!
//! ```sh
//! # Install the pinned release kernel, then boot a VM
//! vmm kernel install
//! vmm run --mem 256
//!
//! # Start the API server
//! vmm serve --socket /tmp/vmm.sock
//!
//! # Restore from a snapshot
//! vmm restore --snapshot /tmp/vmm-vm1.snap
//! ```
//!
//! # API Protocol
//!
//! The API server accepts length-prefixed JSON over a Unix socket:
//! `[4-byte BE length][JSON body]`. See `vmm_api::types::ApiRequest` for
//! the full request schema.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

mod kernel_install;

const DEFAULT_CPU_PERIOD_US: u64 = 100_000;

#[derive(Parser, Debug)]
#[command(
    name = "vmm",
    version,
    about = "Blazing-fast microVM manager for AI/RL sandboxes",
    long_about = "A minimal rust-vmm-based VMM with sub-15ms cold boot, live snapshot, \
                  clone fan-out, host-enforced egress, and a REST API over Unix sockets.\n\n\
                  Built for PaaS-scale isolated compute: boot 100+ VMs per host, snapshot \
                  once, clone N-from-1 in <10ms each."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// Enable verbose logging (use -vv for trace level).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// API socket path (global default for API commands).
    #[arg(
        long,
        default_value = "/run/vmm.sock",
        value_name = "PATH",
        global = true
    )]
    socket: String,
}

#[derive(Args, Debug, Clone, Default)]
struct ServeCgroupArgs {
    /// Cgroup v2 path for the served VM process.
    #[arg(long, value_name = "PATH")]
    cgroup: Option<String>,

    /// memory.max limit for the served VM process (bytes, or K/M/G/T suffix).
    #[arg(long, value_name = "BYTES", value_parser = parse_human_bytes, requires = "cgroup")]
    cgroup_memory_max: Option<u64>,

    /// cpu.max limit as QUOTA/PERIOD (microseconds) or millicpu, e.g. 1000m.
    #[arg(long, value_name = "QUOTA/PERIOD|MILLICPU", value_parser = parse_cgroup_cpu_max, requires = "cgroup")]
    cgroup_cpu_max: Option<String>,

    /// pids.max limit for the served VM process.
    #[arg(long, value_name = "N", value_parser = parse_positive_u64, requires = "cgroup")]
    cgroup_pids_max: Option<u64>,

    /// cpuset.cpus limit for the served VM process, e.g. 0-3 or 0,2.
    #[arg(long, value_name = "CPUS", requires = "cgroup")]
    cpuset: Option<String>,
}

impl ServeCgroupArgs {
    fn limits(&self) -> Option<vmm_jailer::cgroups::CgroupLimits> {
        let limits = vmm_jailer::cgroups::CgroupLimits {
            cpu_max: self.cgroup_cpu_max.clone(),
            cpuset_cpus: self.cpuset.clone(),
            memory_max: self.cgroup_memory_max,
            pids_max: self.cgroup_pids_max,
            ..Default::default()
        };
        (!limits.is_empty()).then_some(limits)
    }
}

fn parse_human_bytes(value: &str) -> std::result::Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("size must not be empty".into());
    }

    let suffix_start = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, suffix) = value.split_at(suffix_start);
    if number.is_empty() {
        return Err(format!(
            "invalid size {value:?}: missing numeric byte count"
        ));
    }
    let bytes = number
        .parse::<u64>()
        .map_err(|e| format!("invalid size {value:?}: {e}"))?;
    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1_u64,
        "k" | "kb" | "kib" => 1024_u64,
        "m" | "mb" | "mib" => 1024_u64.pow(2),
        "g" | "gb" | "gib" => 1024_u64.pow(3),
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        other => {
            return Err(format!(
                "invalid size suffix {other:?}; use bytes or K/M/G/T suffixes"
            ))
        }
    };
    bytes
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size {value:?} overflows u64"))
}

fn mib_to_bytes(mib: u64, label: &str) -> Result<u64> {
    anyhow::ensure!(mib > 0, "{label} must be greater than zero MiB");
    mib.checked_mul(1024)
        .and_then(|v| v.checked_mul(1024))
        .ok_or_else(|| anyhow::anyhow!("{label} size {mib} MiB overflows u64"))
}

fn parse_cgroup_cpu_max(value: &str) -> std::result::Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("cpu.max must not be empty".into());
    }
    if value.eq_ignore_ascii_case("max") {
        return Ok(format!("max {DEFAULT_CPU_PERIOD_US}"));
    }
    if let Some(millis) = value.strip_suffix('m') {
        let millis = parse_positive_u64(millis)?;
        let quota = (millis as u128)
            .checked_mul(DEFAULT_CPU_PERIOD_US as u128)
            .and_then(|v| v.checked_div(1000))
            .ok_or_else(|| format!("cpu millicpu value {value:?} overflows"))?;
        let quota = u64::try_from(quota)
            .map_err(|_| format!("cpu millicpu value {value:?} overflows u64"))?;
        return Ok(format!("{quota} {DEFAULT_CPU_PERIOD_US}"));
    }

    if let Some((quota, period)) = value.split_once('/') {
        return format_cpu_max(quota, period);
    }

    let mut parts = value.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(quota), Some(period), None) => format_cpu_max(quota, period),
        _ => Err(format!(
            "invalid cpu.max {value:?}; use QUOTA/PERIOD, \"QUOTA PERIOD\", or millicpu like 1000m"
        )),
    }
}

fn format_cpu_max(quota: &str, period: &str) -> std::result::Result<String, String> {
    let quota = quota.trim();
    let period = parse_positive_u64(period.trim())?;
    if quota.eq_ignore_ascii_case("max") {
        Ok(format!("max {period}"))
    } else {
        Ok(format!("{} {period}", parse_positive_u64(quota)?))
    }
}

fn parse_positive_u64(value: &str) -> std::result::Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|e| format!("invalid positive integer {value:?}: {e}"))?;
    if parsed == 0 {
        Err(format!("value {value:?} must be greater than zero"))
    } else {
        Ok(parsed)
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Boot a fresh VM from a kernel image.
    #[command(alias = "start")]
    Run {
        /// Kernel path. If omitted, use or offer to install the pinned vmlinux.
        #[arg(long, value_name = "PATH")]
        kernel: Option<String>,

        /// Kernel command line (overrides the default).
        #[arg(long, value_name = "CMDLINE")]
        cmdline: Option<String>,

        /// Path to an initramfs image (optional).
        #[arg(long, value_name = "PATH")]
        initramfs: Option<String>,

        /// Guest memory size in MiB.
        #[arg(long, default_value = "256", value_name = "MIB")]
        mem: u64,

        /// Number of vCPUs.
        #[arg(long, default_value = "1", value_name = "N")]
        vcpus: u8,

        /// Pre-existing rootfs image (ext4/raw). Mounted as /dev/vda read-only.
        /// Use this when the orchestrator has already prepared the rootfs
        /// (e.g. from an OCI image conversion pipeline). For OCI pull, use
        /// `vmm pull` first, then pass the resulting .ext4 here.
        #[arg(long, value_name = "PATH")]
        rootfs: Option<String>,

        /// Attach a data volume (repeatable). Format: path[:ro|rw].
        /// Use --rootfs for the boot disk; use --volume for additional disks.
        #[arg(long, value_name = "PATH[:ro|rw]")]
        volume: Vec<String>,

        /// Attach a private sparse CoW overlay for each --volume path.
        #[arg(long, value_name = "PATH", requires = "volume")]
        overlay: Vec<String>,

        /// Attach a virtio-net device (at most one). Format:
        /// `tap=<name>[,mac=aa:bb:cc:dd:ee:ff]`. The tap must exist on the
        /// host (created by orchestrator/jailer). Without this flag the
        /// guest gets no NIC.
        #[arg(long, value_name = "SPEC")]
        net: Option<String>,

        /// Enable full boot mode (IRQCHIP + PIT for timer interrupts).
        #[arg(long)]
        full_boot: bool,

        /// Enable jailer confinement (chroot + namespaces + privilege drop).
        #[arg(long, value_name = "CHROOT_DIR")]
        jail: Option<String>,

        /// UID to drop to when jailed.
        #[arg(long, default_value = "1000", value_name = "UID")]
        uid: u32,

        /// GID to drop to when jailed.
        #[arg(long, default_value = "1000", value_name = "GID")]
        gid: u32,
    },

    /// Create (boot) the VM inside a running `vmm serve` (via the API).
    ///
    /// Send this to a `vmm serve` socket to boot the single VM from flags,
    /// then drive it with `exec`, `status`, `snapshot`, and `stop`.
    Create {
        /// Kernel path. If omitted, use or offer to install the pinned vmlinux.
        #[arg(long, value_name = "PATH")]
        kernel: Option<String>,

        /// Kernel command line. Defaults to a fast-boot cmdline (plus
        /// `root=/dev/vda rw` when `--rootfs` is given).
        #[arg(long, value_name = "CMDLINE")]
        cmdline: Option<String>,

        /// Path to an initramfs image (optional).
        #[arg(long, value_name = "PATH")]
        initramfs: Option<String>,

        /// Guest memory size in MiB.
        #[arg(long, default_value = "256", value_name = "MIB")]
        mem: u64,

        /// Number of vCPUs.
        #[arg(long, default_value = "1", value_name = "N")]
        vcpus: u8,

        /// Rootfs image (ext4/raw), attached as /dev/vda.
        #[arg(long, value_name = "PATH")]
        rootfs: Option<String>,

        /// Attach a data volume (repeatable). Format: path[:ro|rw].
        #[arg(long, value_name = "PATH[:ro|rw]")]
        volume: Vec<String>,

        /// Attach a private sparse CoW overlay for each --volume path.
        #[arg(long, value_name = "PATH", requires = "volume")]
        overlay: Vec<String>,
    },

    /// Restore a VM from a snapshot file.
    #[command(alias = "load")]
    Restore {
        /// Path to the snapshot file.
        #[arg(long, value_name = "PATH")]
        snapshot: String,

        /// Enable jailer confinement (chroot + namespaces + privilege drop).
        #[arg(long, value_name = "CHROOT_DIR")]
        jail: Option<String>,

        /// UID to drop to when jailed.
        #[arg(long, default_value = "1000", value_name = "UID")]
        uid: u32,

        /// GID to drop to when jailed.
        #[arg(long, default_value = "1000", value_name = "GID")]
        gid: u32,
    },

    /// Start the API server on a Unix socket.
    #[command(alias = "server")]
    Serve {
        /// Enable jailer confinement (chroot + namespaces + privilege drop).
        #[arg(long, value_name = "CHROOT_DIR")]
        jail: Option<String>,

        /// UID to drop to when jailed.
        #[arg(long, default_value = "1000", value_name = "UID")]
        uid: u32,

        /// GID to drop to when jailed.
        #[arg(long, default_value = "1000", value_name = "GID")]
        gid: u32,

        /// Network namespace path to enter when jailed.
        #[arg(long, value_name = "PATH")]
        netns: Option<String>,

        #[command(flatten)]
        cgroup: ServeCgroupArgs,
    },

    /// Create a snapshot from the running VM (via the API).
    #[command(alias = "snap")]
    Snapshot {
        /// Create a diff (incremental) snapshot.
        #[arg(long)]
        diff: bool,
    },

    /// Execute a command in the guest VM (via the API).
    #[command(alias = "run-in")]
    Exec {
        /// Command to execute.
        #[arg(value_name = "COMMAND")]
        command: String,

        /// Timeout in milliseconds (0 = no timeout).
        #[arg(long, default_value = "5000", value_name = "MS")]
        timeout: u64,
    },

    /// Attach an interactive PTY in the guest (via the API).
    AttachPty {
        /// Shell path to exec in the guest (default: guest $SHELL, bash, or sh).
        #[arg(long, value_name = "SHELL")]
        shell: Option<String>,
    },

    /// Stop the running VM (via the API).
    #[command(alias = "kill")]
    Stop,

    /// Remove orphaned VMM scratch files from a directory.
    Gc {
        /// Directory to sweep for VMM scratch files.
        #[arg(long, default_value = "/tmp", value_name = "DIR")]
        dir: std::path::PathBuf,

        /// Minimum scratch-file age before removal.
        #[arg(long, default_value_t = 3600, value_name = "SECS")]
        max_age: u64,
    },

    /// Pause the running VM (via the API).
    Pause,

    /// Suspend the VM and release resident guest RAM (via the API).
    Suspend,

    /// Resume the paused VM (via the API).
    Resume,

    /// Print a health/info snapshot of the VM (via the API).
    #[command(alias = "info")]
    Status,

    /// Update egress policy on the running VM (via the API).
    #[command(alias = "egress")]
    UpdateEgress {
        /// Allowlist rules in "cidr:port/proto" or "cidr" format.
        #[arg(long, value_name = "RULE")]
        allow: Vec<String>,

        /// Allow existing connections to persist.
        #[arg(long)]
        allow_existing: bool,
    },

    /// Pull an OCI image and convert to a bootable disk image.
    #[command(alias = "oci-pull")]
    Pull {
        /// OCI image reference (e.g., docker://ubuntu:22.04).
        #[arg(value_name = "REF")]
        image: String,

        /// Output disk image path.
        #[arg(long, value_name = "PATH")]
        output: String,

        /// Disk image size in MiB.
        #[arg(long, default_value = "1024", value_name = "MIB")]
        size: u64,

        /// Auth file path (for private registries).
        #[arg(long, value_name = "PATH")]
        auth: Option<String>,

        /// Path to the compiled guest exec agent (guest/agent/vmm-agent). When
        /// given, it is injected as the image's init so an app image like
        /// node:20 (which has no init system) boots straight to the exec agent.
        #[arg(long, value_name = "PATH")]
        agent: Option<String>,
    },

    /// Install and verify Tarit's pinned guest kernel.
    Kernel {
        #[command(subcommand)]
        command: KernelCommand,
    },
}

#[derive(Subcommand, Debug)]
enum KernelCommand {
    /// Download the pinned vmlinux release artifact.
    Install {
        /// Install path. Defaults to TARIT_KERNEL or the versioned Tarit data directory.
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,

        /// Replace an existing file whose checksum does not match.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .format_timestamp_millis()
        .try_init();

    log::info!("vmm {} — {:?}", vmm_core::VERSION, cli.cmd);

    match cli.cmd {
        Cmd::Run {
            kernel,
            cmdline,
            initramfs,
            mem,
            vcpus,
            rootfs,
            volume,
            overlay,
            net,
            full_boot,
            jail,
            uid,
            gid,
        } => run(
            kernel_install::resolve(kernel)?,
            cmdline,
            initramfs,
            mem,
            vcpus,
            rootfs,
            volume,
            overlay,
            net,
            full_boot,
            jail,
            uid,
            gid,
        ),
        Cmd::Restore {
            snapshot,
            jail,
            uid,
            gid,
        } => restore(snapshot, jail, uid, gid),
        Cmd::Serve {
            jail,
            uid,
            gid,
            netns,
            cgroup,
        } => serve(&cli.socket, jail, uid, gid, netns, cgroup),
        Cmd::Snapshot { diff } => api_snapshot(&cli.socket, diff),
        Cmd::Create {
            kernel,
            cmdline,
            initramfs,
            mem,
            vcpus,
            rootfs,
            volume,
            overlay,
        } => cmd_create(
            &cli.socket,
            kernel_install::resolve(kernel)?,
            cmdline,
            initramfs,
            mem,
            vcpus,
            rootfs,
            volume,
            overlay,
        ),
        Cmd::Exec { command, timeout } => api_request(
            &cli.socket,
            &vmm_api::types::ApiRequest::Exec {
                command,
                timeout_ms: timeout,
            },
        ),
        Cmd::AttachPty { shell } => attach_pty(&cli.socket, shell),
        Cmd::Stop => api_request(&cli.socket, &vmm_api::types::ApiRequest::Stop),
        Cmd::Gc { dir, max_age } => gc(dir, max_age),
        Cmd::Pause => api_request(&cli.socket, &vmm_api::types::ApiRequest::Pause),
        Cmd::Suspend => api_request(&cli.socket, &vmm_api::types::ApiRequest::Suspend),
        Cmd::Resume => api_request(&cli.socket, &vmm_api::types::ApiRequest::Resume),
        Cmd::Status => api_request(&cli.socket, &vmm_api::types::ApiRequest::Status),
        Cmd::UpdateEgress {
            allow,
            allow_existing,
        } => api_request(
            &cli.socket,
            &vmm_api::types::ApiRequest::UpdateEgress {
                allowlist: allow,
                allow_existing,
            },
        ),
        Cmd::Pull {
            image,
            output,
            size,
            auth,
            agent,
        } => pull_oci(image, output, size, auth, agent),
        Cmd::Kernel { command } => match command {
            KernelCommand::Install { output, force } => {
                kernel_install::install(output, force).map(|_| ())
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    kernel: String,
    cmdline: Option<String>,
    initramfs: Option<String>,
    mem_mib: u64,
    vcpus: u8,
    rootfs: Option<String>,
    volume: Vec<String>,
    overlay: Vec<String>,
    net: Option<String>,
    _full_boot: bool,
    jail_dir: Option<String>,
    uid: u32,
    gid: u32,
) -> Result<()> {
    anyhow::ensure!(vcpus > 0, "vcpus must be greater than zero");
    let mem_size = mib_to_bytes(mem_mib, "guest memory")?;
    #[cfg(target_os = "linux")]
    let rlimit_as = mem_size
        .checked_add(128 * 1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("guest memory plus jail overhead overflows u64"))?;

    // Apply jailer confinement before booting, if requested.
    #[cfg(target_os = "linux")]
    if let Some(chroot_dir) = &jail_dir {
        let cfg = vmm_jailer::jailer::JailerConfig {
            chroot_dir: chroot_dir.clone(),
            uid,
            gid,
            cgroup: String::new(),
            cgroup_limits: Some(vmm_jailer::cgroups::CgroupLimits {
                memory_max: Some(mem_size),
                pids_max: Some(64),
                ..Default::default()
            }),
            rlimit_nofile: 4096,
            rlimit_as,
            netns: String::new(),
        };
        vmm_jailer::jail(&cfg).map_err(|e| anyhow::anyhow!("jail: {e}"))?;
        log::info!("jailer confinement applied: chroot={chroot_dir} uid={uid} gid={gid}");
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (jail_dir, uid, gid);
    }

    let volumes = build_volume_configs(rootfs.as_deref(), &volume, &overlay)?;
    if let Some(rfs) = &rootfs {
        log::info!("rootfs: {rfs} (attached as /dev/vda, read-write)");
    }

    // Build the kernel cmdline, appending virtio_mmio.device entries for
    // each volume + the optional net device. The kernel needs these to
    // discover the MMIO devices.
    let mut cmdline = cmdline.unwrap_or_else(vmm_loader::default_cmdline);
    let mut next_mmio = 0xd000_0000u64;
    let mut next_irq: u32 = 5;
    for _ in volumes.iter() {
        let entry = format!(" virtio_mmio.device=4K@0x{next_mmio:x}:{next_irq}");
        cmdline.push_str(&entry);
        next_mmio += 0x1000;
        next_irq += 1;
    }
    if net.is_some() {
        let entry = format!(" virtio_mmio.device=4K@0x{next_mmio:x}:{next_irq}");
        cmdline.push_str(&entry);
    }
    log::info!("run: kernel={kernel} mem={mem_mib}MiB vcpus={vcpus} rootfs={rootfs:?} volumes={volumes:?} net={net:?} cmdline={cmdline:?}");
    #[allow(unused_variables)]
    let mem = vmm_memory_backend::GuestMemory::new(mem_size)
        .map_err(|e| anyhow::anyhow!("memory: {e}"))?;
    log::info!("guest memory: {mem_size} bytes at GPA 0");

    #[cfg(target_arch = "x86_64")]
    {
        let loaded = vmm_loader::load(
            &mem.inner,
            &kernel,
            &cmdline,
            initramfs.as_ref(),
            mem.size_bytes,
        )?;
        log::info!(
            "loaded: entry=0x{:x} kernel_end=0x{:x} zero_page=0x{:x} cmdline=0x{:x} initramfs={:?}",
            loaded.entry,
            loaded.kernel_end,
            loaded.zero_page_addr,
            loaded.cmdline_addr,
            loaded.initramfs_addr.map(|a| format!("0x{:x}", a)),
        );

        #[cfg(all(target_os = "linux", feature = "boot"))]
        {
            boot_on_kvm(
                &mem,
                mem_size,
                &loaded,
                vcpus,
                &volumes,
                net.as_deref(),
                _full_boot,
            )?;
        }
        #[cfg(not(all(target_os = "linux", feature = "boot")))]
        {
            log::warn!("boot path needs Linux+KVM + the `boot` feature");
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        log::warn!("kernel load not implemented on this arch (need x86_64)");
        let _ = (kernel, initramfs);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_create(
    socket: &str,
    kernel: String,
    cmdline: Option<String>,
    initramfs: Option<String>,
    mem_mib: u64,
    vcpus: u8,
    rootfs: Option<String>,
    volume: Vec<String>,
    overlay: Vec<String>,
) -> Result<()> {
    let volumes = build_volume_configs(rootfs.as_deref(), &volume, &overlay)?;
    let cmdline = match cmdline {
        Some(c) => c,
        None if rootfs.is_some() => format!("root=/dev/vda rw {}", vmm_loader::default_cmdline()),
        None => vmm_loader::default_cmdline(),
    };
    let config = vmm_core::config::VmConfig {
        kernel: vmm_core::config::KernelConfig {
            path: kernel,
            cmdline,
            initramfs,
        },
        memory: vmm_core::config::MemoryConfig { size_mib: mem_mib },
        vcpus: vmm_core::config::VcpuConfig { count: vcpus },
        volumes,
        net: Vec::new(),
    };
    config
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid VM config: {e}"))?;
    api_request(
        socket,
        &vmm_api::types::ApiRequest::Create(vmm_api::types::VmSpec { config }),
    )
}

fn build_volume_configs(
    rootfs: Option<&str>,
    volumes: &[String],
    overlays: &[String],
) -> Result<Vec<vmm_core::config::VolumeConfig>> {
    if !overlays.is_empty() && overlays.len() != volumes.len() {
        anyhow::bail!("--overlay must be specified once per --volume when any overlays are used");
    }

    let mut configs = Vec::with_capacity(volumes.len() + usize::from(rootfs.is_some()));
    if let Some(rfs) = rootfs {
        configs.push(vmm_core::config::VolumeConfig {
            path: rfs.into(),
            read_only: false,
            overlay: None,
        });
    }

    for (idx, spec) in volumes.iter().enumerate() {
        let mut config = parse_volume_spec(spec);
        config.overlay = overlays.get(idx).cloned();
        configs.push(config);
    }

    Ok(configs)
}

fn parse_volume_spec(spec: &str) -> vmm_core::config::VolumeConfig {
    let (path, read_only) = match spec.rsplit_once(':') {
        Some((path, "ro")) => (path, true),
        Some((path, "rw")) => (path, false),
        _ => (spec, false),
    };
    vmm_core::config::VolumeConfig {
        path: path.into(),
        read_only,
        overlay: None,
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux", feature = "boot"))]
fn boot_on_kvm(
    mem: &vmm_memory_backend::GuestMemory,
    _mem_size: u64,
    loaded: &vmm_loader::LoadedKernel,
    vcpus: u8,
    volumes: &[vmm_core::config::VolumeConfig],
    net_spec: Option<&str>,
    full_boot: bool,
) -> Result<()> {
    use vmm_core::KvmVm;
    use vmm_devices::bus::MmioRange;
    use vmm_devices::virtio::blk_transport::VirtioBlkMmio;
    use vmm_devices::virtio::net_io_loop::spawn_net_io_loop;
    use vmm_devices::virtio::net_transport::VirtioNetMmio;

    let guest_mem = mem.inner.clone();

    // IRQ EventFds for each block device. Used to signal completion to the guest.
    // NOTE: no ioeventfd for block — QUEUE_NOTIFY must fall through to userspace
    // so the synchronous process_queue() path runs. An ioeventfd would consume
    // the kick in-kernel with no drain thread to process it → boot deadlock.
    let irq_evts: Vec<vmm_sys_util::eventfd::EventFd> = (0..volumes.len())
        .map(|_| {
            vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
                .map_err(|e| anyhow::anyhow!("EventFd: {e}"))
        })
        .collect::<Result<Vec<_>>>()?;

    // Build the device list with irqfds attached.
    let mut devices: Vec<(MmioRange, Box<dyn vmm_devices::bus::MmioDevice>)> = Vec::new();
    let mut mmio_base = 0xd000_0000u64;
    let mut ioevent_addrs: Vec<(u64, vmm_sys_util::eventfd::EventFd)> = Vec::new();
    for (i, vol) in volumes.iter().enumerate() {
        let irq = 5 + i as u32;

        let backend = vmm_core::volume::open_volume_backend(vol)
            .map_err(|e| anyhow::anyhow!("blk backend {}: {e}", vol.path))?;
        let transport = VirtioBlkMmio::new(irq, backend);
        transport.set_guest_memory(guest_mem.clone());
        transport.set_irq_evt(irq_evts[i].try_clone()?);
        devices.push((MmioRange::new(mmio_base, 0x1000), Box::new(transport)));
        log::info!(
            "volume {i}: {} ({}) at mmio 0x{mmio_base:x} irq {irq}",
            vol.path,
            if vol.overlay.is_some() {
                "cow"
            } else if vol.read_only {
                "ro"
            } else {
                "rw"
            }
        );
        mmio_base += 0x1000;
    }

    // virtio-net: optional. The tap must already exist on the host (the
    // orchestrator/jailer creates it). We open it by name + wire up the
    // transport, then spawn an I/O loop after the VM is created.
    let mut net_setup: Option<(
        std::sync::Arc<VirtioNetMmio>,
        vmm_net::tap::Tap,
        vmm_sys_util::eventfd::EventFd,
        vmm_sys_util::eventfd::EventFd,
        u64,
        u32,
    )> = None;
    if let Some(spec) = net_spec {
        let mut tap_name: Option<String> = None;
        let mut mac: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
        for kv in spec.split(',') {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--net: expected k=v pairs, got {kv:?}"))?;
            match k.trim() {
                "tap" => tap_name = Some(v.trim().to_string()),
                "mac" => {
                    let parts: Vec<&str> = v.trim().split(':').collect();
                    if parts.len() != 6 {
                        return Err(anyhow::anyhow!(
                            "--net mac: expected 6 colon-separated bytes"
                        ));
                    }
                    for (i, p) in parts.iter().enumerate() {
                        mac[i] = u8::from_str_radix(p, 16)
                            .map_err(|e| anyhow::anyhow!("--net mac byte {i}: {e}"))?;
                    }
                }
                _ => log::warn!("--net: ignoring unknown key '{k}'"),
            }
        }
        let tap_name =
            tap_name.ok_or_else(|| anyhow::anyhow!("--net: missing required 'tap=<name>'"))?;
        let tap = vmm_net::tap::Tap::create(&tap_name)
            .map_err(|e| anyhow::anyhow!("tap create {tap_name}: {e}"))?;
        let irq = 5 + volumes.len() as u32;
        let net_dev = std::sync::Arc::new(VirtioNetMmio::new(irq, mac));
        net_dev.set_guest_memory(guest_mem.clone());
        net_dev.set_tap_fd(tap.fd);
        let net_irq_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| anyhow::anyhow!("net EventFd: {e}"))?;
        let net_io_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| anyhow::anyhow!("net EventFd: {e}"))?;
        net_dev.set_irq_evt(net_irq_evt.try_clone()?);
        devices.push((MmioRange::new(mmio_base, 0x1000), Box::new(net_dev.clone())));
        ioevent_addrs.push((mmio_base + 0x50, net_io_evt.try_clone()?));
        log::info!(
            "net: tap={} mac={:02x?} at mmio 0x{mmio_base:x} irq {irq}",
            tap.name,
            mac
        );
        net_setup = Some((net_dev, tap, net_irq_evt, net_io_evt, mmio_base, irq));
        let _ = mmio_base; // cursor reserved for future devices
    }

    vmm_core::write_gdt(mem)?;

    // Write ACPI tables for full boot (kernel needs MADT for IOAPIC/LAPIC
    // and DSDT for virtio-mmio device discovery).
    if full_boot {
        // Build the device list for ACPI DSDT: each volume + net gets
        // a virtio-mmio device entry at its MMIO address with its GSI.
        let mut acpi_devices: Vec<(u64, u64, u32)> = Vec::new();
        let mut acpi_mmio = 0xd000_0000u64;
        for i in 0..volumes.len() {
            acpi_devices.push((acpi_mmio, 0x1000, 5 + i as u32));
            acpi_mmio += 0x1000;
        }
        if net_spec.is_some() {
            acpi_devices.push((acpi_mmio, 0x1000, 5 + volumes.len() as u32));
        }
        vmm_core::vcpu_setup::write_acpi_tables_with_devices(mem, vcpus, &acpi_devices)?;
    }

    let template = vmm_core::cpu_template::CpuTemplate::bare();
    let vm = KvmVm::new_with_options(mem.clone(), devices, template, full_boot)
        .map_err(|e| anyhow::anyhow!("KvmVm: {e}"))?;

    // Register irqfds + ioeventfds with KVM.
    for (i, evt) in irq_evts.iter().enumerate() {
        let irq = 5 + i as u32;
        match vm.register_irqfd(evt, irq) {
            Ok(_) => log::info!("irqfd registered for volume {i} (gsi={irq})"),
            Err(e) => log::warn!("irqfd registration failed for volume {i}: {e}"),
        }
    }
    for (addr, evt) in &ioevent_addrs {
        match vm.register_ioeventfd(*addr, evt) {
            Ok(_) => log::info!("ioeventfd registered for volume at 0x{addr:x}"),
            Err(e) => log::warn!("ioeventfd registration failed at 0x{addr:x}: {e}"),
        }
    }
    let _net_io_loop = if let Some((net_dev, tap, net_irq_evt, net_io_evt, _mmio, irq)) = net_setup
    {
        match vm.register_irqfd(&net_irq_evt, irq) {
            Ok(_) => log::info!("irqfd registered for net (gsi={irq})"),
            Err(e) => log::warn!("irqfd registration failed for net: {e}"),
        }
        let tap_fd = tap.fd;
        let kick_fd = {
            use std::os::fd::AsRawFd;
            net_io_evt.as_raw_fd()
        };
        let handle = spawn_net_io_loop(net_dev, tap_fd, kick_fd)
            .map_err(|e| anyhow::anyhow!("net io_loop: {e}"))?;
        // Keep the tap + EventFds alive for the lifetime of the VM run.
        Some((handle, tap, net_irq_evt, net_io_evt))
    } else {
        None
    };
    // Register PIO ioeventfds for i8042 ports 0x60/0x64.
    // Also register an irqfd for i8042 IRQ (GSI 1) so the kernel's
    // i8042 driver gets interrupted after each keyboard write.
    let i8042_irq_evt = if full_boot {
        let evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| anyhow::anyhow!("EventFd: {e}"))?;
        match vm.register_irqfd(&evt, 1) {
            Ok(_) => {
                log::info!("i8042 irqfd registered (gsi=1)");
                Some(evt)
            }
            Err(e) => {
                log::warn!("i8042 irqfd failed: {e}");
                None
            }
        }
    } else {
        None
    };

    let mut vcpu = vm
        .create_vcpu(0)
        .map_err(|e| anyhow::anyhow!("create_vcpu: {e}"))?;
    vmm_core::setup_vcpu_for_bzimage_boot_full(&vcpu, loaded, full_boot, Some(mem))?;
    log::info!(
        "vcpu 0 configured: entry=0x{:x} — entering run loop",
        loaded.entry
    );
    if full_boot {
        // Full boot: synchronous run loop. The VM runs until the kernel
        // reboots, panics, or the watchdog timeout fires.
        vm.run_vcpu_with_i8042(&mut vcpu, i8042_irq_evt.as_ref())
            .map_err(|e| anyhow::anyhow!("run_vcpu: {e}"))?;
    } else {
        // Fast boot: synchronous run (exits on HLT loop).
        vm.run_vcpu_with_i8042(&mut vcpu, i8042_irq_evt.as_ref())
            .map_err(|e| anyhow::anyhow!("run_vcpu: {e}"))?;
    }

    let _ = vcpus;
    Ok(())
}
fn restore(snapshot: String, jail_dir: Option<String>, uid: u32, gid: u32) -> Result<()> {
    // Apply jailer confinement before restoring, if requested.
    #[cfg(target_os = "linux")]
    if let Some(chroot_dir) = &jail_dir {
        let cfg = vmm_jailer::jailer::JailerConfig {
            chroot_dir: chroot_dir.clone(),
            uid,
            gid,
            cgroup: String::new(),
            cgroup_limits: None,
            rlimit_nofile: 4096,
            rlimit_as: 0,
            netns: String::new(),
        };
        vmm_jailer::jail(&cfg).map_err(|e| anyhow::anyhow!("jail: {e}"))?;
        log::info!("jailer confinement applied: chroot={chroot_dir} uid={uid} gid={gid}");
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (jail_dir, uid, gid);
    }

    log::info!("restore: snapshot={snapshot}");
    let controller = vmm_core::VmmController::new();
    controller
        .restore(&snapshot, None)
        .map_err(|e| anyhow::anyhow!("restore: {e}"))?;
    println!("Restored VM from {snapshot}");
    Ok(())
}

fn serve(
    socket: &str,
    jail_dir: Option<String>,
    uid: u32,
    gid: u32,
    netns: Option<String>,
    cgroup: ServeCgroupArgs,
) -> Result<()> {
    // Apply jailer confinement before serving, if requested. The RPC server
    // binds the Unix socket after chroot, so `socket` is interpreted inside the
    // jail; the orchestrator must make <chroot>/<socket> reachable externally.
    // It must also provide /dev/kvm inside the jail and choose a uid/gid with
    // KVM access (for example via the kvm group) before launching this process.
    let cgroup_limits = cgroup.limits();
    #[cfg(target_os = "linux")]
    let mut netns_entered = false;
    #[cfg(target_os = "linux")]
    if let Some(chroot_dir) = &jail_dir {
        let cfg = vmm_jailer::jailer::JailerConfig {
            chroot_dir: chroot_dir.clone(),
            uid,
            gid,
            cgroup: cgroup.cgroup.clone().unwrap_or_default(),
            cgroup_limits: cgroup_limits.clone(),
            rlimit_nofile: 4096,
            rlimit_as: 0,
            netns: netns.clone().unwrap_or_default(),
        };
        vmm_jailer::jail(&cfg).map_err(|e| anyhow::anyhow!("jail: {e}"))?;
        // The jailer either enters the assigned namespace or creates a fresh
        // empty one, so host-level egress can never be affected from here.
        netns_entered = true;
        log::info!("jailer confinement applied: chroot={chroot_dir} uid={uid} gid={gid}");
    }
    #[cfg(target_os = "linux")]
    if jail_dir.is_none() {
        if let Some(cgroup_path) = &cgroup.cgroup {
            apply_serve_cgroup(cgroup_path, cgroup_limits.as_ref())?;
        }
    }
    // If we entered a per-VM network namespace, egress enforcement is safe:
    // programming the netfilter `output` hook then only affects this isolated
    // netns, never the host. Signal the RPC egress handler to apply rules for
    // real; without a netns it stays compile-only so it can't drop host egress.
    #[cfg(target_os = "linux")]
    if netns_entered {
        std::env::set_var("VMM_EGRESS_ENFORCE", "1");
    } else {
        std::env::remove_var("VMM_EGRESS_ENFORCE");
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (jail_dir, uid, gid, netns, cgroup, cgroup_limits);
    }

    log::info!("serve: API on {socket}");
    vmm_api::rpc::serve(socket).map_err(|e| anyhow::anyhow!("api serve: {e}"))
}

#[cfg(target_os = "linux")]
fn apply_serve_cgroup(
    cgroup_path: &str,
    limits: Option<&vmm_jailer::cgroups::CgroupLimits>,
) -> Result<()> {
    vmm_jailer::cgroups::apply_current_process(cgroup_path, limits)
        .map_err(|e| anyhow::anyhow!("cgroup apply: {e}"))?;
    log::info!("serve: cgroup applied: {cgroup_path}");
    Ok(())
}

fn api_snapshot(socket: &str, diff: bool) -> Result<()> {
    let request = vmm_api::types::ApiRequest::Snapshot { diff };
    let body = serde_json::to_vec(&request)?;
    let response = send_raw(socket, &body)?;
    let response: vmm_api::types::ApiResponse = serde_json::from_slice(&response)?;
    let path = match &response {
        vmm_api::types::ApiResponse::Snapshot { path } => path,
        vmm_api::types::ApiResponse::Err { msg } => anyhow::bail!("snapshot failed: {msg}"),
        other => anyhow::bail!("unexpected snapshot response: {other:?}"),
    };
    let identity = vmm_core::gc::OwnedScratchFile::identity_for(std::path::Path::new(path))
        .map_err(|error| anyhow::anyhow!("capture snapshot identity {path}: {error}"))?;
    let release = vmm_api::types::ApiRequest::ReleaseScratch {
        path: path.clone(),
        identity,
    };
    let release = serde_json::to_vec(&release)?;
    let release = send_raw(socket, &release)?;
    match serde_json::from_slice::<vmm_api::types::ApiResponse>(&release)? {
        vmm_api::types::ApiResponse::Ok => {}
        vmm_api::types::ApiResponse::Err { msg } => anyhow::bail!("release snapshot: {msg}"),
        other => anyhow::bail!("unexpected release response: {other:?}"),
    }
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn api_request(socket: &str, req: &vmm_api::types::ApiRequest) -> Result<()> {
    let body = serde_json::to_vec(req)?;
    let resp = send_raw(socket, &body)?;
    let resp: vmm_api::types::ApiResponse = serde_json::from_slice(&resp)?;
    println!("{}", serde_json::to_string_pretty(&resp)?);
    Ok(())
}

fn gc(dir: std::path::PathBuf, max_age_secs: u64) -> Result<()> {
    let report =
        vmm_core::gc::gc_scratch_files(&dir, std::time::Duration::from_secs(max_age_secs))?;
    println!(
        "GC {}: removed={} skipped_open={} errors={}",
        dir.display(),
        report.removed.len(),
        report.skipped_open.len(),
        report.errors.len()
    );
    for removed in report.removed {
        println!("removed {:?}: {}", removed.kind, removed.path.display());
    }
    for (path, error) in report.errors {
        eprintln!("gc error {}: {error}", path.display());
    }
    Ok(())
}

static SIGWINCH_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn handle_sigwinch(_sig: libc::c_int) {
    SIGWINCH_PENDING.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn attach_pty(socket: &str, shell: Option<String>) -> Result<()> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    let (cols, rows) = current_terminal_size();
    let req = vmm_api::types::ApiRequest::AttachPty { cols, rows, shell };
    let body = serde_json::to_vec(&req)?;

    let mut stream = UnixStream::connect(socket)?;
    vmm_api::rpc::write_frame(&mut stream, &body)?;
    stream.flush()?;
    // The initial control frame has a bounded deadline; the PTY stream itself
    // is intentionally long-lived and uses poll-driven backpressure.
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;

    let _raw = RawTerminal::enter(libc::STDIN_FILENO)?;
    install_sigwinch_handler();

    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let socket_fd = stream.as_raw_fd();
    let mut stdin_closed = false;

    loop {
        if SIGWINCH_PENDING.swap(false, std::sync::atomic::Ordering::Relaxed) {
            send_resize_frame(&mut stream)?;
        }

        let mut pfds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: if stdin_closed { 0 } else { libc::POLLIN },
                revents: 0,
            },
            libc::pollfd {
                fd: socket_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `pfds` points to a valid mutable array of `pollfd`s for the
        // duration of the call, and the length passed matches the array length.
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, 250) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if rc == 0 {
            continue;
        }

        if !stdin_closed && (pfds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
        {
            let mut buf = [0u8; 4096];
            let n = stdin.read(&mut buf)?;
            if n == 0 {
                stdin_closed = true;
                let _ = stream.shutdown(std::net::Shutdown::Write);
            } else {
                vmm_core::pty_stream::write_frame(
                    &mut stream,
                    vmm_core::pty_stream::TYPE_DATA,
                    &buf[..n],
                )?;
                stream.flush()?;
            }
        }

        if (pfds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
            let frame = match vmm_core::pty_stream::read_frame(&mut stream) {
                Ok(frame) => frame,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            };
            match frame.frame_type {
                vmm_core::pty_stream::TYPE_DATA => {
                    stdout.write_all(&frame.payload)?;
                    stdout.flush()?;
                }
                vmm_core::pty_stream::TYPE_EXIT => break,
                vmm_core::pty_stream::TYPE_ERROR => {
                    let msg = String::from_utf8_lossy(&frame.payload);
                    anyhow::bail!("attach-pty: {msg}");
                }
                _ => {}
            }
        }
    }

    Ok(())
}

fn send_resize_frame(stream: &mut std::os::unix::net::UnixStream) -> Result<()> {
    use std::io::Write;

    let (cols, rows) = current_terminal_size();
    vmm_core::pty_stream::write_json_frame(
        stream,
        vmm_core::pty_stream::TYPE_RESIZE,
        &vmm_core::pty_stream::PtyResize { cols, rows },
    )?;
    stream.flush()?;
    Ok(())
}

fn current_terminal_size() -> (u16, u16) {
    // SAFETY: `winsize` is a plain C struct that can be zero-initialized before
    // it is filled by the TIOCGWINSZ ioctl.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `ws` is valid writable storage for TIOCGWINSZ on stdout; the
    // return code is checked before reading ioctl-populated dimensions.
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

fn install_sigwinch_handler() {
    // SAFETY: `handle_sigwinch` has C ABI, only stores to an atomic flag, and
    // remains valid for the process lifetime. The return value is checked for
    // SIG_ERR because installing the handler can fail.
    let previous = unsafe {
        libc::signal(
            libc::SIGWINCH,
            handle_sigwinch as *const () as libc::sighandler_t,
        )
    };
    if previous == libc::SIG_ERR {
        log::warn!("signal(SIGWINCH): {}", std::io::Error::last_os_error());
    }
}

struct RawTerminal {
    fd: libc::c_int,
    original: libc::termios,
    active: bool,
}

impl RawTerminal {
    fn enter(fd: libc::c_int) -> Result<Self> {
        // SAFETY: `isatty` only observes the supplied file descriptor.
        if unsafe { libc::isatty(fd) } == 0 {
            return Ok(Self {
                fd,
                // SAFETY: This value is never read when `active` is false.
                original: unsafe { std::mem::zeroed() },
                active: false,
            });
        }

        // SAFETY: `termios` is initialized by `tcgetattr` before it is read.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `original` is valid writable termios storage for `fd`; the
        // return code is checked before use.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut raw = original;
        // SAFETY: `raw` is a valid termios value initialized from `tcgetattr`.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `raw` points to a valid termios value for this terminal fd;
        // the return code is checked.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        Ok(Self {
            fd,
            original,
            active: true,
        })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: `original` was captured from this fd by `tcgetattr` while
            // entering raw mode; Drop cannot return errors, so failures are logged.
            let rc = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
            if rc != 0 {
                log::warn!("restore terminal mode: {}", std::io::Error::last_os_error());
            }
        }
    }
}

fn pull_oci(
    image: String,
    output: String,
    size: u64,
    auth: Option<String>,
    agent: Option<String>,
) -> Result<()> {
    let oci_ref = vmm_core::oci::OciImageRef {
        reference: image,
        auth_file: auth,
    };
    let agent_path = agent.as_ref().map(std::path::PathBuf::from);
    let result = vmm_core::oci::pull_and_convert_with_agent(
        &oci_ref,
        std::path::Path::new(&output),
        size,
        agent_path.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("oci pull: {e}"))?;
    println!(
        "Pulled {} → {} ({} bytes, {}ms, agent_init={})",
        oci_ref.reference,
        result.disk_image_path,
        result.size_bytes,
        result.elapsed_ms,
        result.agent_init
    );
    if result.agent_init {
        println!("  boot: init=/usr/sbin/vmm-agent (or default cmdline; agent runs as PID 1)");
    }
    Ok(())
}

fn send_raw(socket: &str, body: &[u8]) -> Result<Vec<u8>> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(socket)?;
    vmm_api::rpc::write_frame(&mut stream, body)?;
    // Requests are small and framed writes keep the control deadline; the
    // response wait is unbounded because exec, snapshot, and restore can
    // legitimately take longer than the control-plane I/O timeout.
    Ok(vmm_api::rpc::read_frame_unbounded(&mut stream)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_accepts_overlay_for_volume() {
        let cli = Cli::try_parse_from([
            "vmm",
            "run",
            "--kernel",
            "/kernel",
            "--volume",
            "/base.img:ro",
            "--overlay",
            "/overlay.cow",
        ])
        .unwrap();

        match cli.cmd {
            Cmd::Run {
                volume, overlay, ..
            } => {
                assert_eq!(volume, vec!["/base.img:ro"]);
                assert_eq!(overlay, vec!["/overlay.cow"]);
            }
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn cli_allows_default_kernel_for_run() {
        let cli = Cli::try_parse_from(["vmm", "run"]).unwrap();
        match cli.cmd {
            Cmd::Run { kernel, .. } => assert!(kernel.is_none()),
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn cli_accepts_kernel_install() {
        let cli = Cli::try_parse_from([
            "vmm",
            "kernel",
            "install",
            "--output",
            "/tmp/vmlinux",
            "--force",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Kernel {
                command: KernelCommand::Install { output, force },
            } => {
                assert_eq!(
                    output.as_deref(),
                    Some(std::path::Path::new("/tmp/vmlinux"))
                );
                assert!(force);
            }
            _ => panic!("expected kernel install command"),
        }
    }

    #[test]
    fn volume_configs_pair_overlays_with_user_volumes() {
        let volumes = vec!["/base.img:ro".to_string()];
        let overlays = vec!["/overlay.cow".to_string()];
        let configs = build_volume_configs(Some("/rootfs.img"), &volumes, &overlays).unwrap();

        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].path, "/rootfs.img");
        assert!(!configs[0].read_only);
        assert_eq!(configs[0].overlay, None);
        assert_eq!(configs[1].path, "/base.img");
        assert!(configs[1].read_only);
        assert_eq!(configs[1].overlay.as_deref(), Some("/overlay.cow"));
    }

    #[test]
    fn cli_accepts_attach_pty_shell() {
        let cli = Cli::try_parse_from([
            "vmm",
            "--socket",
            "/run/vmm.sock",
            "attach-pty",
            "--shell",
            "/bin/sh",
        ])
        .unwrap();

        match cli.cmd {
            Cmd::AttachPty { shell } => assert_eq!(shell.as_deref(), Some("/bin/sh")),
            _ => panic!("expected attach-pty command"),
        }
        assert_eq!(cli.socket, "/run/vmm.sock");
    }

    #[test]
    fn overlay_count_must_match_volume_count_when_present() {
        let volumes = vec!["/one.img".to_string(), "/two.img".to_string()];
        let overlays = vec!["/one.cow".to_string()];
        assert!(build_volume_configs(None, &volumes, &overlays).is_err());
    }

    #[test]
    fn cli_accepts_gc_options() {
        let cli = Cli::try_parse_from([
            "vmm",
            "gc",
            "--dir",
            "target/test-work/gc",
            "--max-age",
            "5",
        ])
        .unwrap();

        match cli.cmd {
            Cmd::Gc { dir, max_age } => {
                assert_eq!(dir, std::path::PathBuf::from("target/test-work/gc"));
                assert_eq!(max_age, 5);
            }
            _ => panic!("expected gc command"),
        }
    }

    #[test]
    fn human_size_parser_accepts_binary_suffixes() {
        assert_eq!(parse_human_bytes("512M").unwrap(), 536_870_912);
        assert_eq!(parse_human_bytes("1G").unwrap(), 1_073_741_824);
        assert_eq!(parse_human_bytes("4096").unwrap(), 4096);
        assert!(parse_human_bytes("1.5G").is_err());
    }

    #[test]
    fn cpu_max_parser_accepts_millicpu_and_quota_period() {
        assert_eq!(parse_cgroup_cpu_max("1000m").unwrap(), "100000 100000");
        assert_eq!(parse_cgroup_cpu_max("2500m").unwrap(), "250000 100000");
        assert_eq!(
            parse_cgroup_cpu_max("50000/100000").unwrap(),
            "50000 100000"
        );
        assert_eq!(parse_cgroup_cpu_max("max/100000").unwrap(), "max 100000");
    }

    #[test]
    fn serve_cgroup_flags_build_limits() {
        let cli = Cli::try_parse_from([
            "vmm",
            "serve",
            "--cgroup",
            "/sys/fs/cgroup/vmm-test",
            "--cgroup-memory-max",
            "512M",
            "--cgroup-cpu-max",
            "1000m",
            "--cgroup-pids-max",
            "64",
            "--cpuset",
            "0-1",
        ])
        .unwrap();

        match cli.cmd {
            Cmd::Serve { cgroup, .. } => {
                assert_eq!(cgroup.cgroup.as_deref(), Some("/sys/fs/cgroup/vmm-test"));
                assert_eq!(
                    cgroup.limits(),
                    Some(vmm_jailer::cgroups::CgroupLimits {
                        cpu_max: Some("100000 100000".into()),
                        cpuset_cpus: Some("0-1".into()),
                        memory_max: Some(536_870_912),
                        pids_max: Some(64),
                        ..Default::default()
                    })
                );
            }
            _ => panic!("expected serve command"),
        }
    }

    #[test]
    fn serve_cgroup_limit_flags_require_cgroup_path() {
        assert!(Cli::try_parse_from(["vmm", "serve", "--cgroup-memory-max", "512M",]).is_err());
    }
}
