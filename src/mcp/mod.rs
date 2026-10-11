//! Bounded read-only MCP protocol and single-connection stdio transport.
pub mod catalog;
pub mod session;
pub mod tools;
pub mod wire;

use crate::store::topology::WorkspaceIdentity;
use std::os::unix::fs::MetadataExt;

/// An already selected and marker-attached workspace. Never opens an index or repairs a marker.
#[derive(Debug)]
pub struct OpenedWorkspace {
    identity: WorkspaceIdentity,
    label: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    RootChanged,
    StoreUnavailable,
}
impl OpenedWorkspace {
    pub fn new(identity: WorkspaceIdentity) -> Self {
        let label = identity
            .root
            .file_name()
            .map(|name| name.to_str().expect("canonical root is UTF-8").to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "workspace".into());
        Self { identity, label }
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn root(&self) -> &std::path::Path {
        &self.identity.root
    }
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }
    pub fn build_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    pub fn check(&self) -> Result<(), IdentityError> {
        let m = std::fs::symlink_metadata(&self.identity.root)
            .map_err(|_| IdentityError::RootChanged)?;
        if !m.is_dir()
            || m.file_type().is_symlink()
            || (m.dev(), m.ino()) != (self.identity.device, self.identity.inode)
        {
            return Err(IdentityError::RootChanged);
        }
        self.identity
            .verify()
            .map_err(|_| IdentityError::StoreUnavailable)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_guard() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("selected");
        std::fs::create_dir(&root).unwrap();
        let context =
            OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), temp.path()).unwrap());
        assert_eq!(context.check(), Ok(()));
        assert_eq!(context.label(), "selected");
        std::fs::rename(&root, temp.path().join("former")).unwrap();
        assert_eq!(context.check(), Err(IdentityError::RootChanged));
        std::os::unix::fs::symlink(temp.path().join("former"), &root).unwrap();
        assert_eq!(context.check(), Err(IdentityError::RootChanged));
    }
    #[test]
    fn marker_guard_does_not_repair() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("git-workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let context =
            OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), temp.path()).unwrap());
        assert_eq!(context.check(), Ok(()));
        let marker = root.join(".git/trellis/workspace-id");
        std::fs::remove_file(&marker).unwrap();
        assert_eq!(context.check(), Err(IdentityError::StoreUnavailable));
        assert!(!marker.exists());
    }
}

/// Run the line-framed transport. The reader owns stdin; this coordinator alone owns stdout.
/// Input already waiting in the bounded channel is applied before any queued outcome commits.
pub fn run_stdio(workspace: OpenedWorkspace) -> std::io::Result<()> {
    run_transport(workspace, std::io::stdin(), std::io::stdout(), None)
}

/// Run the same protocol coordinator on a daemon-owned session socket.
pub fn run_socket(
    workspace: OpenedWorkspace,
    stream: std::os::unix::net::UnixStream,
    registry: std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    session_id: u64,
) -> std::io::Result<()> {
    let reader = stream.try_clone()?;
    run_transport(workspace, reader, stream, Some((registry, session_id)))
}

fn catching_up(
    registry: &Option<(
        std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
        u64,
    )>,
    key: &str,
) -> bool {
    registry
        .as_ref()
        .and_then(|(registry, _)| registry.blocking_lock().runtime(key))
        .is_none_or(|runtime| runtime.catching_up())
}

/// An opt-in, bounded fixture barrier at an actual coordinator boundary.
/// Normal MCP calls do not connect to or wait for a test socket.
pub(crate) fn phase_hook(stage: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    if std::env::var("TRELLIS_TEST_MCP_PHASE").ok().as_deref() != Some(stage) {
        return Ok(());
    }
    let path = std::env::var_os("TRELLIS_TEST_MCP_PHASE_SOCKET").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing MCP fixture socket",
        )
    })?;
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.write_all(stage.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut release = [0u8; 1];
    stream.read_exact(&mut release)?;
    Ok(())
}

