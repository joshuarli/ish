use std::collections::HashMap;

/// Maps alias names to their expansion (command + args).
pub struct AliasMap {
    map: HashMap<String, Vec<String>>,
}

impl AliasMap {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn set(&mut self, name: String, expansion: Vec<String>) {
        self.map.insert(name, expansion);
    }

    pub fn get(&self, name: &str) -> Option<&[String]> {
        self.map.get(name).map(|v| v.as_slice())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.map.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }
}
