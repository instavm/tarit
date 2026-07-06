use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use crossterm::terminal;
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use std::{
    env, io::Write as _, net::SocketAddr, path::PathBuf, process::Command as ProcessCommand,
};
use tarit_types::{ExecutionRecord, VmRecord};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";

#[derive(Debug, Parser)]
#[command(
    name = "taritd",
    version,
    about = "Tarit: run or operate the microVM orchestrator and PaaS control plane"
)]
pub struct Cli {
    #[arg(long, global = true, env = "TARIT_BASE_URL", default_value = DEFAULT_BASE_URL, value_name = "URL")]
    base_url: String,
    #[arg(long, global = true, env = "TARIT_API_KEY", value_name = "KEY")]
    api_key: Option<String>,
    #[arg(long, global = true, help = "Print raw JSON responses")]
    json: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn runs_server(&self) -> bool {
        matches!(&self.command, None | Some(Command::Serve))
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Run the API daemon")]
    Serve,
    #[command(about = "Check API health")]
    Health,
    #[command(about = "Show cluster capacity and health")]
    Cluster,
    #[command(about = "Print Prometheus metrics")]
    Metrics,
    #[command(about = "Manage VMs")]
    Vm {
        #[command(subcommand)]
        command: VmCommand,
    },
    #[command(about = "Build and manage registered rootfs images")]
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    #[command(about = "Restore a VM from a snapshot")]
    Restore(RestoreArgs),
    #[command(about = "Run a command inside a VM")]
    Exec(ExecArgs),
    #[command(name = "ssh-key", about = "Manage caller-scoped SSH keys")]
    SshKey {
        #[command(subcommand)]
        command: SshKeyCommand,
    },
    #[command(about = "Attach to a VM PTY over WebSocket")]
    Pty(PtyArgs),
    #[command(about = "Open the VM through the taritd SSH gateway")]
    Ssh(SshArgs),
}

#[derive(Debug, Subcommand)]
pub enum VmCommand {
    #[command(about = "Create a VM")]
    Create(VmCreateArgs),
    #[command(about = "List local VM records")]
    List,
    #[command(about = "Show one VM")]
    Get(VmIdArgs),
    #[command(about = "Delete a VM")]
    Delete(VmIdArgs),
    #[command(about = "Pause a VM")]
    Pause(VmIdArgs),
    #[command(about = "Resume a VM")]
    Resume(VmIdArgs),
    #[command(about = "Snapshot a VM")]
    Snapshot(VmSnapshotArgs),
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    #[command(about = "Build and register a rootfs image from an OCI image")]
    Build(ImageBuildArgs),
    #[command(name = "ls", about = "List registered images")]
    List,
    #[command(name = "rm", about = "Remove an unreferenced image")]
    Remove(ImageRemoveArgs),
    #[command(about = "Remove unreferenced images older than a threshold")]
    Gc(ImageGcArgs),
}

#[derive(Debug, Args)]
pub struct ImageBuildArgs {
    #[arg(long, value_name = "OCI_REF")]
    oci: String,
    #[arg(long, value_name = "NAME[:TAG]")]
    name: String,
}

#[derive(Debug, Args)]
pub struct ImageRemoveArgs {
    #[arg(value_name = "NAME[:TAG]")]
    name: String,
}

#[derive(Debug, Args)]
pub struct ImageGcArgs {
    #[arg(long, default_value_t = 7, value_name = "DAYS")]
    older_than_days: u64,
    #[arg(long, value_name = "PATTERN")]
    pattern: Option<String>,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
pub struct VmCreateArgs {
    #[arg(long, value_name = "N")]
    memory_mib: Option<u64>,
    #[arg(long, value_name = "N")]
    vcpus: Option<u8>,
    #[arg(long, value_name = "PATH")]
    rootfs: Option<PathBuf>,
    #[arg(long, value_name = "NAME[:TAG]", conflicts_with = "rootfs")]
    image: Option<String>,
}

#[derive(Debug, Args)]
pub struct VmIdArgs {
    id: Uuid,
}

#[derive(Debug, Args)]
pub struct VmSnapshotArgs {
    id: Uuid,
    #[arg(long)]
    diff: bool,
}

#[derive(Debug, Args)]
pub struct RestoreArgs {
    snapshot_path: PathBuf,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    id: Uuid,
    #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
    cmd: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum SshKeyCommand {
    #[command(about = "Add an OpenSSH public key")]
    Add(SshKeyAddArgs),
    #[command(about = "List active SSH keys")]
    List,
    #[command(name = "rm", about = "Remove an SSH key")]
    Remove(SshKeyRemoveArgs),
}

#[derive(Debug, Args)]
pub struct SshKeyAddArgs {
    #[arg(value_name = "FILE|-")]
    file: String,
}

#[derive(Debug, Args)]
pub struct SshKeyRemoveArgs {
    key_id: Uuid,
}

#[derive(Debug, Args)]
pub struct PtyArgs {
    id: Uuid,
    #[arg(long, value_name = "S")]
    shell: Option<String>,
}

#[derive(Debug, Args)]
pub struct SshArgs {
    id: Uuid,
    #[arg(long, value_name = "HOST")]
    ssh_host: Option<String>,
    #[arg(long, value_name = "PORT")]
    ssh_port: Option<u16>,
    #[arg(num_args = 0.., trailing_var_arg = true, allow_hyphen_values = true)]
    extra_args: Vec<String>,
}

pub async fn run_client(cli: Cli) -> Result<()> {
    let Cli {
        base_url,
        api_key,
        json,
        command,
    } = cli;
    let command = match command {
        Some(command) => command,
        None => return Ok(()),
    };

    if let Command::Ssh(args) = command {
        return run_ssh(args);
    }
    if let Command::Image { command } = command {
        return image(command, json);
    }

    let client = ClientConfig::new(base_url, api_key, json)?;
    match command {
        Command::Serve | Command::Ssh(_) | Command::Image { .. } => Ok(()),
        Command::Health => health(&client).await,
        Command::Cluster => cluster(&client).await,
        Command::Metrics => metrics(&client).await,
        Command::Vm { command } => vm(&client, command).await,
        Command::Restore(args) => restore(&client, args).await,
        Command::Exec(args) => exec(&client, args).await,
        Command::SshKey { command } => ssh_key(&client, command).await,
        Command::Pty(args) => pty(&client, args).await,
    }
}

struct ClientConfig {
    base_url: String,
    api_key: Option<String>,
    json: bool,
    http: reqwest::Client,
}

impl ClientConfig {
    fn new(base_url: String, api_key: Option<String>, json: bool) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        reqwest::Url::parse(&base_url).with_context(|| format!("invalid base URL {base_url}"))?;
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            base_url,
            api_key: api_key.filter(|key| !key.is_empty()),
            json,
            http,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn ws_endpoint(&self, path: &str, connect_token: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.endpoint(path))?;
        let scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            "ws" => "ws",
            "wss" => "wss",
            other => bail!("unsupported base URL scheme {other}"),
        };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid WebSocket URL scheme"))?;
        url.query_pairs_mut().append_pair("token", connect_token);
        Ok(url)
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<String> {
        let mut req = self.http.request(method, self.endpoint(path));
        if let Some(api_key) = &self.api_key {
            req = req.header("X-API-Key", api_key);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }

        let resp = req.send().await.context("HTTP request failed")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            if text.is_empty() {
                bail!("HTTP {status}");
            }
            bail!("HTTP {status}: {text}");
        }
        Ok(text)
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(String, T)> {
        let text = self.request(method, path, body).await?;
        let parsed =
            serde_json::from_str(&text).with_context(|| format!("decode JSON from {path}"))?;
        Ok((text, parsed))
    }
}

async fn health(client: &ClientConfig) -> Result<()> {
    let (body, value): (String, Value) = client.json(Method::GET, "/health", None).await?;
    if client.json {
        print_json(&body);
    } else {
        println!(
            "{}",
            value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
    }
    Ok(())
}

async fn cluster(client: &ClientConfig) -> Result<()> {
    let (body, value): (String, Value) = client.json(Method::GET, "/v1/cluster", None).await?;
    if client.json {
        print_json(&body);
        return Ok(());
    }

    println!("host: {}", str_field(&value, "this_host"));
    println!("clustered: {}", bool_field(&value, "clustered"));
    println!(
        "nodes: {}/{} healthy",
        u64_field(&value, "healthy_nodes"),
        u64_field(&value, "total_nodes")
    );
    println!(
        "free: {} vcpus, {} MiB",
        u64_field(&value, "cluster_free_vcpus"),
        u64_field(&value, "cluster_free_memory_mib")
    );
    if let Some(nodes) = value.get("nodes").and_then(Value::as_array) {
        println!(
            "{:<24} {:<5} {:>8} {:>10} {:>10}",
            "HOST", "UP", "VMS", "VCPUS", "MEM"
        );
        for node in nodes {
            println!(
                "{:<24} {:<5} {:>8} {:>10} {:>10}",
                str_field(node, "host_id"),
                bool_field(node, "up"),
                u64_field(node, "sandbox_count"),
                u64_field(node, "free_vcpus"),
                u64_field(node, "free_memory_mib")
            );
        }
    }
    Ok(())
}

async fn metrics(client: &ClientConfig) -> Result<()> {
    let body = client.request(Method::GET, "/metrics", None).await?;
    print!("{body}");
    std::io::stdout().flush().ok();
    Ok(())
}

async fn vm(client: &ClientConfig, command: VmCommand) -> Result<()> {
    match command {
        VmCommand::Create(args) => vm_create(client, args).await,
        VmCommand::List => vm_list(client).await,
        VmCommand::Get(args) => vm_get(client, args.id).await,
        VmCommand::Delete(args) => vm_delete(client, args.id).await,
        VmCommand::Pause(args) => vm_action(client, args.id, "pause").await,
        VmCommand::Resume(args) => vm_action(client, args.id, "resume").await,
        VmCommand::Snapshot(args) => vm_snapshot(client, args).await,
    }
}

async fn vm_create(client: &ClientConfig, args: VmCreateArgs) -> Result<()> {
    let mut body = serde_json::Map::new();
    if let Some(memory_mib) = args.memory_mib {
        body.insert("memory_mib".into(), json!(memory_mib));
    }
    if let Some(vcpus) = args.vcpus {
        body.insert("vcpus".into(), json!(vcpus));
    }
    if let Some(rootfs) = args.rootfs {
        body.insert(
            "rootfs_path".into(),
            json!(rootfs.to_string_lossy().to_string()),
        );
    }
    if let Some(image) = args.image {
        body.insert("image".into(), json!(image));
    }
    let (raw, vm): (String, VmRecord) = client
        .json(Method::POST, "/v1/vms", Some(Value::Object(body)))
        .await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("{} {}", vm.id, vm.status.as_str());
    }
    Ok(())
}

fn image(command: ImageCommand, json_output: bool) -> Result<()> {
    match command {
        ImageCommand::Build(args) => image_build(args, json_output),
        ImageCommand::List => image_list(json_output),
        ImageCommand::Remove(args) => image_remove(args, json_output),
        ImageCommand::Gc(args) => image_gc(args, json_output),
    }
}

fn image_build(args: ImageBuildArgs, json_output: bool) -> Result<()> {
    let config = crate::image::LocalImageConfig::from_env();
    let image_ref = crate::image::parse_image_ref(&args.name)?;
    let image = crate::image::build_image(crate::image::BuildImageOptions {
        oci_ref: args.oci,
        image_ref,
        vmm_bin: config.vmm_bin,
        vmm_agent: config.vmm_agent,
        db_path: config.db_path,
        images_dir: config.images_dir,
    })?;
    if json_output {
        println!("{}", image_json(&image));
    } else {
        println!(
            "{}:{} {} {}",
            image.name,
            image.tag,
            format_bytes(image.size_bytes),
            image.rootfs_path
        );
    }
    Ok(())
}

fn image_list(json_output: bool) -> Result<()> {
    let config = crate::image::LocalImageConfig::from_env();
    let images = crate::image::list_images(&config)?;
    if json_output {
        let values = images.iter().map(image_json).collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&values)?);
        return Ok(());
    }
    println!(
        "{:<32} {:<16} {:>10} {:<24} ROOTFS",
        "NAME", "TAG", "SIZE", "CREATED"
    );
    for image in images {
        println!(
            "{:<32} {:<16} {:>10} {:<24} {}",
            image.name,
            image.tag,
            format_bytes(image.size_bytes),
            image.created_at.to_rfc3339(),
            image.rootfs_path
        );
    }
    Ok(())
}

