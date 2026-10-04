//! Local operator requests use the same ownership gate as cache maintenance.
use super::*;
use crate::cache::inspect::{self, Configuration, Overrides, Report};
use std::{collections::BTreeSet, os::unix::fs::FileTypeExt, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

impl App {
    fn cache_configuration(&self) -> Configuration {
        Configuration {
            limits: self.cache_limits,
            repositories: self.remote.as_ref().map(|r| (&r.options).into()),
            editors: self
                .remote
                .as_ref()
                .map(|r| {
                    r.options
                        .unity_versions
                        .iter()
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
    pub(super) fn save_cache_configuration(&self) -> Result<()> {
        crate::repository::manager::write_json(
            &self.cache.join(inspect::CONFIG),
            &self.cache_configuration(),
        )
    }
    pub async fn inspect_cache(self: &Arc<Self>, overrides: Overrides) -> Result<Report> {
        let mut active: BTreeSet<_> = self
            .workspaces
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, slot)| Arc::strong_count(&slot.state) > 1)
            .map(|(entry, _)| entry.clone())
            .collect();
        if let Some(remote) = &self.remote {
            active.extend(remote.active_entries());
        }
        // Existing environments are safe to read alongside queries. An unopened
        // environment needs a writer gate so NO_LOCK inspection excludes openers.
        let read = self.activity.clone().read_owned().await;
        let analysis = self.cache.join("analysis");
        let database = crate::store::Database::opened(&analysis);
        let (read, write) = if database.is_some() {
            (Some(read), None)
        } else {
            drop(read);
            (None, Some(self.activity.clone().write_owned().await))
        };
        let app = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut owners = app.owners.lock().unwrap().clone();
            inspect::seed_repositories(&app.cache, &mut owners)?;
            let analysis = if let Some(database) =
                database.or_else(|| crate::store::Database::opened(&analysis))
            {
                Some(inspect::Analysis::read(&mut owners, |visit| {
                    database.inspection(visit)
                })?)
            } else if analysis.join("data.mdb").is_file() {
                Some(inspect::Analysis::read(&mut owners, |visit| {
                    crate::store::shared::inspect_idle(&analysis, visit)
                })?)
            } else {
                None
            };
            // Filesystem measurement can run alongside queries once the idle
            // database has been closed again.
            let _activity =
                read.or_else(|| write.map(tokio::sync::OwnedRwLockWriteGuard::downgrade));
            inspect::build(
                &app.cache,
                app.cache_configuration(),
                overrides,
                owners,
                active,
                analysis,
                true,
            )
        })
        .await?
    }
    /// The cache directory is private to the service account. This socket only reads state.
    pub fn start_inspection(self: &Arc<Self>) -> Result<tokio::task::JoinHandle<()>> {
        self.save_cache_configuration()?;
        let path = self.cache.join(inspect::SOCKET);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket(),
                    "Invalid cache inspection socket"
                );
                std::fs::remove_file(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let listener = tokio::net::UnixListener::bind(path)?;
        let weak = Arc::downgrade(self);
        let shutdown = self.inspection_shutdown.clone();
        Ok(tokio::spawn(async move {
            loop {
                let connection = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    connection = listener.accept() => connection,
                };
                let Ok((mut stream, _)) = connection else {
                    return;
                };
                // Process one report at a time; reports never compete with each other.
                let result = async {
                    let mut bytes = Vec::new();
                    tokio::time::timeout(
                        Duration::from_secs(5),
                        (&mut stream).take(4097).read_to_end(&mut bytes),
                    )
                    .await??;
                    ensure!(bytes.len() <= 4096, "Inspection request too large");
                    let request: Overrides = serde_json::from_slice(&bytes)?;
                    let app = weak
                        .upgrade()
                        .ok_or_else(|| anyhow::anyhow!("Service stopped"))?;
                    app.inspect_cache(request).await
                }
                .await
                .map_err(|error: anyhow::Error| format!("{error:#}"));
                if let Ok(bytes) = serde_json::to_vec(&result) {
                    let _ = tokio::time::timeout(Duration::from_secs(5), stream.write_all(&bytes))
                        .await;
                }
            }
        }))
    }
}
