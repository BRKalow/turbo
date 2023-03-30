use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures::StreamExt;
use globwatch::{GlobSender, GlobWatcher, StopToken, Watcher};
use itertools::Itertools;
use log::{trace, warn};
use notify::RecommendedWatcher;

/// Tracks changes for a given hash. A hash is a unique identifier for a set of
/// files. Given a hash and a set of globs to track, this will watch for file
/// changes and allow the user to query for changes.
#[derive(Clone)]
pub struct HashGlobWatcher<T: Watcher> {
    hash_globs: Arc<Mutex<HashMap<String, Glob>>>,
    glob_status: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    watcher: Arc<Mutex<Option<GlobWatcher<T>>>>,
    config: GlobSender,
}

#[derive(Clone)]
pub struct Glob {
    include: HashSet<String>,
    exclude: HashSet<String>,
}

impl HashGlobWatcher<RecommendedWatcher> {
    pub fn new(flush_folder: PathBuf) -> Result<Self, globwatch::Error> {
        let (watcher, config) = GlobWatcher::new(flush_folder)?;
        Ok(Self {
            hash_globs: Default::default(),
            glob_status: Default::default(),
            watcher: Arc::new(Mutex::new(Some(watcher))),
            config,
        })
    }
}

impl<T: Watcher> HashGlobWatcher<T> {
    /// Watches a given path, using the flush_folder as temporary storage to
    /// make sure that file events are handled in the appropriate order.
    pub async fn watch(&self, root_folder: PathBuf, token: StopToken) {
        let start_globs = {
            let lock = self.hash_globs.lock().expect("no panic");
            lock.iter()
                .flat_map(|(_, g)| &g.include)
                .cloned()
                .collect::<Vec<_>>()
        };

        let mut stream = match self.watcher.lock().expect("no panic").take() {
            Some(watcher) => watcher.into_stream(token),
            None => {
                warn!("watcher already consumed");
                return;
            }
        };

        // watch all the globs currently in the map
        for glob in start_globs {
            self.config.include(glob.to_owned()).await.unwrap();
        }

        while let Some(Ok(event)) = stream.next().await {
            trace!("event: {:?}", event);

            let repo_relative_paths_iter = event
                .paths
                .iter()
                .filter_map(|path| path.strip_prefix(&root_folder).ok());

            let mut clear_glob_status = vec![];
            let mut exclude_globs = vec![];

            // put these in a block so we can drop the locks before we await
            {
                let mut glob_status = self.glob_status.lock().expect("ok");
                let mut hash_globs = self.hash_globs.lock().expect("ok");

                for ((glob, hash_status), path) in glob_status
                    .iter()
                    .cartesian_product(repo_relative_paths_iter)
                    .filter(|((glob, _), path)| {
                        glob_match::glob_match(glob, path.to_str().unwrap())
                    })
                {
                    for hash in hash_status.iter() {
                        let globs = match hash_globs.get_mut(hash).filter(|globs| {
                            globs
                                .exclude
                                .iter()
                                .any(|f| glob_match::glob_match(f, path.to_str().unwrap()))
                        }) {
                            Some(globs) => globs,
                            None => continue,
                        };

                        // we can stop tracking that glob
                        globs.include.remove(glob);
                        if globs.include.is_empty() {
                            hash_globs.remove(hash);
                        }

                        // store the hash and glob so we can remove it from the glob_status
                        exclude_globs.push(glob.to_owned());
                        clear_glob_status.push((hash.clone(), glob.clone()));
                    }
                }

                for (hash, glob) in clear_glob_status {
                    let empty = if let Some(globs) = glob_status.get_mut(&hash) {
                        globs.remove(&glob);
                        globs.is_empty()
                    } else {
                        false
                    };

                    if empty {
                        glob_status.remove(&hash);
                    }
                }
            }

            for glob in exclude_globs {
                self.config.exclude(glob.to_owned()).await.unwrap();
            }
        }
    }

    pub async fn watch_globs(
        &self,
        hash: String,
        include: HashSet<String>,
        exclude: HashSet<String>,
    ) {
        self.config.flush().await.unwrap();

        for glob in include.iter() {
            self.config.include(glob.to_owned()).await.unwrap();
        }

        let mut map = self.glob_status.lock().expect("no panic");
        map.entry(hash.clone()).or_default().extend(include.clone());

        let mut map = self.hash_globs.lock().expect("no panic");
        map.insert(hash, Glob { include, exclude });
    }

    /// Given a hash and a set of candidates, return the subset of candidates
    /// that have changed.
    pub async fn changed_globs(
        &self,
        hash: &str,
        mut candidates: HashSet<String>,
    ) -> HashSet<String> {
        self.config.flush().await.unwrap();

        let globs = self.hash_globs.lock().unwrap();
        match globs.get(hash) {
            Some(glob) => {
                candidates.retain(|c| glob.include.contains(c));
                candidates
            }
            None => candidates,
        }
    }
}