fn image_remove(args: ImageRemoveArgs, json_output: bool) -> Result<()> {
    let config = crate::image::LocalImageConfig::from_env();
    let image_ref = crate::image::parse_image_ref(&args.name)?;
    let image = crate::image::remove_image(&config, &image_ref)?;
    if json_output {
        println!("{}", image_json(&image));
    } else {
        println!("removed {}:{}", image.name, image.tag);
    }
    Ok(())
}

fn image_gc(args: ImageGcArgs, json_output: bool) -> Result<()> {
    let config = crate::image::LocalImageConfig::from_env();
    let plan = crate::image::gc_images(
        &config,
        args.older_than_days,
        args.pattern.as_deref(),
        args.dry_run,
    )?;
    if json_output {
        let values = plan.candidates.iter().map(image_json).collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        let verb = if args.dry_run {
            "would remove"
        } else {
            "removed"
        };
        println!("{verb} {} image(s)", plan.candidates.len());
        for image in plan.candidates {
            println!("{}:{} {}", image.name, image.tag, image.rootfs_path);
        }
    }
    Ok(())
}

async fn vm_list(client: &ClientConfig) -> Result<()> {
    let (raw, vms): (String, Vec<VmRecord>) = client.json(Method::GET, "/v1/vms", None).await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("{:<36} {:<10} {:>5} {:>8}", "ID", "STATUS", "VCPUS", "MEM");
        for vm in vms {
            println!(
                "{:<36} {:<10} {:>5} {:>8}",
                vm.id,
                vm.status.as_str(),
                vm.vcpus,
                vm.memory_mib
            );
        }
    }
    Ok(())
}

