//! The `Analyzer` abstraction: the native Rust pipeline, or (only if the native one cannot meet
//! the synthetic gate) an essentia Python sidecar. See docs/decisions/ws3.md.

use std::path::Path;

use crate::pipeline::{AnalyzeError, AnalyzeOptions, TrackAnalysis, analyze_file};

pub trait Analyzer: Send + Sync {
    fn name(&self) -> &'static str;
    fn analyze(&self, path: &Path) -> Result<TrackAnalysis, AnalyzeError>;
}

pub struct NativeAnalyzer {
    pub opts: AnalyzeOptions,
}

impl Analyzer for NativeAnalyzer {
    fn name(&self) -> &'static str {
        crate::persist::ANALYZER_NATIVE
    }
    fn analyze(&self, path: &Path) -> Result<TrackAnalysis, AnalyzeError> {
        analyze_file(path, &self.opts)
    }
}
