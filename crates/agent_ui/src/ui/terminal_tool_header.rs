use ui::prelude::*;

/// Why a terminal command ran without the OS sandbox, ready to render.
///
/// Upstream pairs this with a `TerminalToolHeader` card header; our terminal
/// tool calls render as quiet one-line rows instead, so only the warning data
/// is shared and the rendering lives in the thread view.
pub struct TerminalSandboxWarning {
    pub title: SharedString,
    pub detail: SharedString,
    pub docs_url: SharedString,
}