async fn vm_get(client: &ClientConfig, id: Uuid) -> Result<()> {
    let (raw, vm): (String, VmRecord) = client
        .json(Method::GET, &format!("/v1/vms/{id}"), None)
        .await?;
    if client.json {
        print_json(&raw);
    } else {
        print_vm(&vm);
    }
    Ok(())
}

async fn vm_delete(client: &ClientConfig, id: Uuid) -> Result<()> {
    let raw = client
        .request(Method::DELETE, &format!("/v1/vms/{id}"), None)
        .await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("deleted {id}");
    }
    Ok(())
}

async fn vm_action(client: &ClientConfig, id: Uuid, action: &str) -> Result<()> {
    let (raw, vm): (String, VmRecord) = client
        .json(
            Method::POST,
            &format!("/v1/vms/{id}/{action}"),
            Some(json!({})),
        )
        .await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("{} {}", vm.id, vm.status.as_str());
    }
    Ok(())
}

async fn vm_snapshot(client: &ClientConfig, args: VmSnapshotArgs) -> Result<()> {
    let (raw, snapshot): (String, SnapshotResponse) = client
        .json(
            Method::POST,
            &format!("/v1/vms/{}/snapshot", args.id),
            Some(json!({ "diff": args.diff })),
        )
        .await?;
    if client.json {
        print_json(&raw);
    } else if let Some(host_id) = snapshot.host_id {
        println!("{} {}", snapshot.path, host_id);
    } else {
        println!("{}", snapshot.path);
    }
    Ok(())
}

