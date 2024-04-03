use anyhow::{anyhow, Context as _, Result};
use collections::HashMap;
use fs::Fs;
use gpui::{AppContext, Context, EventEmitter, Global, Model, ModelContext, Task, WeakModel};
use language::{LanguageRegistry, PARSER};
use project::{Project, Worktree};
use smol::channel;
use std::{
    path::{Path, PathBuf},
    sync::{atomic::AtomicUsize, Arc},
};
use util::ResultExt;

pub struct SemanticIndex {
    db: lmdb::Database,
    project_indices: HashMap<WeakModel<Project>, Model<ProjectIndex>>,
}

impl Global for SemanticIndex {}

impl SemanticIndex {
    pub fn new(db_path: &Path) -> Result<Self> {
        dbg!(&db_path);
        let env = lmdb::Environment::new()
            .open(db_path)
            .context("failed to open environment")?;
        let db = env
            .create_db(None, lmdb::DatabaseFlags::empty())
            .context("failed to create db")?;

        Ok(SemanticIndex {
            db,
            project_indices: HashMap::default(),
        })
    }

    pub fn project_index(
        &mut self,
        project: Model<Project>,
        cx: &mut AppContext,
    ) -> Model<ProjectIndex> {
        self.project_indices
            .entry(project.downgrade())
            .or_insert_with(|| cx.new_model(|cx| ProjectIndex::new(project, self.db.clone(), cx)))
            .clone()
    }
}

pub struct ProjectIndex {
    db: lmdb::Database,
    project: WeakModel<Project>,
    worktree_scans: HashMap<WeakModel<Worktree>, Task<()>>,
    language_registry: Arc<LanguageRegistry>,
    fs: Arc<dyn Fs>,
}

impl ProjectIndex {
    fn new(project: Model<Project>, db: lmdb::Database, cx: &mut ModelContext<Self>) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let fs = project.read(cx).fs().clone();
        let mut this = ProjectIndex {
            db,
            project: project.downgrade(),
            worktree_scans: HashMap::default(),
            language_registry,
            fs,
        };

        for worktree in project.read(cx).worktrees().collect::<Vec<_>>() {
            this.add_worktree(worktree, cx);
        }

        this
    }

    fn add_worktree(&mut self, worktree: Model<Worktree>, cx: &mut ModelContext<Self>) {
        let Some(local_worktree) = worktree.read(cx).as_local() else {
            return;
        };
        let local_worktree = local_worktree.snapshot();

        if self.worktree_scans.is_empty() {
            cx.emit(StatusEvent::Scanning);
        }

        let language_registry = self.language_registry.clone();
        let fs = self.fs.clone();
        let worktree = worktree.downgrade();
        let scan = cx.spawn(|this, mut cx| {
            let worktree = worktree.clone();
            async move {
                let (paths_tx, paths_rx) = channel::bounded(512);
                let worktree_abs_path = local_worktree.abs_path().clone();
                let path_producer = cx.background_executor().spawn(async move {
                    for entry in local_worktree.files(false, 0) {
                        if paths_tx.send(entry.path.clone()).await.is_err() {
                            break;
                        }
                    }
                });
                let path_count = AtomicUsize::new(0);

                cx.background_executor()
                    .scoped(|cx| {
                        for _ in 0..cx.num_cpus() {
                            cx.spawn(async {
                                while let Ok(path) = paths_rx.recv().await {
                                    if index_path(
                                        worktree_abs_path.join(&path),
                                        &language_registry,
                                        fs.as_ref(),
                                    )
                                    .await
                                    .log_err()
                                    .is_some()
                                    {
                                        path_count
                                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    }
                                }
                            });
                        }
                    })
                    .await;
                path_producer.await;

                this.update(&mut cx, |this, cx| {
                    this.worktree_scans.remove(&worktree);
                    if this.worktree_scans.is_empty() {
                        cx.emit(StatusEvent::Idle);
                    }
                    dbg!(path_count.load(std::sync::atomic::Ordering::SeqCst));
                })
                .ok();
            }
        });
        self.worktree_scans.insert(worktree, scan);
    }
}

impl EventEmitter<StatusEvent> for ProjectIndex {}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StatusEvent {
    Idle,
    Scanning,
}

async fn index_path(
    path: PathBuf,
    language_registry: &Arc<LanguageRegistry>,
    fs: &dyn Fs,
) -> Result<()> {
    // Plan:
    // Read file contents
    // Parse with Tree Sitter
    // Walk the parse tree from the top to find nodes below our embedding threshold

    let text = fs.load(&path).await?;
    let language = language_registry
        .language_for_file_path(&path)
        .await
        .with_context(|| format!("{:?}", path))?;
    if let Some(grammar) = language.grammar() {
        let tree = PARSER.with(|parser| {
            let mut parser = parser.borrow_mut();
            parser
                .set_language(&grammar.ts_language)
                .expect("incompatible grammar");
            parser.parse(&text, None).expect("invalid language")
        });
        dbg!(tree.root_node().start_byte()..tree.root_node().end_byte());
    } else {
        return Err(anyhow!("plain text"));
    }

    Ok(())

    // if let Ok(contents) = fs.read(&path).await {
    //     if let Some(language) = language_registry.get_language_for_path(&path) {
    //         // Process the contents with the determined language
    //         // This is a placeholder for actual indexing logic
    //         println!("Indexing {:?} with language {:?}", path, language);
    //     } else {
    //         println!("No language found for {:?}", path);
    //     }
    // } else {
    //     println!("Failed to read {:?}", path);
    // }
}