use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    process::{self, Child, Stdio},
    sync::Arc,
};

use anyhow::Result;
use async_std::future;
use futures::channel::mpsc::{channel, Receiver, Sender};
use futures::channel::oneshot::{channel as oneshot_channel, Sender as OneshotSender};
use futures::SinkExt;
use futures::Stream;
use serde::Serialize;

use chromeoxid_types::*;

use crate::cdp::browser_protocol::target::{
    CreateTargetParams, SessionId, SetDiscoverTargetsParams,
};
use crate::conn::Connection;
use crate::handler::Handler;
use crate::page::Page;

/// A [`Browser`] is created when chromeoxid connects to a Chromium instance.
///
/// Browser drives all the events and dispatches to Tabs?
pub struct Browser {
    tabs: Vec<Arc<Page>>,
    /// The `Sender` to send messages to the connection handler that drives the
    /// websocket
    sender: Sender<BrowserMessage>,
    /// How the spawned chromium instance was configured, if any
    config: Option<BrowserConfig>,
    /// The spawned chromium instance
    child: Option<Child>,
    /// The debug web socket url of the chromium instance
    debug_ws_url: String,
}

impl Browser {
    /// Connect to an already running chromium instance via websocket
    pub async fn connect(debug_ws_url: impl Into<String>) -> Result<(Self, Handler)> {
        let debug_ws_url = debug_ws_url.into();
        let conn = Connection::<CdpJsonEventMessage>::connect(&debug_ws_url).await?;

        let (tx, rx) = channel(1);

        let fut = Handler::new(conn, rx);
        let browser = Self {
            tabs: vec![],
            sender: tx,
            config: None,
            child: None,
            debug_ws_url,
        };
        Ok((browser, fut))
    }

    /// Launches a new instance of `chromium` in the background and attaches to
    /// its debug web socket.
    ///
    /// This fails when no chromium executable could be detected.
    ///
    /// This fails if no web socket url could be detected from the child
    /// processes stderr for more than 20 seconds.
    pub async fn launch(config: BrowserConfig) -> Result<(Self, Handler)> {
        // launch a new chromium instance
        let mut child = config.launch()?;

        // extract the ws:
        let get_ws_url = ws_url_from_output(&mut child);

        let dur = Duration::from_secs(20);
        let debug_ws_url = future::timeout(dur, get_ws_url).await?;

        let conn = Connection::<CdpJsonEventMessage>::connect(&debug_ws_url).await?;

        let (tx, rx) = channel(1);

        let fut = Handler::new(conn, rx);

        let browser = Self {
            tabs: Vec::new(),
            sender: tx,
            config: Some(config),
            child: Some(child),
            debug_ws_url,
        };

        Ok((browser, fut))
    }

    pub async fn set_discover_targets(&self) -> Result<()> {
        // discover targets
        // let (discover_tx, discover_rx) = oneshot_channel();

        let cmd = SetDiscoverTargetsParams::new(true);

        let resp = self.execute(cmd).await?;

        Ok(())
    }

    /// Returns the address of the websocket this browser is attached to
    pub fn websocket_address(&self) -> &String {
        &self.debug_ws_url
    }

    /// Create a new page and return a handle to it.
    pub async fn new_page(&self, params: impl Into<CreateTargetParams>) -> Result<Page> {
        let params = params.into();
        let resp = self.execute(params).await?;
        let target_id = resp.result.target_id;
        let (commands, from_commands) = channel(1);

        self.sender
            .clone()
            .send(BrowserMessage::RegisterTab(from_commands))
            .await?;
        Ok(Page::new(target_id, commands).await?)
    }

    pub async fn new_blank_tab(&self) -> anyhow::Result<Page> {
        Ok(self
            .new_page(CreateTargetParams::new("about:blank"))
            .await?)
    }

    /// Call a browser method.
    pub async fn execute<T: Command>(
        &self,
        cmd: T,
    ) -> anyhow::Result<CommandResponse<T::Response>> {
        let (tx, rx) = oneshot_channel();
        let method = cmd.identifier();
        let msg = CommandMessage::new(cmd, tx)?;

        self.sender
            .clone()
            .send(BrowserMessage::Command(msg))
            .await?;
        let resp = rx.await?;

        if let Some(res) = resp.result {
            let result = serde_json::from_value(res)?;
            Ok(CommandResponse {
                id: resp.id,
                result,
                method,
            })
        } else if let Some(err) = resp.error {
            Err(err.into())
        } else {
            Err(anyhow::anyhow!("Empty Response"))
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.kill().expect("!kill");
        }
    }
}

