//! quiet-ui: the work a thread does outside its own turn (commands still
//! running, async tasks, subagents) and the failures a session reports,
//! which the sidebar row and the thread's chips read.

use super::*;

/// Work outside the agent's own turn. Services (dev servers and the like) are
/// counted apart: they never finish, so they are not work anything waits on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunningWork {
    pub terminals: usize,
    pub subagents: usize,
    /// Detached commands, workflows and tasks, services excepted.
    pub async_tasks: usize,
    pub workflows: usize,
    pub other_tasks: usize,
    pub services: usize,
}

impl RunningWork {
    /// Whether anything the agent will wake up for is still running.
    pub fn is_empty(&self) -> bool {
        self.terminals == 0
            && self.subagents == 0
            && self.async_tasks == 0
            && self.workflows == 0
            && self.other_tasks == 0
    }
}

/// Why a session stopped, as the agent reported it. The Claude adapter
/// publishes this under the AIR `sessionFailure` capability; the payload
/// carries a category rather than its own finer `kind`, so this is what a row
/// can actually say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFailure {
    pub id: SharedString,
    /// Rises with each report of the same failure; a stale one is dropped.
    pub revision: u64,
    pub category: SessionFailureCategory,
    /// A sentence, for a tooltip rather than a row.
    pub title: SharedString,
    pub details: Option<SharedString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionFailureCategory {
    Connection,
    Access,
    Limit,
    Request,
    Service,
    Unknown,
}

impl SessionFailureCategory {
    pub fn from_wire(category: &str) -> Self {
        match category {
            "connection" => Self::Connection,
            "access" => Self::Access,
            "limit" => Self::Limit,
            "request" => Self::Request,
            "service" => Self::Service,
            _ => Self::Unknown,
        }
    }
}

/// An unknown state is treated as running: a task counted too long is better
/// than one that vanishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncTaskState {
    Running,
    Paused,
    Completed,
    Failed,
    Stopped,
}

impl AsyncTaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }

    pub fn from_wire(state: &str) -> Self {
        match state {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "stopped" => Self::Stopped,
            "paused" => Self::Paused,
            _ => Self::Running,
        }
    }
}

/// What the agent says an [`AsyncTask`] is, from its `taskType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsyncTaskKind {
    Command,
    Workflow,
    Task,
}

impl AsyncTaskKind {
    /// Claude's are `local_bash`, `local_workflow` and the agent kinds; a task
    /// that names no type is the command it has always been.
    pub fn from_wire(task_type: Option<&str>) -> Self {
        match task_type {
            None => Self::Command,
            Some(task_type) if task_type.contains("bash") || task_type.contains("shell") => {
                Self::Command
            }
            Some(task_type) if task_type.contains("workflow") => Self::Workflow,
            Some(_) => Self::Task,
        }
    }

    pub fn noun(&self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Workflow => "workflow",
            Self::Task => "task",
        }
    }
}

/// Work the agent runs outside its turn, such as Claude's `run_in_background`
/// commands. The tool call that started it completes on hand-off, so the
/// lifecycle arrives separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsyncTask {
    pub id: SharedString,
    pub name: SharedString,
    pub description: Option<SharedString>,
    pub kind: AsyncTaskKind,
    pub state: AsyncTaskState,
    pub summary: Option<SharedString>,
    /// Can arrive after the task itself.
    pub tool_call_id: Option<acp_v2::ToolCallId>,
    pub output_file_path: Option<SharedString>,
    pub can_stop: bool,
    pub started_at: Instant,
    /// Set once its output shows it serving on a port: a service, which
    /// nothing waits on.
    pub service_port: Option<u16>,
}

impl AsyncTask {
    pub fn is_service(&self) -> bool {
        self.service_port.is_some()
    }
}

/// A subagent as the adapter reports its lifecycle. Claude's `Agent` call
/// completes on hand-off, so the call's status says nothing about whether the
/// subagent is still working.
#[derive(Debug, Clone)]
pub struct Subagent {
    pub session_id: acp_v2::SessionId,
    pub name: Option<SharedString>,
    pub task: Option<SharedString>,
    pub state: AsyncTaskState,
}

