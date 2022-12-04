use std::fmt::Display;

#[derive(PartialEq, Debug)]
pub enum IndexType {
    Single,
}

impl IndexType {
    pub fn from_name(name: &str) -> Result<Self, ()> {
        match name {
            "single" => Ok(Self::Single),
            _ => Err(()),
        }
    }
}

impl Display for IndexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                IndexType::Single => "single",
            }
        )
    }
}

#[derive(Debug, PartialEq)]
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

    pub fn from_name(name: &str) -> Result<Self, ()> {
        let token_list = name.rsplit_once(".").unwrap().0.rsplit_once("#");
        if let Some(tokens) = token_list {
            return Ok(Self::new(name, tokens.0, IndexType::from_name(tokens.1)?));
        }
        Err(())
    }

    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn indexed_field(&self) -> &str {
        self.indexed_field.as_str()
    }
}
