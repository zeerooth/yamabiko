use std::str::FromStr;
use std::{fmt::Display, path::Path};

use git2::{Index as GitIndex, IndexEntry, IndexTime, Oid, Repository};

use crate::debug;
use crate::field::Field;

#[derive(PartialEq, Eq, Hash, Debug, Clone, Copy)]
pub enum IndexType {
    Numeric,
    Sequential,
    Collection,
}

impl FromStr for IndexType {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, String> {
        match name {
            "numeric" => Ok(Self::Numeric),
            "sequential" => Ok(Self::Sequential),
            "collection" => Ok(Self::Collection),
            _ => Err(String::from("No such index type")),
        }
    }
}

impl Display for IndexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Numeric => "numeric",
                Self::Sequential => "sequential",
                Self::Collection => "collection",
            }
        )
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Index {
    name: String,
    indexed_field: String,
    kind: IndexType,
}

impl Index {
    pub fn new(name: &str, indexed_field: &str, kind: IndexType) -> Self {
        Self {
            name: name.to_string(),
            indexed_field: indexed_field.to_string(),
            kind,
        }
    }

    pub fn from_name(name: &str) -> Result<Self, String> {
        let token_list = name.rsplit_once(".").unwrap().0.rsplit_once("#");
        if let Some(tokens) = token_list {
            return Ok(Self::new(name, tokens.0, IndexType::from_str(tokens.1)?));
        }
        Err(String::from("No such index"))
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn indexed_field(&self) -> &str {
        self.indexed_field.as_str()
    }

    pub fn indexes_given_field(&self, field: &Field) -> bool {
        match field {
            Field::Int(_) => self.kind == IndexType::Numeric,
            Field::Float(_) => self.kind == IndexType::Numeric,
            Field::String(_) => self.kind == IndexType::Sequential,
        }
    }

    pub fn create_entry(&self, repo: &Repository, oid: Oid, field: &Field) {
        let value = field.to_index_value();
        let mut git_index = self.git_index(repo);
        let last_entry = git_index.find_prefix(&value);
        let next_value = match last_entry {
            Ok(v) => {
                let path = git_index.get(v).unwrap().path;
                let num = u64::from_str_radix(
                    core::str::from_utf8(path.split_at(path.len() - 16).1).unwrap(),
                    16,
                )
                .unwrap();
                num - 1
            }
            Err(_) => u64::MAX,
        };
        let path = format!("{}/{:16x}", value, next_value);
        let entry = IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: field.to_ino_number(),
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: oid,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        };
        debug!("creating a new entry: {:?}", entry);
        git_index.add(&entry).unwrap();
        git_index.write().unwrap();
    }

    pub fn delete_entry(&self, repo: &Repository, oid: Oid) -> bool {
        // this method is going to be terribly slow on large indexes but it works for now
        let mut git_index = self.git_index(repo);
        debug!("removing an entry with oid: {}", oid);
        if let Some(entry) = git_index.iter().find(|x| x.id == oid) {
            git_index
                .remove(Path::new(&String::from_utf8(entry.path).unwrap()), 0)
                .unwrap();
            git_index.write().unwrap();
            return true;
        }
        git_index.write().unwrap();
        false
    }

    pub fn git_index(&self, repo: &Repository) -> GitIndex {
        GitIndex::open(
            Path::new(repo.path())
                .join(".index")
                .join(self.name())
                .as_path(),
        )
        .unwrap()
    }

    pub fn extract_value(entry: &IndexEntry) -> &[u8] {
        let n = match entry.ino {
            1 => 2,
            _ => 3,
        };
        entry.path.rsplitn(n, |b| *b == b'/').nth(1).unwrap()
    }
}
