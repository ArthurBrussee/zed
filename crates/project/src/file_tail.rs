//! The end of a file outside any worktree, read where the project runs.

use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use rpc::proto;

use crate::Project;

pub fn read_tail(
    project: &Entity<Project>,
    path: PathBuf,
    max_bytes: usize,
    cx: &App,
) -> Task<Result<String>> {
    let project = project.read(cx);
    if let Some(remote) = project.remote_client() {
        let request = remote.read(cx).proto_client().request(proto::ReadFileTail {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            path: path.to_string_lossy().into_owned(),
            max_bytes: max_bytes as u64,
        });
        return cx.background_spawn(async move { Ok(request.await?.content) });
    }
    let fs = project.fs().clone();
    cx.background_spawn(async move { read_local_tail(fs, path, max_bytes).await })
}

pub async fn read_local_tail(fs: Arc<dyn Fs>, path: PathBuf, max_bytes: usize) -> Result<String> {
    let content = fs.load(&path).await?;
    let mut start = content.len().saturating_sub(max_bytes);
    while !content.is_char_boundary(start) {
        start += 1;
    }
    Ok(content[start..].to_string())
}