async fn restore(client: &ClientConfig, args: RestoreArgs) -> Result<()> {
    let (raw, vm): (String, VmRecord) = client
        .json(
            Method::POST,
            "/v1/restore",
            Some(json!({ "snapshot_path": args.snapshot_path.to_string_lossy() })),
        )
        .await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("{} {}", vm.id, vm.status.as_str());
    }
    Ok(())
}

async fn exec(client: &ClientConfig, args: ExecArgs) -> Result<()> {
    let command = args.cmd.join(" ");
    let raw = client
        .request(
            Method::POST,
            "/v1/execute",
            Some(json!({ "vm_id": args.id, "command": command })),
        )
        .await?;
    let record: ExecutionRecord =
        serde_json::from_str(&raw).context("decode execution response")?;
    if client.json {
        print_json(&raw);
    } else {
        if let Some(stdout) = &record.stdout {
            print!("{stdout}");
            std::io::stdout().flush().ok();
        }
        if let Some(stderr) = &record.stderr {
            eprint!("{stderr}");
            std::io::stderr().flush().ok();
        }
    }

    if let Some(error) = record.error {
        bail!("execution failed: {error}");
    }
    let code = record.exit_code.unwrap_or(1);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

async fn ssh_key(client: &ClientConfig, command: SshKeyCommand) -> Result<()> {
    match command {
        SshKeyCommand::Add(args) => ssh_key_add(client, args).await,
        SshKeyCommand::List => ssh_key_list(client).await,
        SshKeyCommand::Remove(args) => ssh_key_remove(client, args.key_id).await,
    }
}

async fn ssh_key_add(client: &ClientConfig, args: SshKeyAddArgs) -> Result<()> {
    let public_key = if args.file == "-" {
        let mut input = String::new();
        tokio::io::stdin()
            .read_to_string(&mut input)
            .await
            .context("read public key from stdin")?;
        input
    } else {
        tokio::fs::read_to_string(&args.file)
            .await
            .with_context(|| format!("read public key from {}", args.file))?
    };
    if public_key.trim().is_empty() {
        bail!("SSH public key is empty");
    }

    let (raw, key): (String, SshKeySummary) = client
        .json(
            Method::POST,
            "/v1/ssh-keys",
            Some(json!({ "public_key": public_key.trim() })),
        )
        .await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("{} {}", key.id, key.fingerprint);
    }
    Ok(())
}

