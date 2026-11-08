//! Repository history previews retain one commit's file list and one file diff.
use super::HistoricalReviewPreview;
use crate::{
    compositor::{Context, DrawContext, EventResult},
    keymap::component::{action_hint, PickerAction},
    ui::picker::{PickerPreview, PreviewAction},
};
use event::TaskController;
use std::time::Duration;
use tokio::sync::oneshot;
use tui::buffer::Buffer;
use vcs::operations::{
    history::{CommitEntry, HistoryFile, RevisionId},
    GitRepository,
};
use view::{features::change_review::HistoricalReviewRequest, graphics::Rect};

pub struct CommitReviewPreview {
    repository: GitRepository,
    selected: Option<RevisionId>,
    task: TaskController,
    pending: Option<oneshot::Receiver<anyhow::Result<Vec<HistoryFile>>>>,
    files: Vec<HistoryFile>,
    index: usize,
    diff: Option<HistoricalReviewPreview<HistoryFile, HistoryFile>>,
    error: Option<String>,
}

impl CommitReviewPreview {
    pub fn new(repository: GitRepository) -> Self {
        Self {
            repository,
            selected: None,
            task: TaskController::new(),
            pending: None,
            files: Vec::new(),
            index: 0,
            diff: None,
            error: None,
        }
    }

    fn diff_area(area: Rect) -> Rect {
        let heading = area.height.min(4);
        Rect::new(area.x, area.y + heading, area.width, area.height - heading)
    }
}

impl PickerPreview<CommitEntry> for CommitReviewPreview {
    fn cancel(&mut self) {
        self.task.cancel();
        self.pending = None;
        self.selected = None;
        self.files.clear();
        self.index = 0;
        self.diff = None;
        self.error = None;
    }

    fn close_on_open(&self) -> bool {
        self.diff.as_ref().is_some_and(|diff| diff.close_on_open())
    }

