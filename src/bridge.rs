use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "fractal.local_bridge.v1";
const MAX_HEADERS: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfig {
    schema: String,
    token: String,
    port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildRequest {
    request: String,
    project_name: String,
    #[serde(default)]
    lead_agent: String,
}

#[derive(Deserialize)]
struct AmendRequest {
    request: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopRequest {
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    all: bool,
}

#[derive(Debug, Serialize)]
struct CommandResponse {
    ok: bool,
    exit_code: i32,
    output: String,
}

#[derive(Debug, Serialize)]
struct ReadinessResponse {
    agents: Vec<AgentReadiness>,
    git_installed: bool,
    github_cli_installed: bool,
    github_authenticated: bool,
    fractal_society_authenticated: bool,
    fractal_society_account: Option<String>,
}

#[derive(Debug, Serialize)]
struct AgentReadiness {
    id: &'static str,
    installed: bool,
    authenticated: bool,
}

pub(crate) fn serve(port: u16, fractalwork: Option<&Path>, coordinate: bool) -> Result<()> {
    let mut config = ensure_config(port)?;
    if config.port != port {
        config.port = port;
        save_config(&config)?;
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("bind Fractal bridge to 127.0.0.1:{port}"))?;
    println!("Fractal local bridge listening on http://127.0.0.1:{port}");
    for connection in listener.incoming() {
        let stream = match connection {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("bridge connection note: {error}");
                continue;
            }
        };
        let token = config.token.clone();
        let fractalwork = fractalwork.map(Path::to_path_buf);
        thread::spawn(move || {
            if let Err(error) = handle(stream, &token, fractalwork.as_deref(), coordinate) {
                eprintln!("bridge request note: {error:#}");
            }
        });
    }
    Ok(())
}

pub(crate) fn install(port: u16) -> Result<()> {
    let config = ensure_config(port)?;
    let executable = std::env::current_exe().context("resolve current Fractal executable")?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unavailable")?;
    let agents = home.join("Library/LaunchAgents");
    fs::create_dir_all(&agents).context("create LaunchAgents directory")?;
    let plist = agents.join("com.fractalsociety.fractal-bridge.plist");
    let log = bridge_root()?.join("bridge.log");
    let command_path = std::env::var("PATH").unwrap_or_else(|_| {
        format!(
            "{}/.cargo/bin:{}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
            home.display(),
            home.display()
        )
    });
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\n\
<key>Label</key><string>com.fractalsociety.fractal-bridge</string>\n\
<key>ProgramArguments</key><array><string>{}</string><string>bridge</string>\
<string>serve</string><string>--port</string><string>{}</string></array>\n\
<key>EnvironmentVariables</key><dict>\
<key>HOME</key><string>{}</string><key>PATH</key><string>{}</string></dict>\n\
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n\
<key>StandardOutPath</key><string>{}</string>\n\
<key>StandardErrorPath</key><string>{}</string>\n\
</dict></plist>\n",
        xml_escape(&executable.display().to_string()),
        port,
        xml_escape(&home.display().to_string()),
        xml_escape(&command_path),
        xml_escape(&log.display().to_string()),
        xml_escape(&log.display().to_string())
    );
    fs::write(&plist, xml).with_context(|| format!("write {}", plist.display()))?;
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &domain, &plist.display().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain, &plist.display().to_string()])
        .status()
        .context("start Fractal bridge launch agent")?;
    if !status.success() {
        bail!("launchctl could not start the Fractal bridge");
    }
    println!("Fractal bridge installed and started.");
    println!("Pairing token: {}", config.token);
    println!("Enter this token in Fractal Voice. Treat it like a local password.");
    Ok(())
}

pub(crate) fn print_token() -> Result<()> {
    println!("{}", ensure_config(18_372)?.token);
    Ok(())
}

pub(crate) fn status(port: u16) -> Result<()> {
    let response = ureq::get(&format!("http://127.0.0.1:{port}/v1/health"))
        .timeout(Duration::from_secs(2))
        .call()
        .context("Fractal bridge is not reachable")?;
    if response.status() != 200 {
        bail!("Fractal bridge health check returned {}", response.status());
    }
    println!("Fractal bridge is ready on 127.0.0.1:{port}");
    Ok(())
}

