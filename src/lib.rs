use std::str;
use std::sync::Arc;
use std::{collections::HashMap, path::Path};

use blake3;
use git2::build::CheckoutBuilder;
use git2::{
    BranchType, Commit, ErrorCode, FileFavor, MergeOptions, Oid, PushOptions, RebaseOptions,
    Repository, Signature, Time, Tree, TreeBuilder,
};
use parking_lot::{Mutex, MutexGuard};
use rand::distributions::Alphanumeric;
use rand::prelude::*;
use tokio::runtime::{Handle, Runtime};

pub mod error;
pub mod replica;

pub enum OperationTarget<'a> {
    Main,
    Transaction(&'a str),
}

pub enum ConflictResolution {
    Overwrite,
    DiscardChanges,
    Abort,
}

pub struct Collection<'c> {
    repository: Arc<Mutex<Repository>>,
    replicas: Vec<replica::Replica<'c>>,
    handle: Handle,
}

impl<'c> Collection<'c> {
    pub fn load(path: &Path) -> Result<Self, error::CollectionInitError> {
        Ok(Self {
            repository: Arc::new(Mutex::new(Repository::open(path)?)),
            replicas: Vec::new(),
            handle: Collection::get_runtime_handle().0,
        })
    }

    pub fn create(path: &Path) -> Result<Self, error::CollectionInitError> {
        let repo = Repository::init_bare(path).unwrap();
        {
            let index = &mut repo.index()?;
            let id = index.write_tree()?;
            let tree = repo.find_tree(id)?;
            let sig = repo.signature()?;
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])?;
            let head = repo.head()?.target().unwrap();
            let head_commit = repo.find_commit(head)?;
            repo.branch("main", &head_commit, true)?;
        }
        Ok(Self {
            repository: Arc::new(Mutex::new(repo)),
            replicas: Vec::new(),
            handle: Collection::get_runtime_handle().0,
        })
    }

    pub fn add_replica(
        &mut self,
        name: &str,
        url: &str,
        replication_method: replica::ReplicationMethod,
        push_options: Option<PushOptions<'c>>,
    ) {
        if self.replicas.iter().any(|x| x.remote.as_str() == name) {
            return;
        }
        let repo = self.repository.lock();
        let remote = repo
            .find_remote(name.as_ref())
            .unwrap_or_else(|_| repo.remote(name, url).unwrap());
        self.replicas.push(replica::Replica {
            remote: remote.name().unwrap().to_string(),
            replication_method,
            push_options,
        });
    }

    pub fn get(
        &self,
        key: &str,
        target: OperationTarget,
    ) -> Result<Option<Vec<u8>>, error::GetObjectError> {
        let path = Self::construct_path_to_key(key);
        let branch = match target {
            OperationTarget::Main => "main",
            OperationTarget::Transaction(t) => t,
        };
        let repo = self.repository.lock();
        let tree_path = Collection::current_commit(&repo, branch)
            .map_err(|e| match e.code() {
                ErrorCode::NotFound => error::GetObjectError::InvalidOperationTarget,
                _ => e.into(),
            })?
            .tree()
            .unwrap()
            .get_path(Path::new(&path))
            .ok();
        if let Some(tree_entry) = tree_path {
            let obj = tree_entry.to_object(&repo)?;
            let blob = obj
                .as_blob()
                .ok_or_else(|| error::GetObjectError::CorruptedObject)?;
            let blob_content = blob.content();
            return Ok(Some(blob_content.to_vec()));
        };
        Ok(None)
    }

    pub fn set_batch<'a, I, T>(
        &self,
        items: I,
        target: OperationTarget,
    ) -> HashMap<String, tokio::task::JoinHandle<Result<(), git2::Error>>>
    where
        I: IntoIterator<Item = (T, &'a [u8])>,
        T: AsRef<str>,
    {
        let repo = self.repository.lock();
        let branch = match target {
            OperationTarget::Main => "main",
            OperationTarget::Transaction(t) => t,
        };
        let commit = Collection::current_commit(&repo, branch).unwrap();
        {
            let mut root_tree = commit.tree().unwrap();
            for (key, value) in items {
                let blob = repo.blob(value).unwrap();
                let hash = blake3::hash(key.as_ref().as_bytes());
                let trees =
                    Collection::make_tree(&repo, hash.as_bytes(), &root_tree, key.as_ref(), blob)
                        .unwrap();
                root_tree = repo.find_tree(trees).unwrap();
            }
            let signature = self.signature();
            let new_commit = repo
                .commit_create_buffer(&signature, &signature, "update db", &root_tree, &[&commit])
                .unwrap();
            let commit_obj = repo
                .commit_signed(str::from_utf8(&new_commit).unwrap(), "", None)
                .unwrap();
            let mut branch_ref = repo.find_branch(branch, BranchType::Local).unwrap();
            branch_ref
                .get_mut()
                .set_target(commit_obj, "update db")
                .unwrap();
        }
        drop(commit);
        drop(repo);
        self.replicate()
    }

    pub fn set(
        &self,
        key: &str,
        value: &[u8],
        target: OperationTarget,
    ) -> HashMap<String, tokio::task::JoinHandle<Result<(), git2::Error>>> {
        self.set_batch([(key, value)], target)
    }

    pub fn new_transaction(&self, name: Option<&str>) -> String {
        let repo = self.repository.lock();
        let head = repo.head().unwrap().target().unwrap();
        let head_commit = repo.find_commit(head).unwrap();
        let transaction_name = name.map(|n| n.to_string()).unwrap_or_else(|| {
            format!(
                "t-{}",
                rand::thread_rng()
                    .sample_iter(&Alphanumeric)
                    .take(8)
                    .map(char::from)
                    .collect::<String>()
            )
        });
        repo.branch(&transaction_name, &head_commit, true).unwrap();
        transaction_name
    }

    pub fn apply_transaction<S>(&self, name: S, conflict_resolution: ConflictResolution)
    where
        S: AsRef<str>,
    {
        let repo = self.repository.lock();
        let main_branch = repo
            .find_annotated_commit(Collection::current_commit(&repo, "main").unwrap().id())
            .unwrap();
        let target_branch = repo
            .find_annotated_commit(
                Collection::current_commit(&repo, name.as_ref())
                    .unwrap()
                    .id(),
            )
            .unwrap();
        let mut checkout_options = CheckoutBuilder::new();
        checkout_options.force();
        checkout_options.allow_conflicts(true);
        let mut merge_options = MergeOptions::new();
        match conflict_resolution {
            ConflictResolution::DiscardChanges => {
                checkout_options.use_ours(true);
                merge_options.file_favor(FileFavor::Ours);
            }
            ConflictResolution::Overwrite => {
                checkout_options.use_theirs(true);
                merge_options.file_favor(FileFavor::Theirs);
            }
            ConflictResolution::Abort => {
                merge_options.fail_on_conflict(true);
            }
        }
        let mut rebase_options = RebaseOptions::new();
        let mut rebase_opts = rebase_options
            .inmemory(true)
            .checkout_options(checkout_options)
            .merge_options(merge_options);
        let mut rebase = repo
            .rebase(
                Some(&target_branch),
                Some(&main_branch),
                None,
                Some(&mut rebase_opts),
            )
            .unwrap();
        let mut current_commit: Option<Oid> = None;
        loop {
            let change = rebase.next();
            if change.is_none() {
                rebase.finish(None).unwrap();
                if let Some(commit) = current_commit {
                    let mut branch_ref = repo.find_branch("main", BranchType::Local).unwrap();
                    branch_ref
                        .get_mut()
                        .set_target(commit, "update db")
                        .unwrap();
                };
                break;
            }
            if let Ok(com) = rebase.commit(None, &self.signature(), None) {
                current_commit = Some(com);
            }
        }
        repo.find_branch(name.as_ref(), BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
    }

    fn replicate(&self) -> HashMap<String, tokio::task::JoinHandle<Result<(), git2::Error>>> {
        let mut remote_push_results = HashMap::new();
        let rand_res: f64 = rand::thread_rng().gen();
        for replica in &self.replicas {
            let replicate = match replica.replication_method {
                replica::ReplicationMethod::All => true,
                replica::ReplicationMethod::Random(chance) => rand_res > chance,
                _ => true,
            };
            if !replicate {
                continue;
            }
            let data = Arc::clone(&self.repository);
            let replica_remote = replica.remote.clone();
            let task = self.handle.spawn(async move {
                let repo = data.lock();
                let mut remote = repo.find_remote(&replica_remote).unwrap();
                remote.push(&["refs/heads/main"], None)
            });
            remote_push_results.insert(replica.remote.clone(), task);
        }
        remote_push_results
    }

    fn make_tree<'a>(
        repo: &'a MutexGuard<Repository>,
        oid: &[u8],
        root_tree: &'a Tree,
        key: &str,
        blob: Oid,
    ) -> Result<Oid, git2::Error> {
        let mut trees: Vec<TreeBuilder> = vec![repo.treebuilder(Some(root_tree)).unwrap()];
        for part in 0..2 {
            let parent_tree = trees.pop().unwrap();
            let octal_part = oid[part];
            let mut tree_builder = parent_tree
                .get(format!("{octal_part:o}"))
                .unwrap()
                .map(|x| repo.treebuilder(Some(&x.to_object(&repo).unwrap().into_tree().unwrap())))
                .unwrap_or_else(|| repo.treebuilder(None))?;
            if part == 1 {
                tree_builder.insert(key, blob, 0o100644)?;
            }
            trees.push(parent_tree);
            trees.push(tree_builder);
        }
        let mut index: usize = 2;
        loop {
            if let Some(self_tree) = trees.pop() {
                if let Some(mut parent_tree) = trees.pop() {
                    let tree_id = self_tree.write()?;
                    index -= 1;
                    let octal_part = oid[index];
                    parent_tree.insert(format!("{octal_part:o}"), tree_id, 0o040000)?;
                    trees.push(parent_tree);
                } else {
                    return Ok(self_tree.write()?);
                }
            } else {
                // TODO: what to do in that case?
                panic!("This shouldn't have happened");
            }
        }
    }

    pub fn revert_to_commit(&self, commit: Oid) {
        let repo = self.repository.lock();
        let target_commit = repo.find_commit(commit).unwrap();
        repo.reset(target_commit.as_object(), git2::ResetType::Soft, None)
            .unwrap();
    }

    pub fn revert_n_commits(&self, n: usize) -> Result<(), error::RevertError> {
        if n == 0 {
            return Ok(());
        }
        let repo = self.repository.lock();
        let head = repo.head().unwrap().target().unwrap();
        let mut target_commit = repo.find_commit(head).unwrap();
        for _ in 0..n {
            if target_commit.parent_count() > 1 {
                return Err(error::RevertError::BranchingHistory { commit: head });
            } else if target_commit.parent_count() == 0 {
                break;
            }
            target_commit = target_commit.parent(0)?;
        }
        repo.reset(target_commit.as_object(), git2::ResetType::Soft, None)?;
        Ok(())
    }

    fn current_commit<'a>(
        repo: &'a MutexGuard<Repository>,
        branch: &str,
    ) -> Result<Commit<'a>, git2::Error> {
        let reference = repo
            .find_branch(branch.as_ref(), BranchType::Local)?
            .into_reference();
        let commit = reference.peel_to_commit()?;
        Ok(commit)
    }

    fn construct_path_to_key(key: &str) -> String {
        let hash = blake3::hash(key.as_bytes());
        let hash_bytes = hash.as_bytes();
        let mut path = String::new();
        for x in 0..2 {
            let val = &hash_bytes[x];
            path.push_str(format!("{val:o}").as_ref());
            path.push('/');
        }
        path.push_str(key);
        path
    }

    fn get_runtime_handle() -> (Handle, Option<Runtime>) {
        match Handle::try_current() {
            Ok(h) => (h, None),
            Err(_) => {
                let rt = Runtime::new().unwrap();
                (rt.handle().clone(), Some(rt))
            }
        }
    }

    fn signature(&self) -> Signature {
        let current_time = &Time::new(chrono::Utc::now().timestamp(), 0);
        Signature::new("yamabiko", "yamabiko", current_time).unwrap()
    }
}

