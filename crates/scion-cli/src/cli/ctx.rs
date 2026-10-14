//! Per-invocation context: environment, endpoint and credential resolution, the cached client
//! and output (SPEC 6.4, 12.2, 17; PLAN 6.15).

use super::args::Global;
use super::error::{CliResult, local};
use crate::output::envelope;
use scion_config::{ClientConfig, Env, WcConfig, parse_client, parse_wc, resolve_endpoint};
use scion_core::{ErrorCode, Result as ScionResult};
use scion_proto::{Endpoint, ScionClient, TlsOptions, error_from_status, v1};
use scion_wc::credentials::{self, Credentials};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// `SCION_TOKEN` (SPEC 6.4): overrides the credentials file.
pub(crate) const ENV_TOKEN: &str = "SCION_TOKEN";
/// CA bundle for HTTPS endpoints (same variable as `scion-import`).
pub(crate) const ENV_CA_FILE: &str = "SCION_CA_FILE";
/// Commit author override.
pub(crate) const ENV_AUTHOR: &str = "SCION_AUTHOR";
/// Service secret for `login --service`.
pub(crate) const ENV_SERVICE_SECRET: &str = "SCION_SERVICE_SECRET";
/// Log filter (tracing-subscriber env-filter syntax) for diagnostics on stderr.
pub(crate) const ENV_LOG: &str = "SCION_LOG";

/// Name of the working-copy directory (SPEC 11.1).
pub(crate) const SCION_DIR: &str = ".scion";

/// The output of a successful (or conflict-paused) command.
pub(crate) struct Reply {
    /// The full `--json` envelope.
    pub json: serde_json::Value,
    /// Human output.
    pub human: Vec<u8>,
    /// Exit code (0, or 3 for a rebase paused on conflicts).
    pub exit: i32,
}

impl Reply {
    /// A reply for `cmd` with `data` and human text.
    pub(crate) fn new<T: Serialize>(
        cmd: &str,
        data: T,
        human: impl Into<Vec<u8>>,
    ) -> CliResult<Self> {
        let json = serde_json::to_value(envelope(cmd, data))
            .map_err(|e| local(format!("cannot serialize the --json output: {e}")))?;
        Ok(Reply {
            json,
            human: human.into(),
            exit: super::error::EXIT_OK,
        })
    }

    /// Override the exit code.
    pub(crate) fn with_exit(mut self, code: i32) -> Self {
        self.exit = code;
        self
    }
}

/// Where the token came from.
#[derive(Clone, Debug, PartialEq)]
enum TokenSource {
    /// Not resolved yet.
    Unresolved,
    /// `SCION_TOKEN`.
    Env(String),
    /// The credentials file.
    File(Credentials),
    /// No token at all (anonymous).
    None,
}

/// Per-invocation state.
pub(crate) struct Ctx<'a> {
    pub env: &'a dyn Env,
    pub global: Global,
    cwd: PathBuf,
    token: TokenSource,
    client: Option<(Option<PathBuf>, ScionClient, String)>,
    reexchanged: bool,
}