fn handle(
    mut stream: TcpStream,
    token: &str,
    fractalwork: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let request = read_request(&mut stream)?;
    if request.origin.is_some() {
        return write_json(
            &mut stream,
            403,
            &serde_json::json!({"error":"browser origins denied"}),
        );
    }
    if request.method == "GET" && request.path == "/v1/health" {
        return write_json(
            &mut stream,
            200,
            &serde_json::json!({"schema":SCHEMA,"status":"ready"}),
        );
    }
    if !constant_time_equal(request.bearer.as_deref().unwrap_or(""), token) {
        return write_json(
            &mut stream,
            401,
            &serde_json::json!({"error":"unauthorized"}),
        );
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/v1/build") => {
            let input: BuildRequest =
                serde_json::from_slice(&request.body).context("decode bridge build request")?;
            let content = input.request.trim();
            let project_name = input.project_name.trim();
            if content.is_empty() || content.len() > 32 * 1024 {
                return write_json(
                    &mut stream,
                    400,
                    &serde_json::json!({"error":"invalid request"}),
                );
            }
            if project_name.is_empty() || project_name.len() > 80 {
                return write_json(
                    &mut stream,
                    400,
                    &serde_json::json!({"error":"invalid project name"}),
                );
            }
            let executable = std::env::current_exe().context("resolve Fractal executable")?;
            let mut command = Command::new(executable);
            if ["codex", "cursor", "claude", "hermes"].contains(&input.lead_agent.as_str()) {
                command.env("FRACTAL_LEAD_AGENT", &input.lead_agent);
            }
            if let Some(fractalwork) = fractalwork {
                command.arg("--fractalwork").arg(fractalwork);
            }
            if coordinate {
                command.arg("--coordinate");
            }
            let mut child = command
                .args([
                    "ingest",
                    "--source",
                    "fractal-mac-app",
                    "--format",
                    "text",
                    "--stdin",
                    "--managed-project",
                    "--project-name",
                    project_name,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("start managed Fractal build")?;
            child
                .stdin
                .take()
                .context("open build stdin")?
                .write_all(content.as_bytes())
                .context("send build request")?;
            let output = child
                .wait_with_output()
                .context("wait for managed Fractal build")?;
            let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            if combined.len() > 2 * 1024 * 1024 {
                combined.truncate(2 * 1024 * 1024);
                combined.push_str("\n[bridge output truncated]\n");
            }
            write_json(
                &mut stream,
                200,
                &CommandResponse {
                    ok: output.status.success(),
                    exit_code: output.status.code().unwrap_or(1),
                    output: combined,
                },
            )
        }
        ("POST", "/v1/stop") => {
            let input: StopRequest =
                serde_json::from_slice(&request.body).context("decode bridge stop request")?;
            let args = crate::cli::StopArgs {
                project: input.project,
                all: input.all,
            };
            match crate::run_control::stop(&args) {
                Ok(()) => write_json(&mut stream, 200, &serde_json::json!({"ok":true})),
                Err(error) => write_json(
                    &mut stream,
                    409,
                    &serde_json::json!({"ok":false,"error":format!("{error:#}")}),
                ),
            }
        }
        ("POST", "/v1/amend") => {
            let input: AmendRequest =
                serde_json::from_slice(&request.body).context("decode bridge amendment request")?;
            let content = input.request.trim();
            if content.is_empty() || content.len() > 4_000 {
                return write_json(
                    &mut stream,
                    400,
                    &serde_json::json!({"error":"invalid amendment request"}),
                );
            }
            let mut child = Command::new(std::env::current_exe()?)
                .args([
                    "ingest",
                    "--source",
                    "fractal-mac-app",
                    "--format",
                    "text",
                    "--stdin",
                    "--amend",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("start Fractal amendment intake")?;
            child
                .stdin
                .take()
                .context("open amendment stdin")?
                .write_all(content.as_bytes())?;
            let output = child.wait_with_output()?;
            let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            write_json(
                &mut stream,
                200,
                &CommandResponse {
                    ok: output.status.success(),
                    exit_code: output.status.code().unwrap_or(1),
                    output: combined,
                },
            )
        }
        ("GET", "/v1/readiness") => write_json(&mut stream, 200, &readiness()),
        ("POST", "/v1/login") => {
            let output = Command::new(std::env::current_exe()?)
                .arg("login")
                .output()
                .context("start Fractal Society login")?;
            let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            write_json(
                &mut stream,
                200,
                &CommandResponse {
                    ok: output.status.success(),
                    exit_code: output.status.code().unwrap_or(1),
                    output: combined,
                },
            )
        }
        _ => write_json(&mut stream, 404, &serde_json::json!({"error":"not found"})),
    }
}

fn readiness() -> ReadinessResponse {
    let agents = [
        ("codex", &["codex", "login", "status"][..]),
        ("cursor", &["cursor-agent", "status"][..]),
        ("claude", &["claude", "auth", "status"][..]),
        ("hermes", &["hermes", "status"][..]),
    ]
    .into_iter()
    .map(|(id, command)| {
        let result = command_output(command);
        AgentReadiness {
            id,
            installed: result.is_some(),
            authenticated: result
                .as_ref()
                .is_some_and(|(success, output)| *success && agent_authenticated(id, output)),
        }
    })
    .collect();
    let git = command_output(&["git", "--version"]);
    let github = command_output(&["gh", "auth", "status"]);
    let fractal = std::env::current_exe()
        .ok()
        .and_then(|path| command_output_path(&path, &["login", "--status"]));
    ReadinessResponse {
        agents,
        git_installed: git.as_ref().is_some_and(|(success, _)| *success),
        github_cli_installed: github.is_some(),
        github_authenticated: github.as_ref().is_some_and(|(success, _)| *success),
        fractal_society_authenticated: fractal.as_ref().is_some_and(|(success, _)| *success),
        fractal_society_account: fractal
            .filter(|(success, _)| *success)
            .and_then(|(_, output)| society_account(&output)),
    }
}

fn command_output(command: &[&str]) -> Option<(bool, String)> {
    let executable = command.first()?;
    command_output_path(Path::new(executable), &command[1..])
}

fn command_output_path(executable: &Path, arguments: &[&str]) -> Option<(bool, String)> {
    let output = Command::new(executable).args(arguments).output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some((output.status.success(), text))
}

fn agent_authenticated(id: &str, output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    match id {
        "codex" => output.contains("logged in"),
        "cursor" => output.contains("logged in") || output.contains("authenticated"),
        "claude" => output.contains("\"loggedin\": true") || output.contains("\"loggedin\":true"),
        "hermes" => {
            output.contains("logged in")
                || output.contains("authenticated")
                || output.contains("api key") && output.contains('✓')
        }
        _ => false,
    }
}

fn society_account(output: &str) -> Option<String> {
    let (_, suffix) = output.split_once(" as @")?;
    let username: String = suffix
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || *character == '-' || *character == '_'
        })
        .collect();
    (!username.is_empty()).then(|| format!("@{username}"))
}

struct HttpRequest {
    method: String,
    path: String,
    bearer: Option<String>,
    origin: Option<String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).context("read bridge request")?;
        if count == 0 {
            bail!("connection closed before headers");
        }
        data.extend_from_slice(&buffer[..count]);
        if data.len() > MAX_HEADERS {
            bail!("bridge request headers are too large");
        }
        if let Some(index) = find_bytes(&data, b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = std::str::from_utf8(&data[..header_end]).context("headers are not UTF-8")?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().context("missing request line")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().context("missing method")?.to_owned();
    let path = request_parts.next().context("missing path")?.to_owned();
    if request_parts.next() != Some("HTTP/1.1") {
        bail!("only HTTP/1.1 is supported");
    }
    let mut content_length = 0_usize;
    let mut bearer = None;
    let mut origin = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("malformed header")?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().context("invalid content length")?;
        } else if name.eq_ignore_ascii_case("authorization") {
            bearer = value.strip_prefix("Bearer ").map(str::to_owned);
        } else if name.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_owned());
        }
    }
    if content_length > MAX_BODY {
        bail!("bridge request body is too large");
    }
    while data.len() - header_end < content_length {
        let count = stream.read(&mut buffer).context("read bridge body")?;
        if count == 0 {
            bail!("connection closed before body");
        }
        data.extend_from_slice(&buffer[..count]);
    }
    Ok(HttpRequest {
        method,
        path,
        bearer,
        origin,
        body: data[header_end..header_end + content_length].to_vec(),
    })
}

