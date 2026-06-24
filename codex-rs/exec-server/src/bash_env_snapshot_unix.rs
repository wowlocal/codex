use std::collections::HashMap;
use std::fs::Metadata;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use sha2::Digest;
use sha2::Sha256;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::OnceCell;
use tokio::time::timeout;
use uuid::Uuid;

use super::ExecParams;
use super::ExecServerRuntimePaths;
use crate::process_sandbox::PreparedExecRequest;
use crate::process_sandbox::prepare_exec_request;

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CACHE_ENTRIES: usize = 16;
const MAX_ENV_BYTES: usize = 256 * 1024;
const MAX_SCOPE_BYTES: usize = 384 * 1024;
const MAX_PRESERVE_KEYS: usize = 128;
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
type SnapshotCell = OnceCell<Option<Arc<BashEnvSnapshot>>>;

pub(crate) struct BashEnvSnapshotCache {
    entries: Mutex<HashMap<[u8; 32], Arc<SnapshotCell>>>,
    root: Option<TempDir>,
}

struct BashEnvSnapshotRequest {
    key: [u8; 32],
    shell: String,
    cwd: codex_utils_path_uri::PathUri,
    environment: HashMap<String, String>,
    preserve_env_keys: Vec<String>,
    bash_env: String,
}

struct BashEnvSnapshot {
    path: PathBuf,
    bash_env: String,
    completion: String,
    prefix: String,
}

impl Default for BashEnvSnapshotCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            root: tempfile::Builder::new()
                .prefix("codex-exec-server-bash-env-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()
                .ok(),
        }
    }
}

impl BashEnvSnapshotCache {
    pub(crate) async fn prepare_launch(
        &self,
        params: &ExecParams,
        environment: &HashMap<String, String>,
        runtime_paths: Option<&ExecServerRuntimePaths>,
    ) -> (Vec<String>, HashMap<String, String>) {
        let Some(request) = BashEnvSnapshotRequest::new(params, environment) else {
            return fallback(params, environment);
        };
        let preserve_env_keys = request.preserve_env_keys.clone();
        let Some(snapshot) = self.snapshot(params, request, runtime_paths).await else {
            return fallback(params, environment);
        };
        let Some(argv) = wrap_command(&params.argv, &snapshot, &preserve_env_keys) else {
            return fallback(params, environment);
        };
        let mut environment = environment.clone();
        environment.remove("BASH_ENV");
        (argv, environment)
    }

    pub(crate) fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    async fn snapshot(
        &self,
        params: &ExecParams,
        request: BashEnvSnapshotRequest,
        runtime_paths: Option<&ExecServerRuntimePaths>,
    ) -> Option<Arc<BashEnvSnapshot>> {
        self.root.as_ref()?;
        let key = request.key;
        let cell = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cell) = entries.get(&key) {
                Arc::clone(cell)
            } else {
                if entries.len() >= MAX_CACHE_ENTRIES {
                    return None;
                }
                let cell = Arc::new(OnceCell::new());
                entries.insert(key, Arc::clone(&cell));
                cell
            }
        };
        cell.get_or_init(|| async {
            self.capture(params, request, runtime_paths)
                .await
                .map(Arc::new)
        })
        .await
        .clone()
    }

    async fn capture(
        &self,
        params: &ExecParams,
        request: BashEnvSnapshotRequest,
        runtime_paths: Option<&ExecServerRuntimePaths>,
    ) -> Option<BashEnvSnapshot> {
        let nonce = Uuid::new_v4().simple().to_string();
        let prefix = format!("__CODEX_SNAPSHOT_{}", nonce.to_ascii_uppercase());
        let marker = format!("# Codex remote Bash environment snapshot {nonce}");
        let mut capture = params.clone();
        capture.argv = vec![
            request.shell.clone(),
            "-c".to_string(),
            capture_script(&prefix, &marker, &request.preserve_env_keys),
        ];
        capture.cwd = request.cwd;
        capture.tty = false;
        capture.pipe_stdin = false;
        capture.arg0 = None;
        capture.bash_env_snapshot = None;
        let prepared = prepare_exec_request(&capture, request.environment, runtime_paths).ok()?;
        let (status, output) = capture_output(prepared).await?;
        if !status.success() {
            tracing::debug!(%status, "remote Bash environment snapshot capture failed");
            return None;
        }
        let contents = strip_preamble(&output, &marker)?;
        let completion = format!("# Codex remote Bash environment snapshot complete {nonce}");
        if contents.len().checked_add(completion.len() + 2)? > MAX_SNAPSHOT_BYTES {
            return None;
        }
        let root = self.root.as_ref()?;
        let mut temp = tempfile::NamedTempFile::new_in(root.path()).ok()?;
        temp.write_all(contents).ok()?;
        writeln!(temp, "\n{completion}").ok()?;
        temp.as_file().sync_all().ok()?;
        let path = root.path().join(format!("{nonce}.sh"));
        temp.persist(&path).ok()?;
        Some(BashEnvSnapshot {
            path,
            bash_env: request.bash_env,
            completion,
            prefix,
        })
    }
}