/// `<root>/.scion/config.toml`, if present.
fn read_wc_config(root: &Path, env: &dyn Env) -> CliResult<Option<WcConfig>> {
    let p = root.join(SCION_DIR).join("config.toml");
    match std::fs::read_to_string(&p) {
        Ok(text) => parse_wc(&text, env)
            .map(Some)
            .map_err(|e| local(format!("{}: {e}", p.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(local(format!("cannot read {}: {e}", p.display()))),
    }
}

/// TOML basic string (JSON string escapes are valid TOML escapes).
fn toml_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Write `<root>/.scion/config.toml` (SPEC 11.1; PLAN 6.15: `endpoint`, `tracking_ref`).
pub(crate) fn write_wc_config(root: &Path, cfg: &WcConfig) -> CliResult<()> {
    let mut text = String::from("# Scion working-copy configuration (SPEC 11.1).\n");
    if let Some(e) = &cfg.endpoint {
        text.push_str(&format!("endpoint = {}\n", toml_string(e)));
    }
    if let Some(r) = &cfg.tracking_ref {
        text.push_str(&format!("tracking_ref = {}\n", toml_string(r)));
    }
    let p = root.join(SCION_DIR).join("config.toml");
    std::fs::write(&p, text).map_err(|e| local(format!("cannot write {}: {e}", p.display())))
}

impl<'a> Ctx<'a> {
    /// A context for one invocation.
    pub(crate) fn new(env: &'a dyn Env, global: Global) -> CliResult<Self> {
        let base = std::env::current_dir()
            .map_err(|e| local(format!("cannot determine the current directory: {e}")))?;
        let cwd = match &global.dir {
            Some(d) if d.is_absolute() => d.clone(),
            Some(d) => base.join(d),
            None => base,
        };
        Ok(Ctx {
            env,
            global,
            cwd,
            token: TokenSource::Unresolved,
            client: None,
            reexchanged: false,
        })
    }

    /// A non-empty environment variable.
    pub(crate) fn var(&self, name: &str) -> Option<String> {
        self.env.var(name).filter(|v| !v.is_empty())
    }

    /// The effective current directory (`-C`).
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// `~/.config/scion` (SPEC 6.4).
    pub(crate) fn config_dir(&self) -> Option<PathBuf> {
        self.var("HOME")
            .map(|h| PathBuf::from(h).join(".config").join("scion"))
    }

    /// `~/.config/scion/credentials` (SPEC 6.4).
    pub(crate) fn credentials_path(&self) -> CliResult<PathBuf> {
        self.config_dir()
            .map(|d| d.join("credentials"))
            .ok_or_else(|| local("HOME is not set; cannot locate ~/.config/scion/credentials"))
    }

    /// The working-copy root containing the current directory, if any (SPEC 11.1).
    pub(crate) fn find_root(&self) -> Option<PathBuf> {
        self.cwd
            .ancestors()
            .find(|a| a.join(SCION_DIR).join("state").is_file())
            .map(Path::to_path_buf)
    }

    /// The working-copy root, or an error outside a working copy.
    pub(crate) fn require_root(&self) -> CliResult<PathBuf> {
        self.find_root().ok_or_else(|| {
            local(format!(
                "not inside a scion working copy ({} has no .scion directory above it)",
                self.cwd.display()
            ))
        })
    }

    /// `~/.config/scion/config.toml`, if present.
    fn user_config(&self) -> CliResult<Option<ClientConfig>> {
        let Some(dir) = self.config_dir() else {
            return Ok(None);
        };
        let p = dir.join("config.toml");
        match std::fs::read_to_string(&p) {
            Ok(text) => parse_client(&text, self.env)
                .map(Some)
                .map_err(|e| local(format!("{}: {e}", p.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(local(format!("cannot read {}: {e}", p.display()))),
        }
    }

    /// Endpoint precedence (SPEC 6.4): flag > `SCION_ENDPOINT` > `<root>/.scion/config.toml`
    /// > `~/.config/scion/config.toml`.
    pub(crate) fn endpoint(&self, root: Option<&Path>) -> CliResult<String> {
        let wc = match root {
            Some(r) => read_wc_config(r, self.env)?,
            None => None,
        };
        let user = self.user_config()?;
        resolve_endpoint(
            self.global.endpoint.as_deref(),
            self.env,
            wc.as_ref(),
            user.as_ref(),
        )
        .ok_or_else(|| {
            local(
                "no endpoint configured: pass --endpoint, set SCION_ENDPOINT, or set `endpoint` \
                 in ~/.config/scion/config.toml",
            )
        })
    }

    /// TLS options from `--ca-file` / `SCION_CA_FILE` (SPEC 17).
    fn tls(&self) -> CliResult<Option<TlsOptions>> {
        let path = self
            .global
            .ca_file
            .clone()
            .or_else(|| self.var(ENV_CA_FILE).map(PathBuf::from));
        match path {
            None => Ok(None),
            Some(p) => {
                let pem = std::fs::read(&p)
                    .map_err(|e| local(format!("cannot read CA file {}: {e}", p.display())))?;
                Ok(Some(TlsOptions {
                    ca_pem: Some(pem),
                    domain: None,
                }))
            }
        }
    }

    /// The token: `SCION_TOKEN`, else the credentials file (refused when group/world
    /// readable, SPEC 17), else none.
    fn token(&mut self) -> CliResult<Option<String>> {
        if self.token == TokenSource::Unresolved {
            self.token = if let Some(t) = self.var(ENV_TOKEN) {
                TokenSource::Env(t)
            } else {
                match self.config_dir() {
                    None => TokenSource::None,
                    Some(d) => match credentials::read(&d.join("credentials")) {
                        Ok(c) => TokenSource::File(c),
                        Err(e) if e.code == ErrorCode::NotFound => TokenSource::None,
                        Err(e) => return Err(e.into()),
                    },
                }
            };
        }
        Ok(match &self.token {
            TokenSource::Env(t) => Some(t.clone()),
            TokenSource::File(c) => Some(c.token.clone()),
            TokenSource::None | TokenSource::Unresolved => None,
        })
    }

    /// Connect to the endpoint for `root` with `token`.
    pub(crate) async fn connect_with(
        &self,
        root: Option<&Path>,
        token: Option<String>,
    ) -> CliResult<(ScionClient, String)> {
        let ep_text = self.endpoint(root)?;
        let ep = Endpoint::parse(&ep_text)?;
        let tls = self.tls()?;
        let client = ScionClient::connect(&ep, tls, token).await?;
        Ok((client, ep_text))
    }

    /// The authenticated client for `root` (cached for the invocation).
    pub(crate) async fn client(&mut self, root: Option<&Path>) -> CliResult<ScionClient> {
        Ok(self.client_and_endpoint(root).await?.0)
    }

    /// The authenticated client for `root` and its endpoint text.
    pub(crate) async fn client_and_endpoint(
        &mut self,
        root: Option<&Path>,
    ) -> CliResult<(ScionClient, String)> {
        if let Some((r, c, e)) = &self.client
            && r.as_deref() == root
        {
            return Ok((c.clone(), e.clone()));
        }
        let token = self.token()?;
        let (c, e) = self.connect_with(root, token).await?;
        self.client = Some((root.map(Path::to_path_buf), c.clone(), e.clone()));
        Ok((c, e))
    }

    /// Whether an UNAUTHENTICATED failure may be retried after a silent re-exchange of the
    /// stored GitHub token (SPEC 12.2).
    pub(crate) fn can_reexchange(&self) -> bool {
        !self.reexchanged && matches!(&self.token, TokenSource::File(c) if c.github_token.is_some())
    }

    /// Exchange the stored GitHub token for a fresh Scion token, store it (0600) and drop the
    /// cached client (SPEC 12.2: "re-exchange silently on expiry").
    pub(crate) async fn reexchange(&mut self, root: Option<&Path>) -> CliResult<()> {
        self.reexchanged = true;
        let TokenSource::File(creds) = self.token.clone() else {
            return Err(local("no stored GitHub token to re-exchange"));
        };
        let Some(gh) = creds.github_token.clone() else {
            return Err(local("no stored GitHub token to re-exchange"));
        };
        let (anon, _) = self.connect_with(root, None).await?;
        let resp = github_login(&anon, &gh).await?;
        let fresh = Credentials {
            token: resp.token,
            github_token: Some(gh),
        };
        credentials::write(&self.credentials_path()?, &fresh)?;
        tracing::info!(principal = %resp.principal, "re-exchanged the stored GitHub token");
        self.token = TokenSource::File(fresh);
        self.client = None;
        Ok(())
    }
}

/// `Login{github_access_token}` (SPEC 12.2 step 2).
pub(crate) async fn github_login(client: &ScionClient, gh: &str) -> ScionResult<v1::LoginResponse> {
    client
        .auth()
        .login(v1::LoginRequest {
            credential: Some(v1::login_request::Credential::GithubAccessToken(
                gh.to_owned(),
            )),
        })
        .await
        .map(tonic::Response::into_inner)
        .map_err(|s| error_from_status(&s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use scion_config::MapEnv;

    #[test]
    fn wc_config_round_trips_through_parse_wc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(SCION_DIR)).unwrap();
        let cfg = WcConfig {
            endpoint: Some("https://h:1\"x\\".into()),
            tracking_ref: Some("main".into()),
        };
        write_wc_config(dir.path(), &cfg).unwrap();
        let env = MapEnv::default();
        assert_eq!(read_wc_config(dir.path(), &env).unwrap(), Some(cfg));
    }

    #[test]
    fn endpoint_precedence() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/scion")).unwrap();
        std::fs::write(
            home.path().join(".config/scion/config.toml"),
            "endpoint = \"unix:///user.sock\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.path().join(SCION_DIR)).unwrap();
        let home_s = home.path().to_str().unwrap().to_owned();
        let env = MapEnv::from_pairs([("HOME", home_s.as_str())]);
        let ctx = Ctx::new(&env, Global::default()).unwrap();
        assert_eq!(
            ctx.endpoint(Some(root.path())).unwrap(),
            "unix:///user.sock"
        );
        write_wc_config(
            root.path(),
            &WcConfig {
                endpoint: Some("unix:///wc.sock".into()),
                tracking_ref: None,
            },
        )
        .unwrap();
        assert_eq!(ctx.endpoint(Some(root.path())).unwrap(), "unix:///wc.sock");
        assert_eq!(ctx.endpoint(None).unwrap(), "unix:///user.sock");
        let env2 = MapEnv::from_pairs([
            ("HOME", home_s.as_str()),
            ("SCION_ENDPOINT", "unix:///env.sock"),
        ]);
        let ctx = Ctx::new(&env2, Global::default()).unwrap();
        assert_eq!(ctx.endpoint(Some(root.path())).unwrap(), "unix:///env.sock");
        let g = Global {
            endpoint: Some("unix:///flag.sock".into()),
            ..Global::default()
        };
        let ctx = Ctx::new(&env2, g).unwrap();
        assert_eq!(
            ctx.endpoint(Some(root.path())).unwrap(),
            "unix:///flag.sock"
        );
        let empty = MapEnv::default();
        let ctx = Ctx::new(&empty, Global::default()).unwrap();
        assert!(ctx.endpoint(None).is_err());
    }

    #[test]
    fn scion_token_overrides_credentials_and_bad_modes_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let home_s = home.path().to_str().unwrap().to_owned();
        let path = home.path().join(".config/scion/credentials");
        credentials::write(
            &path,
            &Credentials {
                token: "from-file".into(),
                github_token: Some("gh".into()),
            },
        )
        .unwrap();
        let env = MapEnv::from_pairs([("HOME", home_s.as_str())]);
        let mut ctx = Ctx::new(&env, Global::default()).unwrap();
        assert_eq!(ctx.token().unwrap().as_deref(), Some("from-file"));
        assert!(ctx.can_reexchange());
        let env = MapEnv::from_pairs([("HOME", home_s.as_str()), (ENV_TOKEN, "from-env")]);
        let mut ctx = Ctx::new(&env, Global::default()).unwrap();
        assert_eq!(ctx.token().unwrap().as_deref(), Some("from-env"));
        assert!(!ctx.can_reexchange());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let env = MapEnv::from_pairs([("HOME", home_s.as_str())]);
        let mut ctx = Ctx::new(&env, Global::default()).unwrap();
        let err = ctx.token().unwrap_err();
        assert_eq!(err.code().0, "PERMISSION_DENIED");
        std::fs::remove_file(&path).unwrap();
        let mut ctx = Ctx::new(&env, Global::default()).unwrap();
        assert_eq!(ctx.token().unwrap(), None);
    }
}