/// Messages used internally to communicate with the connection, which is
/// executed in the the background task.
#[derive(Debug, Serialize)]
pub(crate) struct CommandMessage {
    pub method: Cow<'static, str>,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub params: serde_json::Value,
    #[serde(skip_serializing)]
    pub sender: OneshotSender<Response>,
}

impl CommandMessage {
    pub fn new<C: Command>(cmd: C, sender: OneshotSender<Response>) -> serde_json::Result<Self> {
        Ok(Self {
            method: cmd.identifier(),
            session_id: None,
            params: serde_json::to_value(cmd)?,
            sender,
        })
    }

    pub fn with_session<C: Command>(
        cmd: C,
        sender: OneshotSender<Response>,
        session_id: Option<SessionId>,
    ) -> serde_json::Result<Self> {
        Ok(Self {
            method: cmd.identifier(),
            session_id,
            params: serde_json::to_value(cmd)?,
            sender,
        })
    }
}

impl Method for CommandMessage {
    fn identifier(&self) -> Cow<'static, str> {
        self.method.clone()
    }
}

pub(crate) enum BrowserMessage {
    Command(CommandMessage),
    RegisterTab(Receiver<CommandMessage>),
}

async fn ws_url_from_output(child_process: &mut Child) -> String {
    let stdout = child_process.stderr.take().expect("no stderror");
    let handle = async_std::task::spawn_blocking(|| {
        let mut buf = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            if buf.read_line(&mut line).is_ok() {
                // check for ws in lin
                if let Some(ws) = line.rsplit("listening on ").next() {
                    if ws.starts_with("ws") && ws.contains("devtools/browser") {
                        return ws.trim().to_string();
                    }
                }
            } else {
                line = String::new();
            }
        }
    });
    handle.await
}

impl Stream for Browser {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        unimplemented!()
    }
}

#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// Determines whether to run headless version of the browser. Defaults to
    /// true.
    headless: bool,
    /// Determines whether to run the browser with a sandbox.
    sandbox: bool,
    /// Launch the browser with a specific window width and height.
    window_size: Option<(u32, u32)>,
    /// Launch the browser with a specific debugging port.
    port: u16,
    /// Path for Chrome or Chromium.
    ///
    /// If unspecified, the create will try to automatically detect a suitable
    /// binary.
    executable: std::path::PathBuf,

    /// A list of Chrome extensions to load.
    ///
    /// An extension should be a path to a folder containing the extension code.
    /// CRX files cannot be used directly and must be first extracted.
    ///
    /// Note that Chrome does not support loading extensions in headless-mode.
    /// See https://bugs.chromium.org/p/chromium/issues/detail?id=706008#c5
    extensions: Vec<String>,

    /// Environment variables to set for the Chromium process.
    /// Passes value through to std::process::Command::envs.
    pub process_envs: Option<HashMap<String, String>>,

    /// Data dir for user data
    pub user_data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct BrowserConfigBuilder {
    headless: bool,
    sandbox: bool,
    window_size: Option<(u32, u32)>,
    port: u16,
    executable: Option<PathBuf>,
    extensions: Vec<String>,
    process_envs: Option<HashMap<String, String>>,
    user_data_dir: Option<PathBuf>,
}

impl BrowserConfig {
    pub fn builder() -> BrowserConfigBuilder {
        BrowserConfigBuilder::default()
    }

    pub fn with_executable(path: impl AsRef<Path>) -> Self {
        Self::builder().chrome_executable(path).build().unwrap()
    }
}

impl Default for BrowserConfigBuilder {
    fn default() -> Self {
        Self {
            headless: true,
            sandbox: true,
            window_size: None,
            port: 0,
            executable: None,
            extensions: vec![],
            process_envs: None,
            user_data_dir: None,
        }
    }
}

impl BrowserConfigBuilder {
    pub fn window_size(mut self, width: u32, height: u32) -> Self {
        self.window_size = Some((width, height));
        self
    }