impl AcpThread {
    pub fn session_failure(&self) -> Option<&SessionFailure> {
        self.session_failure.as_ref()
    }

    /// The agent reporting why it stopped. A report of the same failure with a
    /// revision no higher than the one in hand is a replay and is dropped.
    pub fn report_session_failure(&mut self, failure: SessionFailure, cx: &mut Context<Self>) {
        if let Some(current) = &self.session_failure
            && current.id == failure.id
            && current.revision >= failure.revision
        {
            return;
        }
        self.session_failure = Some(failure);
        self.session_warning = None;
        self.stop_background_work(cx);
        cx.emit(AcpThreadEvent::StopReasonChanged);
        cx.notify();
    }

    pub(crate) fn clear_session_failure(&mut self, cx: &mut Context<Self>) {
        if self.session_failure.take().is_some() {
            cx.emit(AcpThreadEvent::StopReasonChanged);
            cx.notify();
        }
    }

    pub fn session_warning(&self) -> Option<&SharedString> {
        self.session_warning.as_ref()
    }

    pub fn report_session_warning(&mut self, warning: SharedString, cx: &mut Context<Self>) {
        if self.session_warning.as_ref() != Some(&warning) {
            self.session_warning = Some(warning);
            cx.emit(AcpThreadEvent::StopReasonChanged);
            cx.notify();
        }
    }

    pub(crate) fn clear_session_warning(&mut self, cx: &mut Context<Self>) {
        if self.session_warning.take().is_some() {
            cx.emit(AcpThreadEvent::StopReasonChanged);
            cx.notify();
        }
    }

    /// Whether this call is still running, for the chip's spinner and the
    /// sidebar row alike: both ask this, so a chip that spins always has a row
    /// that says working.
    ///
    /// Replayed history and a call a dead agent or a cancel left `InProgress`
    /// both read in flight forever, so outside a turn the evidence has to be
    /// the agent saying it detached the call, a reported lifecycle, or a
    /// process of ours still running under it.
    pub fn tool_call_is_running(&self, call: &ToolCall, cx: &App) -> bool {
        if let Some(state) = self.subagent_state_for_tool_call(call) {
            return !state.is_terminal();
        }
        if self.tool_call_is_backgrounded(&call.id) {
            return true;
        }
        if !matches!(
            call.status(),
            ToolCallStatus::Pending | ToolCallStatus::InProgress
        ) {
            return false;
        }
        !matches!(self.foreground_activity(), ForegroundActivity::Idle)
            || call.terminals().any(|terminal| {
                let terminal = terminal.read(cx);
                terminal.is_process_backed() && terminal.output().is_none()
            })
    }

