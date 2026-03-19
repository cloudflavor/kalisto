use std::fmt::{Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Display for Status {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Fail => write!(f, "FAIL"),
            Self::Skip => write!(f, "SKIP"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Result {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl Result {
    pub fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Pass,
            detail: detail.into(),
        }
    }

    pub fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
        }
    }

    pub fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Skip,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub results: Vec<Result>,
}

impl Report {
    pub fn has_failures(&self) -> bool {
        self.results
            .iter()
            .any(|result| result.status == Status::Fail)
    }

    pub fn summary(&self) -> String {
        let passed = self
            .results
            .iter()
            .filter(|result| result.status == Status::Pass)
            .count();
        let failed = self
            .results
            .iter()
            .filter(|result| result.status == Status::Fail)
            .count();
        let skipped = self
            .results
            .iter()
            .filter(|result| result.status == Status::Skip)
            .count();

        format!("summary: {passed} passed, {failed} failed, {skipped} skipped")
    }
}

#[cfg(test)]
mod tests {
    use super::Status;

    #[test]
    fn status_debug_output_is_stable() {
        let status = Status::Pass;
        assert_eq!(format!("{status:?}"), "Pass");
    }
}