fn write_json(stream: &mut TcpStream, status: u16, value: &impl Serialize) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

fn bridge_root() -> Result<PathBuf> {
    let root = std::env::var_os("FRACTAL_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".fractal")))
        .context("set FRACTAL_HOME or HOME")?;
    let root = root.join("bridge");
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    Ok(root)
}

fn config_path() -> Result<PathBuf> {
    Ok(bridge_root()?.join("config.json"))
}

fn ensure_config(port: u16) -> Result<BridgeConfig> {
    let path = config_path()?;
    if path.is_file() {
        let config: BridgeConfig = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("decode {}", path.display()))?;
        if config.schema != SCHEMA || config.token.len() < 48 {
            bail!("Fractal bridge configuration is malformed");
        }
        return Ok(config);
    }
    let config = BridgeConfig {
        schema: SCHEMA.to_owned(),
        token: generate_token()?,
        port,
    };
    save_config(&config)?;
    Ok(config)
}

fn save_config(config: &BridgeConfig) -> Result<()> {
    let path = config_path()?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("write {}", path.display()))?;
    file.write_all(&serde_json::to_vec_pretty(config)?)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn generate_token() -> Result<String> {
    let mut random = [0_u8; 32];
    OpenOptions::new()
        .read(true)
        .open("/dev/urandom")
        .context("open system random source")?
        .read_exact(&mut random)
        .context("read system random source")?;
    let mut hasher = Sha256::new();
    hasher.update(random);
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(*left.get(index).unwrap_or(&0) ^ *right.get(index).unwrap_or(&0));
    }
    difference == 0
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_token_check_requires_exact_value() {
        assert!(constant_time_equal("abc", "abc"));
        assert!(!constant_time_equal("abc", "abd"));
        assert!(!constant_time_equal("abc", "abcd"));
    }

    #[test]
    fn browser_origins_are_detected_by_request_parser() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(
                    b"POST /v1/build HTTP/1.1\r\nOrigin: https://evil.test\r\nContent-Length: 2\r\n\r\n{}",
                )
                .unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream).unwrap();
        writer.join().unwrap();
        assert_eq!(request.origin.as_deref(), Some("https://evil.test"));
        assert_eq!(request.body, b"{}");
    }

    #[test]
    fn xml_escape_protects_launch_agent_values() {
        assert_eq!(xml_escape("a&<\"'"), "a&amp;&lt;&quot;&apos;");
    }
}