pub mod test;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use git2::{BranchType, Repository};

    use crate::{replica::ReplicationMethod, OperationTarget};

    use super::test::*;

    #[test]
    fn set_and_get() {
        let (db, _td) = create_db();
        db.set("key", "value".as_bytes(), OperationTarget::Main);
        assert_eq!(
            db.get("key", OperationTarget::Main).unwrap().unwrap(),
            "value".as_bytes()
        );
    }

    #[test]
    fn batch_set_and_get() {
        let (db, _td) = create_db();
        let mut hm = HashMap::new();
        hm.insert("a", "initial a value".as_bytes());
        hm.insert("b", "initial b value".as_bytes());
        hm.insert("c", "initial c value".as_bytes());
        let mut hm2 = hm.clone();
        db.set_batch(hm, OperationTarget::Main);
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            "initial a value".as_bytes()
        );
        assert_eq!(
            db.get("b", OperationTarget::Main).unwrap().unwrap(),
            "initial b value".as_bytes()
        );
        assert_eq!(
            db.get("c", OperationTarget::Main).unwrap().unwrap(),
            "initial c value".as_bytes()
        );
        hm2.insert("a", "changed a value".as_bytes());
        db.set_batch(hm2, OperationTarget::Main);
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            "changed a value".as_bytes()
        );
    }

    #[test]
    fn get_non_existent_value() {
        let (db, _td) = create_db();
        assert_eq!(db.get("key", OperationTarget::Main).unwrap(), None);
    }

    #[test]
    fn test_revert_n_commits() {
        let (db, _td) = create_db();
        db.set("a", b"initial a value", OperationTarget::Main);
        db.set("b", b"initial b value", OperationTarget::Main);
        db.set("b", b"changed b value", OperationTarget::Main);
        assert_eq!(
            db.get("b", OperationTarget::Main).unwrap().unwrap(),
            b"changed b value"
        );
        db.revert_n_commits(1).unwrap();
        assert_eq!(
            db.get("b", OperationTarget::Main).unwrap().unwrap(),
            b"initial b value"
        );
    }

    #[test]
    fn test_revert_to_commit() {
        let (db, td) = create_db();
        db.set("a", b"initial a value", OperationTarget::Main);
        db.set("a", b"change #1", OperationTarget::Main);
        db.set("a", b"change #2", OperationTarget::Main);
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            b"change #2"
        );
        let repo = Repository::open(td.path()).unwrap();
        let reference = repo
            .find_branch("main", BranchType::Local)
            .unwrap()
            .into_reference();
        let head_commit = reference.peel_to_commit().unwrap();
        let first_commit = head_commit.parent(0).unwrap().parent(0).unwrap().clone();
        db.revert_to_commit(first_commit.id());
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            b"initial a value"
        );
    }

    #[test]
    fn test_replica_same_name() {
        let (mut db, _td) = create_db();
        let (_, _td_backup) = create_db();
        db.add_replica(
            "test",
            _td_backup.path().to_str().unwrap(),
            ReplicationMethod::All,
            None,
        );
        db.add_replica(
            "test",
            _td_backup.path().to_str().unwrap(),
            ReplicationMethod::All,
            None,
        );
        assert_eq!(db.replicas.len(), 1);
    }

    #[test]
    fn test_replica_already_in_git() {
        let (mut db, _td) = create_db();
        let (_, _td_backup) = create_db();
        db.repository
            .lock()
            .remote("test", _td_backup.path().to_str().unwrap())
            .unwrap();
        db.add_replica(
            "test",
            _td_backup.path().to_str().unwrap(),
            ReplicationMethod::All,
            None,
        );
        assert_eq!(db.replicas.len(), 1);
    }

    #[tokio::test]
    async fn test_replica_sync() {
        let (mut db, _td) = create_db();
        let (db_backup, _td_backup) = create_db();
        db.add_replica(
            "test",
            _td_backup.path().to_str().unwrap(),
            ReplicationMethod::All,
            None,
        );
        assert_eq!(db.replicas.len(), 1);
        let result = db.set("a", b"a value", OperationTarget::Main);
        for (_, value) in result {
            value.await.unwrap().unwrap();
        }
        assert_eq!(
            db_backup.get("a", OperationTarget::Main).unwrap().unwrap(),
            b"a value"
        );
    }

    #[tokio::test]
    async fn test_replica_non_existing_repo() {
        let (mut db, _td) = create_db();
        db.add_replica(
            "test",
            "https://800.800.800.800/git.git",
            ReplicationMethod::All,
            None,
        );
        assert_eq!(db.replicas.len(), 1);
        let result = db.set("a", b"a value", OperationTarget::Main);
        for (_, value) in result {
            assert!(value.await.unwrap().is_err());
        }
    }

    #[test]
    fn test_simple_transaction() {
        let (db, _td) = create_db();
        db.set("a", b"a val", OperationTarget::Main);
        let t = db.new_transaction(None);
        db.set("b", b"b val", OperationTarget::Transaction(&t));
        assert_eq!(db.get("b", OperationTarget::Main).unwrap(), None);
        assert_eq!(
            db.get("b", OperationTarget::Transaction(&t))
                .unwrap()
                .unwrap(),
            b"b val"
        );
        db.apply_transaction(&t, crate::ConflictResolution::Overwrite);
        assert_eq!(
            db.get("b", OperationTarget::Main).unwrap().unwrap(),
            b"b val"
        );
    }

    #[test]
    fn test_transaction_overwrite() {
        let (db, _td) = create_db();
        db.set("a", b"INIT\nline2", OperationTarget::Main);
        let t = db.new_transaction(None);
        db.set("a", b"TRAN\nline2", OperationTarget::Transaction(&t));
        db.set("a", b"MAIN\nline2", OperationTarget::Main);
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            b"MAIN\nline2"
        );
        assert_eq!(
            db.get("a", OperationTarget::Transaction(&t))
                .unwrap()
                .unwrap(),
            b"TRAN\nline2"
        );
        db.apply_transaction(&t, crate::ConflictResolution::Overwrite);
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            b"TRAN\nline2"
        );
    }

    #[test]
    fn test_transaction_discard() {
        let (db, _td) = create_db();
        db.set("a", b"INIT\nline2", OperationTarget::Main);
        let t = db.new_transaction(None);
        db.set("a", b"TRAN\nline2", OperationTarget::Transaction(&t));
        db.set("a", b"MAIN\nline2", OperationTarget::Main);
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            b"MAIN\nline2"
        );
        assert_eq!(
            db.get("a", OperationTarget::Transaction(&t))
                .unwrap()
                .unwrap(),
            b"TRAN\nline2"
        );
        db.apply_transaction(&t, crate::ConflictResolution::DiscardChanges);
        assert_eq!(
            db.get("a", OperationTarget::Main).unwrap().unwrap(),
            b"MAIN\nline2"
        );
    }
}