async fn ssh_key_list(client: &ClientConfig) -> Result<()> {
    let (raw, keys): (String, SshKeyListResponse) =
        client.json(Method::GET, "/v1/ssh-keys", None).await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("{:<36} {:<48} TYPE", "ID", "FINGERPRINT");
        for key in keys.keys {
            println!("{:<36} {:<48} {}", key.id, key.fingerprint, key.key_type);
        }
    }
    Ok(())
}

async fn ssh_key_remove(client: &ClientConfig, key_id: Uuid) -> Result<()> {
    let raw = client
        .request(Method::DELETE, &format!("/v1/ssh-keys/{key_id}"), None)
        .await?;
    if client.json {
        print_json(&raw);
    } else {
        println!("removed {key_id}");
    }
    Ok(())
}

async fn pty(client: &ClientConfig, args: PtyArgs) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let (_, session): (String, PtySessionResponse) = client
        .json(
            Method::POST,
            &format!("/v1/vms/{}/pty/sessions", args.id),
            Some(json!({ "cols": cols, "rows": rows, "shell": args.shell })),
        )
        .await?;
    let ws_url = client.ws_endpoint(
        &format!("/v1/vms/{}/pty/{}/connect", args.id, session.pty_id),
        &session.connect_token,
    )?;

    let (ws, _) = connect_async(ws_url.as_str())
        .await
        .context("connect PTY WebSocket")?;
    let (mut ws_write, mut ws_read) = ws.split();
    let raw_mode = RawMode::enable()?;
    let mut stdout = tokio::io::stdout();
    let mut exit_code = 0;
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .context("listen for SIGWINCH")?;

    // Read stdin on a dedicated blocking thread and forward chunks over a
    // channel. Polling tokio::io::stdin directly inside select! is unreliable
    // for an interactive raw terminal: each loop iteration re-creates the read
    // future while a blocking read is in flight, which can drop input bytes.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0_u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        tokio::select! {
            chunk = stdin_rx.recv() => {
                match chunk {
                    Some(data) => {
                        ws_write
                            .send(Message::binary(data))
                            .await
                            .context("send PTY input")?;
                    }
                    None => {
                        let _ = ws_write.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
            message = ws_read.next() => {
                match message {
                    Some(Ok(Message::Binary(data))) => {
                        stdout.write_all(data.as_ref()).await.context("write PTY output")?;
                        stdout.flush().await.ok();
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(PtyServerMessage::Exit { exit_code: code }) = serde_json::from_str(text.as_ref()) {
                            exit_code = code.unwrap_or(0);
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e).context("PTY WebSocket"),
                }
            }
            _ = sigwinch.recv() => {
                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                ws_write
                    .send(Message::text(json!({ "type": "resize", "cols": cols, "rows": rows }).to_string()))
                    .await
                    .context("send PTY resize")?;
            }
        }
    }

    drop(raw_mode);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn run_ssh(args: SshArgs) -> Result<()> {
    let (default_host, default_port) = ssh_gateway_addr();
    let host = args.ssh_host.unwrap_or(default_host);
    let port = args.ssh_port.unwrap_or(default_port);
    let status = ProcessCommand::new("ssh")
        .arg("-p")
        .arg(port.to_string())
        .arg(format!("{}@{host}", args.id))
        .args(args.extra_args)
        .status()
        .context("run ssh")?;
    std::process::exit(status.code().unwrap_or(1));
}

fn ssh_gateway_addr() -> (String, u16) {
    let raw = env::var("TARIT_SSH_GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:2222".into());
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return (addr.ip().to_string(), addr.port());
    }
    if let Some((host, port)) = raw.rsplit_once(':') {
        if let Ok(port) = port.parse::<u16>() {
            return (host.trim_matches(&['[', ']'][..]).to_string(), port);
        }
    }
    ("127.0.0.1".into(), 2222)
}

fn print_vm(vm: &VmRecord) {
    println!("id: {}", vm.id);
    println!("status: {}", vm.status.as_str());
    println!("host: {}", vm.host_id);
    println!("vcpus: {}", vm.vcpus);
    println!("memory_mib: {}", vm.memory_mib);
    println!("kernel: {}", vm.kernel_path);
    if let Some(rootfs) = &vm.rootfs_path {
        println!("rootfs: {rootfs}");
    }
    if let Some(pid) = vm.pid {
        println!("pid: {pid}");
    }
}

fn print_json(body: &str) {
    if body.is_empty() {
        return;
    }
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
    std::io::stdout().flush().ok();
}

fn image_json(image: &tarit_store::ImageRecord) -> Value {
    json!({
        "name": image.name,
        "tag": image.tag,
        "rootfs_path": image.rootfs_path,
        "created_at": image.created_at,
        "size_bytes": image.size_bytes,
        "source_ref": image.source_ref,
        "golden_snapshot_path": image.golden_snapshot_path,
    })
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("-")
        .to_string()
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

struct RawMode;

impl RawMode {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode().context("enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Debug, Deserialize)]
struct SnapshotResponse {
    path: String,
    host_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SshKeySummary {
    id: Uuid,
    fingerprint: String,
    key_type: String,
}

#[derive(Debug, Deserialize)]
struct SshKeyListResponse {
    keys: Vec<SshKeySummary>,
}

#[derive(Debug, Deserialize)]
struct PtySessionResponse {
    pty_id: Uuid,
    connect_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PtyServerMessage {
    Exit { exit_code: Option<i32> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_args_dispatch_to_server() {
        let cli = Cli::try_parse_from(["taritd"]).unwrap();
        assert!(cli.runs_server());
        assert!(cli.command.is_none());
    }

    #[test]
    fn serve_dispatches_to_server() {
        let cli = Cli::try_parse_from(["taritd", "serve"]).unwrap();
        assert!(cli.runs_server());
        assert!(matches!(cli.command, Some(Command::Serve)));
    }

    #[test]
    fn vm_create_vcpus_parses() {
        let cli = Cli::try_parse_from(["taritd", "vm", "create", "--vcpus", "2"]).unwrap();
        match cli.command {
            Some(Command::Vm {
                command: VmCommand::Create(args),
            }) => assert_eq!(args.vcpus, Some(2)),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