    /// `cx.notify()` reaches this thread's own views and nothing else. The
    /// sidebar row watches the panel's events, so every input to
    /// [`Self::running_work`] has to say so as an event too.
    pub(crate) fn running_work_changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(AcpThreadEvent::RunningWorkChanged);
        cx.notify();
    }

    /// What this thread is running right now. Only work with evidence it is
    /// alive counts; replayed history counts for nothing.
    pub fn running_work(&self, cx: &App) -> RunningWork {
        let mut work = RunningWork::default();
        for entry in &self.entries {
            let AgentThreadEntry::ToolCall(call) = entry else {
                continue;
            };
            if !self.tool_call_is_running(call, cx) {
                continue;
            }
            work.terminals += call
                .terminals()
                .filter(|terminal| terminal.read(cx).output().is_none())
                .count();
            if call.is_subagent()
                && matches!(call.status(), ToolCallStatus::InProgress)
                && !self.subagent_is_reported(call)
            {
                work.subagents += 1;
            }
        }
        // Reported subagents count whether or not a turn is running.
        work.subagents += self
            .reported_subagents
            .iter()
            .filter(|subagent| !subagent.state.is_terminal())
            .count();
        for task in self
            .async_tasks
            .iter()
            .filter(|task| !task.state.is_terminal())
        {
            let count = match (task.is_service(), task.kind) {
                (true, _) => &mut work.services,
                (false, AsyncTaskKind::Command) => &mut work.async_tasks,
                (false, AsyncTaskKind::Workflow) => &mut work.workflows,
                (false, AsyncTaskKind::Task) => &mut work.other_tasks,
            };
            *count += 1;
        }
        work
    }

    /// The ports of the services this thread's agent started and are still up.
    pub fn service_ports(&self) -> Vec<u16> {
        let mut ports = self
            .async_tasks
            .iter()
            .filter(|task| !task.state.is_terminal())
            .filter_map(|task| task.service_port)
            .collect::<Vec<_>>();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    pub fn set_async_task_service_port(&mut self, id: &str, port: u16, cx: &mut Context<Self>) {
        let Some(task) = self.async_tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        if task.service_port != Some(port) {
            task.service_port = Some(port);
            self.running_work_changed(cx);
        }
    }

    pub fn async_tasks(&self) -> &[AsyncTask] {
        &self.async_tasks
    }

    pub fn reported_subagents(&self) -> &[Subagent] {
        &self.reported_subagents
    }

    pub fn subagent_state_for_tool_call(&self, call: &ToolCall) -> Option<AsyncTaskState> {
        let session_id = &call.subagent_session_info.as_ref()?.session_id;
        self.reported_subagents
            .iter()
            .find(|subagent| &subagent.session_id == session_id)
            .map(|subagent| subagent.state)
    }

    pub(crate) fn subagent_is_reported(&self, call: &ToolCall) -> bool {
        call.subagent_session_info.as_ref().is_some_and(|info| {
            self.reported_subagents
                .iter()
                .any(|subagent| subagent.session_id == info.session_id)
        })
    }

    pub fn async_task_for_tool_call(
        &self,
        tool_call_id: &acp_v2::ToolCallId,
    ) -> Option<&AsyncTask> {
        self.async_tasks.iter().find(|task| {
            !task.state.is_terminal() && task.tool_call_id.as_ref() == Some(tool_call_id)
        })
    }

    /// A backgrounded call reads `completed` while its command runs on.
    pub fn tool_call_is_backgrounded(&self, tool_call_id: &acp_v2::ToolCallId) -> bool {
        self.backgrounded_tool_calls.contains(tool_call_id)
            || self.async_task_for_tool_call(tool_call_id).is_some()
    }

    pub fn mark_tool_call_backgrounded(
        &mut self,
        tool_call_id: acp_v2::ToolCallId,
        cx: &mut Context<Self>,
    ) {
        // A fast command can finish before the update saying it was
        // backgrounded arrives, and nothing would release the marker.
        let already_finished = self.async_task_for_tool_call(&tool_call_id).is_none()
            && self.async_tasks.iter().any(|task| {
                task.state.is_terminal() && task.tool_call_id.as_ref() == Some(&tool_call_id)
            });
        if already_finished {
            return;
        }
        if self.backgrounded_tool_calls.insert(tool_call_id) {
            self.running_work_changed(cx);
        }
    }

    /// When a turn ends, a marker with no live task behind it is stale.
    pub(crate) fn release_orphan_backgrounded_tool_calls(&mut self) {
        let live: HashSet<acp_v2::ToolCallId> = self
            .async_tasks
            .iter()
            .filter(|task| !task.state.is_terminal())
            .filter_map(|task| task.tool_call_id.clone())
            .collect();
        self.backgrounded_tool_calls.retain(|id| live.contains(id));
    }

    /// The agent's process died: whatever it was running died with it, and it
    /// will not say so.
    pub(crate) fn stop_background_work(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for task in &mut self.async_tasks {
            if !task.state.is_terminal() {
                task.state = AsyncTaskState::Stopped;
                changed = true;
            }
        }
        for subagent in &mut self.reported_subagents {
            if !subagent.state.is_terminal() {
                subagent.state = AsyncTaskState::Stopped;
                changed = true;
            }
        }
        if changed || !self.backgrounded_tool_calls.is_empty() {
            self.backgrounded_tool_calls.clear();
            self.running_work_changed(cx);
        }
    }

    pub fn async_task_spawned(&mut self, task: AsyncTask, cx: &mut Context<Self>) {
        match self
            .async_tasks
            .iter_mut()
            .find(|existing| existing.id == task.id)
        {
            // A task can be announced after its own terminal state.
            Some(existing) => {
                let state = existing.state;
                let summary = existing.summary.take();
                let started_at = existing.started_at;
                let service_port = existing.service_port;
                *existing = task;
                existing.started_at = started_at;
                existing.service_port = service_port;
                if state.is_terminal() {
                    existing.state = state;
                    existing.summary = summary;
                }
            }
            None => self.async_tasks.push(task),
        }
        self.settle_backgrounded_tool_calls();
        self.running_work_changed(cx);
    }

    pub fn async_task_progress(
        &mut self,
        id: &str,
        description: Option<SharedString>,
        summary: Option<SharedString>,
        tool_call_id: Option<acp_v2::ToolCallId>,
        output_file_path: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.async_tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        if description.is_some() {
            task.description = description;
        }
        if summary.is_some() {
            task.summary = summary;
        }
        if tool_call_id.is_some() {
            task.tool_call_id = tool_call_id;
        }
        if output_file_path.is_some() {
            task.output_file_path = output_file_path;
        }
        self.settle_backgrounded_tool_calls();
        self.running_work_changed(cx);
    }

    pub fn async_task_state_updated(
        &mut self,
        id: &str,
        state: AsyncTaskState,
        summary: Option<SharedString>,
        tool_call_id: Option<acp_v2::ToolCallId>,
        cx: &mut Context<Self>,
    ) {
        let Some(task) = self.async_tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        task.state = state;
        if summary.is_some() {
            task.summary = summary;
        }
        if tool_call_id.is_some() {
            task.tool_call_id = tool_call_id;
        }
        self.settle_backgrounded_tool_calls();
        self.running_work_changed(cx);
    }

    pub(crate) fn clear_async_tasks(&mut self, cx: &mut Context<Self>) {
        if self.async_tasks.is_empty() && self.backgrounded_tool_calls.is_empty() {
            return;
        }
        self.async_tasks.clear();
        self.backgrounded_tool_calls.clear();
        self.running_work_changed(cx);
    }

    /// The marker never updates, so a task's terminal state releases it.
    pub(crate) fn settle_backgrounded_tool_calls(&mut self) {
        let finished: Vec<acp_v2::ToolCallId> = self
            .async_tasks
            .iter()
            .filter(|task| task.state.is_terminal())
            .filter_map(|task| task.tool_call_id.clone())
            .collect();
        for tool_call_id in finished {
            if self.async_task_for_tool_call(&tool_call_id).is_none() {
                self.backgrounded_tool_calls.remove(&tool_call_id);
            }
        }
    }

    /// A subagent's lifecycle as its agent reports it; its thread, if any, is
    /// registered separately with [`Self::register_subagent`].
    pub fn subagent_reported(&mut self, subagent: Subagent, cx: &mut Context<Self>) {
        match self
            .reported_subagents
            .iter_mut()
            .find(|existing| existing.session_id == subagent.session_id)
        {
            // A spawn can arrive after its own terminal state.
            Some(existing) => {
                let state = existing.state;
                *existing = subagent;
                if state.is_terminal() {
                    existing.state = state;
                }
            }
            None => self.reported_subagents.push(subagent),
        }
        self.running_work_changed(cx);
    }

    pub fn subagent_state_updated(
        &mut self,
        session_id: &acp_v2::SessionId,
        state: AsyncTaskState,
        cx: &mut Context<Self>,
    ) {
        let Some(subagent) = self
            .reported_subagents
            .iter_mut()
            .find(|subagent| &subagent.session_id == session_id)
        else {
            return;
        };
        if subagent.state == state {
            return;
        }
        subagent.state = state;
        self.running_work_changed(cx);
    }

    pub(crate) fn clear_subagents(&mut self, cx: &mut Context<Self>) {
        if self.reported_subagents.is_empty() {
            return;
        }
        self.reported_subagents.clear();
        self.running_work_changed(cx);
    }
}
