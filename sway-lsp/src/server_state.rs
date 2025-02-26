//! The context or environment in which the language server functions.

use crate::{
    config::{Config, Warnings},
    core::session::{self, ParseResult, Session},
    error::{DirectoryError, DocumentError, LanguageServerError},
    utils::debug,
    utils::keyword_docs::KeywordDocs,
};
use crossbeam_channel::{Receiver, Sender};
use dashmap::DashMap;
use forc_pkg::PackageManifestFile;
use lsp_types::{Diagnostic, Url};
use parking_lot::RwLock;
use std::{
    mem,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;
use tower_lsp::{jsonrpc, Client};

/// `ServerState` is the primary mutable state of the language server
pub struct ServerState {
    pub(crate) client: Option<Client>,
    pub(crate) config: Arc<RwLock<Config>>,
    pub(crate) keyword_docs: Arc<KeywordDocs>,
    pub(crate) sessions: Arc<Sessions>,
    pub(crate) retrigger_compilation: Arc<AtomicBool>,
    pub is_compiling: Arc<AtomicBool>,
    pub(crate) cb_tx: Sender<TaskMessage>,
    pub(crate) cb_rx: Arc<Receiver<TaskMessage>>,
    pub(crate) finished_compilation: Arc<Notify>,
    last_compilation_state: Arc<RwLock<LastCompilationState>>,
}

impl Default for ServerState {
    fn default() -> Self {
        let (cb_tx, cb_rx) = crossbeam_channel::bounded(1);
        let state = ServerState {
            client: None,
            config: Arc::new(RwLock::new(Default::default())),
            keyword_docs: Arc::new(KeywordDocs::new()),
            sessions: Arc::new(Sessions(DashMap::new())),
            retrigger_compilation: Arc::new(AtomicBool::new(false)),
            is_compiling: Arc::new(AtomicBool::new(false)),
            cb_tx,
            cb_rx: Arc::new(cb_rx),
            finished_compilation: Arc::new(Notify::new()),
            last_compilation_state: Arc::new(RwLock::new(LastCompilationState::Uninitialized)),
        };
        // Spawn a new thread dedicated to handling compilation tasks
        state.spawn_compilation_thread();
        state
    }
}

/// `LastCompilationState` represents the state of the last compilation process.
/// It is primarily used for debugging purposes.
#[derive(Debug, PartialEq)]
enum LastCompilationState {
    Success,
    Failed,
    Uninitialized,
}

/// `TaskMessage` represents the set of messages or commands that can be sent to and processed by a worker thread in the compilation environment.
#[derive(Debug)]
pub enum TaskMessage {
    CompilationContext(CompilationContext),
    // A signal to the receiving thread to gracefully terminate its operation.
    Terminate,
}

/// `CompilationContext` encapsulates all the necessary details required by the compilation thread to execute a compilation process.
/// It acts as a container for shared resources and state information relevant to a specific compilation task.
#[derive(Debug, Default)]
pub struct CompilationContext {
    pub session: Option<Arc<Session>>,
    pub uri: Option<Url>,
    pub version: Option<i32>,
}

impl ServerState {
    pub fn new(client: Client) -> ServerState {
        ServerState {
            client: Some(client),
            ..Default::default()
        }
    }

    /// Spawns a new thread dedicated to handling compilation tasks. This thread listens for
    /// `TaskMessage` instances sent over a channel and processes them accordingly.
    ///
    /// This approach allows for asynchronous compilation tasks to be handled in parallel to
    /// the main application flow, improving efficiency and responsiveness.
    pub fn spawn_compilation_thread(&self) {
        let is_compiling = self.is_compiling.clone();
        let retrigger_compilation = self.retrigger_compilation.clone();
        let finished_compilation = self.finished_compilation.clone();
        let rx = self.cb_rx.clone();
        let last_compilation_state = self.last_compilation_state.clone();
        std::thread::spawn(move || {
            while let Ok(msg) = rx.recv() {
                match msg {
                    TaskMessage::CompilationContext(ctx) => {
                        let uri = ctx.uri.as_ref().unwrap().clone();
                        let session = ctx.session.as_ref().unwrap().clone();
                        let mut engines_clone = session.engines.read().clone();

                        if let Some(version) = ctx.version {
                            // Garbage collection is fairly expsensive so we only clear on every 10th keystroke.
                            if version % 10 == 0 {
                                // Call this on the engines clone so we don't clear types that are still in use
                                // and might be needed in the case cancel compilation was triggered.
                                if let Err(err) = session.garbage_collect(&mut engines_clone) {
                                    tracing::error!(
                                        "Unable to perform garbage collection: {}",
                                        err.to_string()
                                    );
                                }
                            }
                        }

                        // Set the is_compiling flag to true so that the wait_for_parsing function knows that we are compiling
                        is_compiling.store(true, Ordering::SeqCst);
                        let mut parse_result = ParseResult::default();
                        match session::parse_project(
                            &uri,
                            &engines_clone,
                            Some(retrigger_compilation.clone()),
                            &mut parse_result,
                        ) {
                            Ok(_) => {
                                mem::swap(&mut *session.engines.write(), &mut engines_clone);
                                session.write_parse_result(&mut parse_result);
                                *last_compilation_state.write() = LastCompilationState::Success;
                            }
                            Err(_err) => {
                                *last_compilation_state.write() = LastCompilationState::Failed;
                            }
                        }

                        // Reset the flags to false
                        is_compiling.store(false, Ordering::SeqCst);
                        retrigger_compilation.store(false, Ordering::SeqCst);

                        // Make sure there isn't any pending compilation work
                        if rx.is_empty() {
                            // finished compilation, notify waiters
                            finished_compilation.notify_waiters();
                        }
                    }
                    TaskMessage::Terminate => {
                        // If we receive a terminate message, we need to exit the thread
                        return;
                    }
                }
            }
        });
    }

    /// Waits asynchronously for the `is_compiling` flag to become false.
    ///
    /// This function checks the state of `is_compiling`, and if it's true,
    /// it awaits on a notification. Once notified, it checks again, repeating
    /// this process until `is_compiling` becomes false.
    pub async fn wait_for_parsing(&self) {
        loop {
            if !self.is_compiling.load(Ordering::SeqCst) {
                // compilation is finished, lets check if there are pending compilation requests.
                if self.cb_rx.is_empty() {
                    // no pending compilation work, safe to break.
                    break;
                }
            }
            // We are still compiling, lets wait to be notified.
            self.finished_compilation.notified().await;
        }
    }

    pub async fn shutdown_server(&self) -> jsonrpc::Result<()> {
        tracing::info!("Shutting Down the Sway Language Server");

        // Drain pending compilation requests
        while self.cb_rx.try_recv().is_ok() {}

        // Set the retrigger_compilation flag to true so that the compilation exits early
        self.retrigger_compilation.store(true, Ordering::SeqCst);
        self.wait_for_parsing().await;

        // Send a terminate message to the compilation thread
        self.cb_tx
            .send(TaskMessage::Terminate)
            .expect("failed to send terminate message");

        let _ = self.sessions.iter().map(|item| {
            let session = item.value();
            session.shutdown();
        });
        Ok(())
    }

    pub(crate) async fn publish_diagnostics(
        &self,
        uri: Url,
        workspace_uri: Url,
        session: Arc<Session>,
    ) {
        let diagnostics = self.diagnostics(&uri, session.clone()).await;
        // Note: Even if the computed diagnostics vec is empty, we still have to push the empty Vec
        // in order to clear former diagnostics. Newly pushed diagnostics always replace previously pushed diagnostics.
        if let Some(client) = self.client.as_ref() {
            client
                .publish_diagnostics(workspace_uri.clone(), diagnostics, None)
                .await;
        }
    }

    async fn diagnostics(&self, uri: &Url, session: Arc<Session>) -> Vec<Diagnostic> {
        let mut diagnostics_to_publish = vec![];
        let config = &self.config.read();
        let tokens = session.token_map().tokens_for_file(uri);
        match config.debug.show_collected_tokens_as_warnings {
            // If collected_tokens_as_warnings is Parsed or Typed,
            // take over the normal error and warning display behavior
            // and instead show the either the parsed or typed tokens as warnings.
            // This is useful for debugging the lsp parser.
            Warnings::Parsed => {
                diagnostics_to_publish = debug::generate_warnings_for_parsed_tokens(tokens)
            }
            Warnings::Typed => {
                diagnostics_to_publish = debug::generate_warnings_for_typed_tokens(tokens)
            }
            Warnings::Default => {
                if let Some(diagnostics) =
                    session.diagnostics.read().get(&PathBuf::from(uri.path()))
                {
                    if config.diagnostic.show_warnings {
                        diagnostics_to_publish.extend(diagnostics.warnings.clone());
                    }
                    if config.diagnostic.show_errors {
                        diagnostics_to_publish.extend(diagnostics.errors.clone());
                    }
                }
            }
        }
        diagnostics_to_publish
    }
}

/// `Sessions` is a collection of [Session]s, each of which represents a project
/// that has been opened in the users workspace.
pub(crate) struct Sessions(DashMap<PathBuf, Arc<Session>>);

impl Sessions {
    async fn init(&self, uri: &Url) -> Result<(), LanguageServerError> {
        let session = Arc::new(Session::new());
        let project_name = session.init(uri).await?;
        self.insert(project_name, session);
        Ok(())
    }

    /// Constructs and returns a tuple of `(Url, Arc<Session>)` from a given workspace URI.
    /// The returned URL represents the temp directory workspace.
    pub(crate) async fn uri_and_session_from_workspace(
        &self,
        workspace_uri: &Url,
    ) -> Result<(Url, Arc<Session>), LanguageServerError> {
        let session = self.url_to_session(workspace_uri).await?;
        let uri = session.sync.workspace_to_temp_url(workspace_uri)?;
        Ok((uri, session))
    }

    async fn url_to_session(&self, uri: &Url) -> Result<Arc<Session>, LanguageServerError> {
        let path = PathBuf::from(uri.path());
        let manifest = PackageManifestFile::from_dir(&path).map_err(|_| {
            DocumentError::ManifestFileNotFound {
                dir: path.to_string_lossy().to_string(),
            }
        })?;

        // strip Forc.toml from the path to get the manifest directory
        let manifest_dir = manifest
            .path()
            .parent()
            .ok_or(DirectoryError::ManifestDirNotFound)?
            .to_path_buf();

        let session = match self.try_get(&manifest_dir).try_unwrap() {
            Some(item) => item.value().clone(),
            None => {
                // If no session can be found, then we need to call init and inserst a new session into the map
                self.init(uri).await?;
                self.try_get(&manifest_dir)
                    .try_unwrap()
                    .map(|item| item.value().clone())
                    .expect("no session found even though it was just inserted into the map")
            }
        };
        Ok(session)
    }
}

impl std::ops::Deref for Sessions {
    type Target = DashMap<PathBuf, Arc<Session>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