fn run_transport(
    workspace: OpenedWorkspace,
    input: impl std::io::Read + Send + 'static,
    output: impl std::io::Write,
    registry: Option<(
        std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
        u64,
    )>,
) -> std::io::Result<()> {
    use std::collections::VecDeque;
    use std::io::BufReader;
    use std::sync::mpsc::{self, TryRecvError};
    let (sender, receiver) = mpsc::sync_channel::<wire::Frame>(16);
    std::thread::Builder::new()
        .name("mcp-stdin".into())
        .spawn(move || {
            let mut reader = BufReader::new(input);
            loop {
                match wire::read_frame(&mut reader) {
                    Ok(wire::Frame::Eof) => {
                        let _ = sender.send(wire::Frame::Eof);
                        break;
                    }
                    Ok(frame) => {
                        if sender.send(frame).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("MCP stdin read failed: {error}");
                        let _ = sender.send(wire::Frame::Eof);
                        break;
                    }
                }
            }
        })?;
    let mut session = session::Session::new();
    let mut outcomes = VecDeque::new();
    let mut writer = output;
    loop {
        // A blocked coordinator waits for one input event. Once there is pending work,
        // observe all input currently waiting before committing even the first result.
        if outcomes.is_empty() {
            let Ok(frame) = receiver.recv() else {
                session.eof();
                break;
            };
            if receive(frame, &mut session, &mut outcomes) {
                break;
            }
        }
        loop {
            match receiver.try_recv() {
                Ok(frame) => {
                    if receive(frame, &mut session, &mut outcomes) {
                        return Ok(());
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    session.eof();
                    return Ok(());
                }
            }
        }
        let Some(outcome) = outcomes.pop_front() else {
            continue;
        };
        match outcome {
            impl_receive::Outcome::Error(error) => {
                wire::write_response(&mut writer, &error.response())?
            }
            impl_receive::Outcome::Request(admission) => {
                // Cancellation may have invalidated the token while its outcome waited.
                if session.pending(admission.token).is_none() {
                    continue;
                }
                let modern = session.mode() == session::Mode::Modern;
                let mut prepared_tool = None;
                let mut capacity_witness = None;
                let result = match &admission.action {
                    session::Action::Discover => catalog::discover(workspace.build_version()),
                    session::Action::Initialize => catalog::initialize(workspace.build_version()),
                    session::Action::List => catalog::list(modern),
                    session::Action::Call { name, arguments } => {
                        let selection = arguments
                            .as_ref()
                            .and_then(|v| v.as_object())
                            .and_then(|o| o.get("workspace"));
                        let valid = tools::validate(name, arguments.as_ref()).is_ok();
                        if !valid {
                            let tool = tools::prepare(
                                name,
                                arguments.as_ref(),
                                &admission.id,
                                modern,
                                &workspace,
                            );
                            let result = tool.response.clone();
                            prepared_tool = Some((tool, None, false));
                            result
                        } else {
                            let selected = if let Some((registry, session_id)) = &registry {
                                match selection.and_then(serde_json::Value::as_str) {
                                    Some(path) => registry
                                        .blocking_lock()
                                        .select(
                                            *session_id,
                                            workspace.identity(),
                                            std::path::Path::new(path),
                                        )
                                        .map(Some),
                                    None => workspace.check().map(|_| None).map_err(|_| {
                                        crate::daemon::registry::SelectionError::IdentityChanged
                                    }),
                                }
                            } else {
                                workspace.check().map(|_| None).map_err(|_| {
                                    crate::daemon::registry::SelectionError::IdentityChanged
                                })
                            };
                            match selected {
                                Ok(selected) => {
                                    if selected.is_some() {
                                        phase_hook("selected_clone")?;
                                    }
                                    let key = selected
                                        .as_ref()
                                        .map_or(&workspace.identity.root_key, |w| {
                                            &w.identity.root_key
                                        });
                                    let catching_up = catching_up(&registry, key);
                                    let selected_workspace = selected.as_ref().map(|w| {
                                        w.identity.verified_clone().map(OpenedWorkspace::new)
                                    });
                                    match selected_workspace.transpose() {
                                        Err(_) => {
                                            let root = selected
                                                .as_ref()
                                                .map_or(workspace.root(), |witness| {
                                                    witness.identity.root.as_path()
                                                });
                                            tools::resolved_root_changed(
                                                &admission.id,
                                                root,
                                                catching_up,
                                                modern,
                                            )
                                        }
                                        Ok(selected_workspace) => {
                                            let answering =
                                                selected_workspace.as_ref().unwrap_or(&workspace);
                                            let mut tool = tools::prepare(
                                                name,
                                                arguments.as_ref(),
                                                &admission.id,
                                                modern,
                                                answering,
                                            );
                                            let mut envelope =
                                                tool.response["structuredContent"].clone();
                                            envelope["catchingUp"] = serde_json::json!(catching_up);
                                            tool.response = tools::result(envelope, modern);
                                            let result = tool.response.clone();
                                            prepared_tool = Some((tool, selected, catching_up));
                                            result
                                        }
                                    }
                                }
                                Err(crate::daemon::registry::SelectionError::CheckoutCapacity) => {
                                    let Some(path) = selection.and_then(serde_json::Value::as_str)
                                    else {
                                        unreachable!(
                                            "validated capacity requires explicit selection"
                                        )
                                    };
                                    let checked_capacity = {
                                        let guard = registry
                                            .as_ref()
                                            .expect("daemon registry required")
                                            .0
                                            .blocking_lock();
                                        guard.capacity_witness(
                                            workspace.identity(),
                                            std::path::Path::new(path),
                                        )
                                    };
                                    match checked_capacity {
                                        Ok(witness) => {
                                            let catching_up =
                                                catching_up(&registry, &witness.identity.root_key);
                                            let response = tools::result(
                                                tools::attributed_root(
                                                    tools::failure(
                                                        &admission.id,
                                                        "checkout_capacity",
                                                    ),
                                                    &witness.identity.root,
                                                    catching_up,
                                                ),
                                                modern,
                                            );
                                            capacity_witness = Some((witness, catching_up));
                                            response
                                        }
                                        Err(error) => tools::result(
                                            tools::selection_failure(
                                                &admission.id,
                                                error,
                                                &serde_json::json!(path),
                                            ),
                                            modern,
                                        ),
                                    }
                                }
                                Err(error) => {
                                    let attempted = selection
                                        .cloned()
                                        .unwrap_or_else(|| serde_json::json!(workspace.root()));
                                    tools::result(
                                        tools::selection_failure(&admission.id, error, &attempted),
                                        modern,
                                    )
                                }
                            }
                        }
                    }
                    session::Action::Unknown => {
                        wire::ProtocolError::new(-32601, admission.id.clone()).response()
                    }
                };
                let response = if matches!(admission.action, session::Action::Unknown) {
                    result
                } else {
                    serde_json::json!({"jsonrpc":"2.0","id":admission.id,"result":result})
                };
                let Some(mut prepared) = session.prepare(admission.token, response) else {
                    continue;
                };
                // A final nonblocking input pass takes precedence over the unsent result.
                loop {
                    match receiver.try_recv() {
                        Ok(frame) => {
                            if receive(frame, &mut session, &mut outcomes) {
                                return Ok(());
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => {
                            session.eof();
                            return Ok(());
                        }
                    }
                }
                if let Some((mut tool, selected, catching_up)) = prepared_tool {
                    if tool.valid_arguments {
                        phase_hook(if selected.is_some() {
                            "selected_final"
                        } else {
                            "launch_final"
                        })?;
                        let changed = selected.as_ref().map_or_else(
                            || workspace.check().is_err(),
                            |witness| witness.before_answer().is_err(),
                        );
                        if changed {
                            let root = selected.as_ref().map_or(workspace.root(), |witness| {
                                witness.identity.root.as_path()
                            });
                            tool.response = tools::resolved_root_changed(
                                &admission.id,
                                root,
                                catching_up,
                                modern,
                            );
                        }
                    }
                    prepared.response["result"] = tool.response;
                }
                if let Some((witness, catching_up)) = capacity_witness {
                    phase_hook("capacity_final")?;
                    if witness.before_answer().is_err() {
                        prepared.response["result"] = tools::resolved_root_changed(
                            &admission.id,
                            &witness.identity.root,
                            catching_up,
                            modern,
                        );
                    }
                }
                if let Some(value) = session.commit(prepared) {
                    wire::write_response(&mut writer, &value)?;
                }
            }
        }
    }
    Ok(())
}

fn receive(
    frame: wire::Frame,
    session: &mut session::Session,
    outcomes: &mut std::collections::VecDeque<impl_receive::Outcome>,
) -> bool {
    if frame == wire::Frame::Eof {
        session.eof();
        return true;
    }
    match wire::decode(frame) {
        Err(error) => outcomes.push_back(impl_receive::Outcome::Error(error)),
        Ok(None) => {
            session.eof();
            return true;
        }
        Ok(Some(request)) => match session.accept(request, catalog::known) {
            session::Event::Error(error) => outcomes.push_back(impl_receive::Outcome::Error(error)),
            session::Event::Admitted(admission) => {
                outcomes.push_back(impl_receive::Outcome::Request(admission))
            }
            session::Event::Ignore => (),
        },
    }
    false
}
mod impl_receive {
    pub(super) enum Outcome {
        Error(super::wire::ProtocolError),
        Request(super::session::Admission),
    }
}