    fn update(&mut self, item: Option<&CommitEntry>, area: Rect, cx: &mut Context) {
        let Some(commit) = item else {
            self.cancel();
            return;
        };
        if self.selected.as_ref() != Some(&commit.id) {
            self.cancel();
            self.selected = Some(commit.id.clone());
            let repository = self.repository.clone();
            let commit = commit.clone();
            let cancel = self.task.restart();
            let redraw = cx.editor.redraw().clone();
            let (tx, rx) = oneshot::channel();
            self.pending = Some(rx);
            tokio::spawn(async move {
                let work = async {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    let result = repository.commit_files(&commit, 0).await;
                    if !cancel.is_canceled() {
                        let _ = tx.send(result);
                        redraw.request();
                    }
                };
                event::cancelable_future(work, &cancel).await;
            });
        }
        if let Some(pending) = &mut self.pending {
            match pending.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    match result {
                        Ok(files) => {
                            self.files = files;
                            let repository = self.repository.clone();
                            let commit = commit.clone();
                            self.diff = Some(HistoricalReviewPreview::new(
                                |file: &HistoryFile| Some(file.clone()),
                                move |editor, file| {
                                    let (before, after) = file.endpoints(&commit, 0)?;
                                    HistoricalReviewRequest::revisions(
                                        editor,
                                        repository.clone(),
                                        before,
                                        after,
                                    )
                                },
                            ));
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.pending = None;
                    self.error = Some("Commit preview unavailable".into());
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
        if let Some(diff) = &mut self.diff {
            diff.update(self.files.get(self.index), Self::diff_area(area), cx);
        }
    }

    fn render(
        &mut self,
        item: Option<&CommitEntry>,
        area: Rect,
        surface: &mut Buffer,
        cx: &mut DrawContext,
    ) {
        let Some(commit) = item else { return };
        let keys = &cx.config.component_keys.history_picker;
        let file = self.files.get(self.index).map_or_else(
            || {
                self.error.clone().unwrap_or_else(|| {
                    if self.pending.is_some() {
                        "Loading changed files…".into()
                    } else {
                        "No changed files".into()
                    }
                })
            },
            |file| {
                format!(
                    "{}/{} · {} {}",
                    self.index + 1,
                    self.files.len(),
                    file.status,
                    file.path().display()
                )
            },
        );
        let headings = [
            format!("{} · {}", commit.id.short(), commit.subject),
            format!("{} · {}", commit.author, commit.date),
            format!(
                "{} · {}/{} files",
                if commit.parents.is_empty() {
                    "Root commit"
                } else {
                    "First parent"
                },
                action_hint(keys, &PickerAction::PreviewPrevious),
                action_hint(keys, &PickerAction::PreviewNext)
            ),
            file,
        ];
        for (row, heading) in headings.iter().take(area.height as usize).enumerate() {
            surface.set_stringn(
                area.x,
                area.y + row as u16,
                heading,
                area.width as usize,
                cx.editor.theme.get(if row == 0 {
                    "ui.text.focus"
                } else {
                    "ui.text.inactive"
                }),
            );
        }
        if !self.files.is_empty()
            && let Some(diff) = &mut self.diff
        {
            diff.render(
                self.files.get(self.index),
                Self::diff_area(area),
                surface,
                cx,
            );
        }
    }

    fn action(
        &mut self,
        item: Option<&CommitEntry>,
        action: PreviewAction,
        cx: &mut Context,
    ) -> EventResult {
        // A key can arrive after selection changes but before the next frame.
        self.update(item, Rect::default(), cx);
        if matches!(action, PreviewAction::Previous | PreviewAction::Next) {
            if !self.files.is_empty() {
                self.index = if matches!(action, PreviewAction::Next) {
                    (self.index + 1) % self.files.len()
                } else {
                    (self.index + self.files.len() - 1) % self.files.len()
                };
                if let Some(diff) = &mut self.diff {
                    diff.cancel();
                }
            }
            EventResult::Consumed(None)
        } else if let Some(diff) = &mut self.diff {
            diff.action(self.files.get(self.index), action, cx)
        } else {
            EventResult::Consumed(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{application::TestApplication, job::Jobs, ui::picker::test::application};
    use std::path::Path;

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env_remove("GIT_DIR")
            .env("GIT_AUTHOR_NAME", "Preview Author")
            .env("GIT_AUTHOR_EMAIL", "preview@example.com")
            .env("GIT_COMMITTER_NAME", "Preview Author")
            .env("GIT_COMMITTER_EMAIL", "preview@example.com")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn frame(
        preview: &mut CommitReviewPreview,
        commit: Option<&CommitEntry>,
        app: &mut TestApplication,
        action: Option<PreviewAction>,
    ) -> String {
        let config = crate::config::Config::default();
        let updates = tokio::sync::mpsc::unbounded_channel().0;
        let mut jobs = Jobs::new();
        let mut cx = Context {
            config: crate::config::Context {
                current: &config,
                updates: &updates,
            },
            editor: &mut app.editor,
            interaction: &mut app.interaction,
            jobs: &mut jobs,
            scroll: None,
            image_picker: None,
            is_cursor_owner: true,
        };
        let area = Rect::new(0, 0, 100, 24);
        let mut surface = Buffer::empty(area);
        preview.update(commit, area, &mut cx);
        if let Some(action) = action {
            preview.action(commit, action, &mut cx);
        }
        preview.render(commit, area, &mut surface, &mut cx.draw_context());
        surface.content.iter().map(|cell| cell.symbol()).collect()
    }

    async fn until_text(
        preview: &mut CommitReviewPreview,
        commit: &CommitEntry,
        app: &mut TestApplication,
        needle: &str,
    ) -> anyhow::Result<String> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let text = frame(preview, Some(commit), app, None);
                if text.contains(needle) {
                    break text;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(Into::into)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commit_preview_cycles_diffs_and_preserves_normal_acceptance() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        git(dir.path(), &["init"]);
        std::fs::write(dir.path().join("a.txt"), "root alpha\n")?;
        std::fs::write(dir.path().join("b.txt"), "root beta\n")?;
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "Root preview"]);
        std::fs::write(dir.path().join("a.txt"), "updated alpha\n")?;
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "Update preview"]);
        git(
            dir.path(),
            &["commit", "--allow-empty", "-m", "Empty preview"],
        );
        let repo = GitRepository::discover(dir.path())?;
        let commits = repo
            .history_page(repo.history_cursor(None)?.unwrap())
            .await?
            .commits;
        let mut app = application(false)?;
        let mut preview = CommitReviewPreview::new(repo);
        let root = &commits[2];
        let text = until_text(&mut preview, root, &mut app, "root alpha").await?;
        assert!(text.contains("Root preview") && text.contains("Preview Author"));
        assert!(text.contains("1/2 · A a.txt"));
        assert!(
            !preview.open_on_accept(root),
            "Enter must still browse changed files"
        );
        frame(
            &mut preview,
            Some(root),
            &mut app,
            Some(PreviewAction::Next),
        );
        let text = until_text(&mut preview, root, &mut app, "root beta").await?;
        assert!(text.contains("2/2 · A b.txt") && !text.contains("root alpha"));
        frame(
            &mut preview,
            Some(root),
            &mut app,
            Some(PreviewAction::Next),
        );
        until_text(&mut preview, root, &mut app, "root alpha").await?;
        frame(
            &mut preview,
            Some(root),
            &mut app,
            Some(PreviewAction::Previous),
        );
        until_text(&mut preview, root, &mut app, "root beta").await?;
        let text = until_text(&mut preview, &commits[1], &mut app, "updated alpha").await?;
        assert!(text.contains("root alpha") && !text.contains("root beta"));
        assert_eq!(preview.index, 0);
        until_text(&mut preview, &commits[0], &mut app, "No changed files").await?;
        frame(
            &mut preview,
            Some(&commits[0]),
            &mut app,
            Some(PreviewAction::Next),
        );
        assert_eq!(preview.index, 0);
        until_text(&mut preview, root, &mut app, "root alpha").await?;
        frame(
            &mut preview,
            Some(root),
            &mut app,
            Some(PreviewAction::Open(view::editor::Action::Replace)),
        );
        assert!(
            preview.close_on_open(),
            "Alt-o must open the displayed historical diff"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn changing_or_hiding_commit_preview_discards_pending_reads() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        git(dir.path(), &["init"]);
        git(dir.path(), &["commit", "--allow-empty", "-m", "First"]);
        git(dir.path(), &["commit", "--allow-empty", "-m", "Second"]);
        let repo = GitRepository::discover(dir.path())?;
        let commits = repo
            .history_page(repo.history_cursor(None)?.unwrap())
            .await?
            .commits;
        let mut app = application(false)?;
        let mut preview = CommitReviewPreview::new(repo);
        frame(&mut preview, Some(&commits[0]), &mut app, None);
        assert!(preview.pending.is_some());
        frame(&mut preview, Some(&commits[1]), &mut app, None);
        assert_eq!(preview.selected.as_ref(), Some(&commits[1].id));
        preview.cancel();
        assert!(preview.pending.is_none() && preview.diff.is_none() && preview.selected.is_none());
        frame(&mut preview, Some(&commits[0]), &mut app, None);
        frame(&mut preview, None, &mut app, None);
        assert!(preview.pending.is_none() && preview.selected.is_none());
        until_text(&mut preview, &commits[1], &mut app, "No changed files").await?;
        Ok(())
    }
}