impl BashEnvSnapshotRequest {
    fn new(params: &ExecParams, environment: &HashMap<String, String>) -> Option<Self> {
        let snapshot = params.bash_env_snapshot.as_ref()?;
        if params.tty
            || params.pipe_stdin
            || params.arg0.is_some()
            || params.argv.len() < 3
            || params.argv.get(1).map(String::as_str) != Some("-c")
            || !params.cwd.starts_with(&snapshot.workspace_root)
        {
            return None;
        }
        let shell = Path::new(params.argv.first()?);
        let bash_env = Path::new(environment.get("BASH_ENV")?);
        if !shell.is_absolute()
            || shell.file_name()?.to_str()? != "bash"
            || !shell.is_file()
            || !bash_env.is_absolute()
            || !bash_env.is_file()
        {
            return None;
        }
        let mut preserve_env_keys = snapshot.preserve_env_keys.clone();
        if preserve_env_keys.len() > MAX_PRESERVE_KEYS
            || preserve_env_keys
                .iter()
                .any(|key| key.len() > 128 || !valid_variable(key))
        {
            return None;
        }
        preserve_env_keys.retain(|key| key != "BASH_ENV");
        preserve_env_keys.sort_unstable();
        preserve_env_keys.dedup();

        let mut environment_scope = environment
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        environment_scope.sort_unstable();
        let environment_bytes = environment_scope
            .iter()
            .try_fold(0usize, |size, (key, value)| {
                size.checked_add(key.len())?.checked_add(value.len())
            })?;
        if environment_bytes > MAX_ENV_BYTES {
            return None;
        }
        let mut scope = serde_json::to_value((
            unsafe { libc::geteuid() },
            &snapshot.workspace_root,
            &params.cwd,
            shell.to_string_lossy(),
            file_identity(&shell.metadata().ok()?),
            bash_env.to_string_lossy(),
            file_identity(&bash_env.metadata().ok()?),
            environment_scope,
            &params.env_policy,
            &params.sandbox,
            params.enforce_managed_network,
            &params.managed_network,
            &preserve_env_keys,
        ))
        .ok()?;
        sort_json(&mut scope);
        let scope = serde_json::to_vec(&scope).ok()?;
        if scope.len() > MAX_SCOPE_BYTES {
            return None;
        }
        let key = Sha256::digest(scope).into();
        Some(Self {
            key,
            shell: shell.to_string_lossy().into_owned(),
            cwd: params.cwd.clone(),
            environment: environment.clone(),
            preserve_env_keys,
            bash_env: bash_env.to_string_lossy().into_owned(),
        })
    }
}

impl Drop for BashEnvSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn capture_output(prepared: PreparedExecRequest) -> Option<(ExitStatus, Vec<u8>)> {
    timeout(CAPTURE_TIMEOUT, async move {
        let mut command = prepared_command(prepared)?;
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn().ok()?;
        let stdout = child.stdout.take()?;
        let mut output = Vec::new();
        stdout
            .take((MAX_SNAPSHOT_BYTES + 1) as u64)
            .read_to_end(&mut output)
            .await
            .ok()?;
        if output.len() > MAX_SNAPSHOT_BYTES {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        }
        Some((child.wait().await.ok()?, output))
    })
    .await
    .ok()
    .flatten()
}

fn prepared_command(prepared: PreparedExecRequest) -> Option<Command> {
    let (program, args) = prepared.command.split_first()?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(prepared.cwd.as_path())
        .env_clear()
        .envs(prepared.env)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(arg0) = prepared.arg0 {
        command.as_std_mut().arg0(arg0);
    }
    Some(command)
}

