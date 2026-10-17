use hashbrown::HashMap;
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, PathBuf};
use std::sync::Arc;


#[derive(Clone, Debug)]
pub struct ScanExclusions {
    nodes: Arc<Vec<ExclusionNode>>,
}

#[derive(Default, Debug)]
struct ExclusionNode {
    children: HashMap<OsString, usize>,
    excluded: bool,
}

impl ScanExclusions {
    
    
    
    
    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> io::Result<Self> {
        let mut nodes = vec![ExclusionNode::default()];
        for path in paths {
            let mut current = 0;
            let mut has_component = false;
            for component in path.components() {
                let Component::Normal(name) = component else {
                    match component {
                        Component::CurDir => continue,
                        Component::ParentDir => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "exclude paths cannot traverse above the scan root",
                            ));
                        }
                        Component::RootDir | Component::Prefix(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "exclude paths must be relative to the scan root",
                            ));
                        }
                        Component::Normal(_) => unreachable!(),
                    }
                };
                has_component = true;
                let next = match nodes[current].children.get(name) {
                    Some(next) => *next,
                    None => {
                        let next = nodes.len();
                        nodes.push(ExclusionNode::default());
                        nodes[current].children.insert(name.to_os_string(), next);
                        next
                    }
                };
                current = next;
            }
            if !has_component {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "exclude paths must name an entry below the scan root",
                ));
            }
            nodes[current].excluded = true;
        }
        Ok(Self { nodes: Arc::new(nodes) })
    }

    pub(crate) fn root_node(&self) -> Option<usize> {
        (!self.nodes[0].children.is_empty()).then_some(0)
    }

    pub(crate) fn child(&self, parent: Option<usize>, name: &[u8]) -> Option<usize> {
        let parent = parent?;
        let name = std::ffi::OsStr::from_bytes(name);
        self.nodes[parent].children.get(name).copied()
    }

    pub(crate) fn excludes(&self, node: usize) -> bool {
        self.nodes[node].excluded
    }

    pub(crate) fn has_children(&self, node: usize) -> bool {
        !self.nodes[node].children.is_empty()
    }
}

impl Default for ScanExclusions {
    fn default() -> Self {
        Self {
            nodes: Arc::new(vec![ExclusionNode::default()]),
        }
    }
}