    pub fn no_sandbox(mut self) -> Self {
        self.sandbox = false;
        self
    }

    pub fn with_head(mut self) -> Self {
        self.headless = false;
        self
    }

    pub fn user_data_dir(mut self, data_dir: impl AsRef<Path>) -> Self {
        self.user_data_dir = Some(data_dir.as_ref().to_path_buf());
        self
    }

    pub fn chrome_executable(mut self, path: impl AsRef<Path>) -> Self {
        self.executable = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn extension(mut self, extension: impl Into<String>) -> Self {
        self.extensions.push(extension.into());
        self
    }

    pub fn extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for ext in extensions {
            self.extensions.push(ext.into());
        }
        self
    }

    pub fn env(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.process_envs
            .get_or_insert(HashMap::new())
            .insert(key.into(), val.into());
        self
    }

    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.process_envs
            .get_or_insert(HashMap::new())
            .extend(envs.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    pub fn build(self) -> std::result::Result<BrowserConfig, String> {
        let executable = if let Some(e) = self.executable {
            e
        } else {
            default_executable()?
        };

        Ok(BrowserConfig {
            headless: self.headless,
            sandbox: self.sandbox,
            window_size: self.window_size,
            port: self.port,
            executable,
            extensions: self.extensions,
            process_envs: None,
            user_data_dir: None,
        })
    }
}

impl BrowserConfig {
    pub fn launch(&self) -> io::Result<Child> {
        let dbg_port = format!("--remote-debugging-port={}", self.port);

        let args = [
            dbg_port.as_str(),
            "--disable-gpu",
            "--enable-logging",
            "--verbose",
            "--log-level=0",
            "--no-first-run",
            "--disable-audio-output",
        ];

        let mut cmd = process::Command::new(&self.executable);
        cmd.args(&args).args(&DEFAULT_ARGS).args(
            self.extensions
                .iter()
                .map(|e| format!("--load-extension={}", e)),
        );

        if let Some(ref user_data) = self.user_data_dir {
            cmd.arg(format!("--user-data-dir={}", user_data.display()));
        }

        if let Some((width, height)) = self.window_size.clone() {
            cmd.arg(format!("--window-size={},{}", width, height));
        }

        if !self.sandbox {
            cmd.args(&["--no-sandbox", "--disable-setuid-sandbox"]);
        }

        if self.headless {
            cmd.args(&["--headless", "--hide-scrollbars", "--mute-audio"]);
        }

        if let Some(ref envs) = self.process_envs {
            cmd.envs(envs);
        }
        cmd.stderr(Stdio::piped()).spawn()
    }
}

/// Returns the path to Chrome's executable.
///
/// If the `CHROME` environment variable is set, `default_executable` will
/// use it as the default path. Otherwise, the filenames `google-chrome-stable`
/// `chromium`, `chromium-browser`, `chrome` and `chrome-browser` are
/// searched for in standard places. If that fails,
/// `/Applications/Google Chrome.app/...` (on MacOS) or the registry (on
/// Windows) is consulted. If all of the above fail, an error is returned.
pub fn default_executable() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("CHROME") {
        if std::path::Path::new(&path).exists() {
            return Ok(path.into());
        }
    }

    for app in &[
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
        "chrome-browser",
    ] {
        if let Ok(path) = which::which(app) {
            return Ok(path);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let default_paths = &["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"][..];
        for path in default_paths {
            if std::path::Path::new(path).exists() {
                return Ok(path.into());
            }
        }
    }

    #[cfg(windows)]
    {
        use crate::browser::process::get_chrome_path_from_registry;

        if let Some(path) = get_chrome_path_from_registry() {
            if path.exists() {
                return Ok(path);
            }
        }
    }

    Err("Could not auto detect a chrome executable".to_string())
}

/// These are passed to the Chrome binary by default.
/// Via https://github.com/puppeteer/puppeteer/blob/4846b8723cf20d3551c0d755df394cc5e0c82a94/src/node/Launcher.ts#L157
static DEFAULT_ARGS: [&str; 23] = [
    "--disable-background-networking",
    "--enable-features=NetworkService,NetworkServiceInProcess",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-extensions",
    "--disable-features=TranslateUI",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--enable-automation",
    "--password-store=basic",
    "--use-mock-keychain",
];
