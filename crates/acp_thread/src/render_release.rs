use gpui::{App, Context, Entity};
use markdown::Markdown;

use crate::{AcpThread, AgentThreadEntry, AssistantMessageChunk, MessageContent, Terminal};

/// What [`AcpThread::release_render_state`] gave up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReleasedRenderState {
    pub markdowns: usize,
    pub terminal_grids: usize,
}

impl AcpThread {
    /// For a thread nothing is drawing: drops every Markdown's parse and every
    /// finished display terminal's grid, which together are most of what a
    /// long thread holds beyond its text. Both come back when drawn.
    pub fn release_render_state(&mut self, cx: &mut Context<Self>) -> ReleasedRenderState {
        let (markdowns, terminals) = self.render_state(cx);
        let mut released = ReleasedRenderState::default();
        for markdown in markdowns {
            if !markdown.read(cx).is_parse_released()
                && markdown.update(cx, |markdown, cx| markdown.release_parsed(cx))
            {
                released.markdowns += 1;
            }
        }
        for terminal in terminals {
            if terminal.update(cx, |terminal, cx| terminal.release_grid(cx)) {
                released.terminal_grids += 1;
            }
        }
        released
    }

    /// The parses and grids [`Self::release_render_state`] would find held.
    #[cfg(any(test, feature = "test-support"))]
    pub fn held_render_state(&self, cx: &App) -> ReleasedRenderState {
        let (markdowns, terminals) = self.render_state(cx);
        ReleasedRenderState {
            markdowns: markdowns
                .iter()
                .filter(|markdown| {
                    let markdown = markdown.read(cx);
                    !markdown.is_parse_released() && !markdown.source().is_empty()
                })
                .count(),
            terminal_grids: terminals
                .iter()
                .filter(|terminal| !terminal.read(cx).is_grid_released())
                .count(),
        }
    }

    fn render_state(&self, cx: &App) -> (Vec<Entity<Markdown>>, Vec<Entity<Terminal>>) {
        let mut markdowns = Vec::new();
        for entry in &self.entries {
            entry_markdowns(entry, &mut markdowns);
        }
        let terminals: Vec<_> = self.terminals.values().cloned().collect();
        for terminal in &terminals {
            markdowns.push(terminal.read(cx).command().clone());
        }
        (markdowns, terminals)
    }
}

fn entry_markdowns(entry: &AgentThreadEntry, out: &mut Vec<Entity<Markdown>>) {
    let content = |content: &MessageContent, out: &mut Vec<Entity<Markdown>>| {
        out.extend(content.markdowns().cloned());
    };
    match entry {
        AgentThreadEntry::UserMessage(message) => content(&message.content, out),
        AgentThreadEntry::SessionMessage(message) => content(&message.content, out),
        AgentThreadEntry::AssistantMessage(message) => {
            for chunk in &message.chunks {
                match chunk {
                    AssistantMessageChunk::Message { block, .. }
                    | AssistantMessageChunk::Thought { block, .. } => content(block, out),
                }
            }
        }
        AgentThreadEntry::ToolCall(call) => {
            out.push(call.label.clone());
            out.extend(call.raw_input_markdown.clone());
            for item in call
                .structured_content
                .iter()
                .chain(call.raw_output_content.as_deref())
            {
                out.extend(item.markdown().cloned());
            }
        }
        AgentThreadEntry::ContextCompaction(compaction) => {
            out.extend(compaction.error.clone());
            content(&compaction.summary, out);
        }
        AgentThreadEntry::Elicitation(_) => {}
    }
}
