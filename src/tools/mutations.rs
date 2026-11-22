//! Serialize mutations by destination path and existing file identity.
//! Hard links share an identity. New destinations reserve a creation gate so
//! a case-insensitive filesystem cannot admit differently cased creates.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Hash, PartialEq, Eq)]
enum Key {
    Path(PathBuf),
    File(FileIdentity),
}

#[cfg(unix)]
#[derive(Hash, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}
#[cfg(windows)]
type FileIdentity = same_file::Handle;

type Locks = Arc<Mutex<HashMap<Arc<Key>, Weak<AsyncMutex<()>>>>>;

#[derive(Clone, Default)]
pub struct MutationQueue {
    locks: Locks,
    creation: Arc<AsyncMutex<()>>,
}

pub struct MutationGuard {
    _path: LockGuard,
    _file: Option<LockGuard>,
    _creation: Option<OwnedMutexGuard<()>>,
}

struct LockGuard {
    guard: Option<OwnedMutexGuard<()>>,
    locks: Locks,
    key: Arc<Key>,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        self.guard.take();
        let mut locks = self.locks.lock().expect("mutation registry");
        if locks
            .get(&self.key)
            .is_some_and(|lock| lock.strong_count() == 0)
        {
            locks.remove(&self.key);
        }
    }
}

impl MutationQueue {
    async fn acquire(&self, key: Key) -> LockGuard {
        let key = Arc::new(key);
        let lock = {
            let mut locks = self.locks.lock().expect("mutation registry");
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                locks.insert(key.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        LockGuard {
            guard: Some(lock.lock_owned().await),
            locks: self.locks.clone(),
            key,
        }
    }

    pub async fn lock(&self, path: &Path) -> std::io::Result<MutationGuard> {
        let creation = self.creation.clone().lock_owned().await;
        let path = path.to_owned();
        let (key, identity) = tokio::task::spawn_blocking(move || destination(&path))
            .await
            .map_err(|_| std::io::Error::other("Mutation identity worker failed"))??;
        let path_guard = self.acquire(Key::Path(key)).await;
        if let Some(identity) = identity {
            // Existing destinations can execute concurrently with other files.
            drop(creation);
            let file_guard = self.acquire(Key::File(identity)).await;
            Ok(MutationGuard {
                _path: path_guard,
                _file: Some(file_guard),
                _creation: None,
            })
        } else {
            // Hold the reservation through creation and mutation. A later alias
            // lookup sees a complete file, not a half-created competing target.
            Ok(MutationGuard {
                _path: path_guard,
                _file: None,
                _creation: Some(creation),
            })
        }
    }
}

fn destination(path: &Path) -> std::io::Result<(PathBuf, Option<FileIdentity>)> {
    let mut probe = path.to_owned();
    let mut suffix = Vec::new();
    let mut key = loop {
        match std::fs::canonicalize(&probe) {
            Ok(canonical) => break canonical,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                suffix.push(probe.file_name().ok_or(error)?.to_owned());
                probe.pop();
            }
            Err(error) => return Err(error),
        }
    };
    let identity = if suffix.is_empty() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&key)?;
            Some(FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            Some(same_file::Handle::from_path(&key)?)
        }
    } else {
        None
    };
    for component in suffix.into_iter().rev() {
        key.push(component);
    }
    #[cfg(windows)]
    let key = PathBuf::from(key.to_string_lossy().to_lowercase());
    Ok((key, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hard_link_aliases_share_a_mutation_lock_and_other_files_do_not() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let alias = temp.path().join("alias");
        let other = temp.path().join("other");
        std::fs::write(&first, "fixture").unwrap();
        std::fs::hard_link(&first, &alias).unwrap();
        std::fs::write(&other, "fixture").unwrap();
        let queue = MutationQueue::default();
        let held = queue.lock(&first).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), queue.lock(&alias))
                .await
                .is_err()
        );
        let independent = queue.lock(&other).await.unwrap();
        drop(independent);
        drop(held);
        assert!(queue.lock(&alias).await.is_ok());
    }
}