fn capture_script(prefix: &str, marker: &str, preserve_env_keys: &[String]) -> String {
    let mut excluded = ":PWD:OLDPWD:BASH_ENV:".to_string();
    for key in preserve_env_keys {
        excluded.push_str(key);
        excluded.push(':');
    }
    for suffix in ["EXCLUDED", "OPTS", "EXPORTS", "ALIASES", "NAME"] {
        excluded.push_str(prefix);
        excluded.push('_');
        excluded.push_str(suffix);
        excluded.push(':');
    }
    r#"unset __CODEX_PREFIX___EXCLUDED __CODEX_PREFIX___OPTS __CODEX_PREFIX___EXPORTS __CODEX_PREFIX___ALIASES __CODEX_PREFIX___NAME
__CODEX_PREFIX___EXCLUDED='__CODEX_EXCLUDED__'
__CODEX_PREFIX___ALIASES=$(alias -p)
unalias -a 2>/dev/null || true
echo '__CODEX_MARKER__'
declare -f
echo ''
__CODEX_PREFIX___OPTS=$(set -o | awk '$2=="on"{print $1}')
[ -z "$__CODEX_PREFIX___OPTS" ] || printf 'set -o %s\n' $__CODEX_PREFIX___OPTS
echo ''
shopt -p || true
echo ''
__CODEX_PREFIX___EXPORTS=$(
  while IFS= read -r __CODEX_PREFIX___NAME; do
    [[ "$__CODEX_PREFIX___EXCLUDED" == *":$__CODEX_PREFIX___NAME:"* ]] && continue
    [[ "$__CODEX_PREFIX___NAME" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || continue
    declare -xp "$__CODEX_PREFIX___NAME" 2>/dev/null || true
  done < <(compgen -e)
)
[ -z "$__CODEX_PREFIX___EXPORTS" ] || printf '%s\n' "$__CODEX_PREFIX___EXPORTS"
echo ''
declare -p BASH_ENV 2>/dev/null || echo 'unset BASH_ENV'
echo ''
[ -z "$__CODEX_PREFIX___ALIASES" ] || printf '%s\n' "$__CODEX_PREFIX___ALIASES"
unset __CODEX_PREFIX___EXCLUDED __CODEX_PREFIX___OPTS __CODEX_PREFIX___EXPORTS __CODEX_PREFIX___ALIASES __CODEX_PREFIX___NAME
"#
        .replace("__CODEX_PREFIX__", prefix)
        .replace("__CODEX_MARKER__", marker)
        .replace("__CODEX_EXCLUDED__", &excluded)
}

fn strip_preamble<'a>(output: &'a [u8], marker: &str) -> Option<&'a [u8]> {
    let marker = marker.as_bytes();
    let start = output
        .windows(marker.len())
        .position(|part| part == marker)?;
    Some(&output[start..])
}

fn wrap_command(
    argv: &[String],
    snapshot: &BashEnvSnapshot,
    preserve_env_keys: &[String],
) -> Option<Vec<String>> {
    let mut script = String::new();
    for (index, key) in preserve_env_keys.iter().enumerate() {
        script.push_str(&format!(
            "{prefix}_{index}_SET=\"${{{key}+x}}\"\n{prefix}_{index}_VALUE=\"${{{key}-}}\"\n",
            prefix = snapshot.prefix,
        ));
    }
    script.push_str("unset BASH_ENV\n");
    script.push_str(&format!(
            "if [ \"$(tail -n 1 -- '{}' 2>/dev/null)\" = '{}' ] && . '{}' >/dev/null 2>&1; then :; else export BASH_ENV='{}'; . \"$BASH_ENV\" >/dev/null 2>&1 || true; fi\n",
            quote(&snapshot.path.to_string_lossy()),
            quote(&snapshot.completion),
            quote(&snapshot.path.to_string_lossy()),
            quote(&snapshot.bash_env),
        ));
    for (index, key) in preserve_env_keys.iter().enumerate() {
        script.push_str(&format!(
                "if [ \"${{{prefix}_{index}_SET}}\" = x ]; then export {key}=\"${{{prefix}_{index}_VALUE}}\"; else unset {key}; fi\nunset {prefix}_{index}_SET {prefix}_{index}_VALUE\n",
                prefix = snapshot.prefix,
            ));
    }
    script.push_str(argv.get(2)?);
    let mut wrapped = vec![argv.first()?.clone(), "-c".to_string(), script];
    wrapped.extend(argv.get(3..)?.iter().cloned());
    Some(wrapped)
}

fn file_identity(metadata: &Metadata) -> (u64, u64, u64, i64, i64, i64, i64, u32) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
        metadata.mode(),
    )
}

fn sort_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => values.iter_mut().for_each(sort_json),
        serde_json::Value::Object(map) => {
            map.values_mut().for_each(sort_json);
            map.sort_keys();
        }
        _ => {}
    }
}

fn valid_variable(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('_') | Some('a'..='z') | Some('A'..='Z'))
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn quote(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

fn fallback(
    params: &ExecParams,
    environment: &HashMap<String, String>,
) -> (Vec<String>, HashMap<String, String>) {
    (params.argv.clone(), environment.clone())
}
