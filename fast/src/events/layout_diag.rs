use super::word;
use std::path::PathBuf;

pub fn run(inputs: Vec<PathBuf>) {
    word::layout_diag::run(inputs);
}
