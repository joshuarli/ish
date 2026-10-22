use crate::complete::CompletionState;
use crate::history::FuzzyMatch;
use crate::line::LineBuffer;

pub(crate) enum Mode {
    Normal,
    Completion(CompletionMode),
    HistorySearch(HistorySearch),
    DirPicker(DirPicker),
}

pub(crate) struct CompletionMode {
    pub(crate) state: CompletionState,
    pub(crate) base_line: LineBuffer,
}

pub(crate) struct HistorySearch {
    pub(crate) query: LineBuffer,
    pub(crate) matches: Vec<FuzzyMatch>,
    pub(crate) candidates: Vec<usize>,
    pub(crate) scratch: Vec<usize>,
    pub(crate) candidate_stack: Vec<(usize, Vec<usize>)>,
    pub(crate) spare_candidates: Vec<Vec<usize>>,
    pub(crate) previous_query: String,
    pub(crate) selected: usize,
    pub(crate) saved_line: String,
}

pub(crate) struct DirPicker {
    pub(crate) entries: Vec<String>,
    pub(crate) selected: usize,
}

impl Mode {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Completion(_) => "completion",
            Mode::HistorySearch(_) => "history_search",
            Mode::DirPicker(_) => "dir_picker",
        }
    }
}
