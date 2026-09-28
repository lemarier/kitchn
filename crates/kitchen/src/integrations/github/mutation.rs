//! Repository inventory comparison for setup; existing labels stay untouched.
use super::{IntegrationError, Label};
use crate::contracts::LabelDefinition;
impl LabelDefinition {
    /// Compare a fully fetched repository label inventory without mutating it.
    ///
    /// # Errors
    /// Rejects invalid input and ambiguous duplicate names.
    pub fn inspect(&self, labels: &[Label]) -> Result<LabelSetup, IntegrationError> {
        self.validate()?;
        let mut matches = labels
            .iter()
            .filter(|label| label.name.eq_ignore_ascii_case(&self.name));
        let found = matches.next();
        if matches.next().is_some() {
            return Err(IntegrationError::Unknown);
        }
        Ok(match found {
            None => LabelSetup::Missing,
            Some(label)
                if label.color.eq_ignore_ascii_case(&self.color)
                    && label.description.as_deref().unwrap_or("") == self.description =>
            {
                LabelSetup::Present
            }
            Some(_) => LabelSetup::Conflict,
        })
    }
}
/// Result of previewing one desired workflow label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelSetup {
    /// Creation is needed, through persisted intent only.
    Missing,
    /// Already configured; no effect is needed.
    Present,
    /// Existing definition differs; report and leave it untouched.
    Conflict,
}
