use crate::complete::CompletionState;
use crate::history::FuzzyMatch;
use crate::line::LineBuffer;

pub enum Mode {
    Normal,
    Completion(CompletionMode),
    HistorySearch(HistorySearch),
    DirPicker(DirPicker),
}

pub struct CompletionMode {
    pub state: CompletionState,
    pub base_line: LineBuffer,
}

pub struct HistorySearch {
    pub query: LineBuffer,
    pub matches: Vec<FuzzyMatch>,
    pub candidates: Vec<usize>,
    pub scratch: Vec<usize>,
    pub candidate_stack: Vec<(usize, Vec<usize>)>,
    pub spare_candidates: Vec<Vec<usize>>,
    pub previous_query: String,
    pub selected: usize,
    pub saved_line: String,
}

pub struct DirPicker {
    pub entries: Vec<String>,
    pub selected: usize,
}

impl Mode {
    pub fn name(&self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Completion(_) => "completion",
            Mode::HistorySearch(_) => "history_search",
            Mode::DirPicker(_) => "dir_picker",
        }
    }
}
