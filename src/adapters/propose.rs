//! `DialoguerProposer`: the real `Proposer`, offering the step past a refusal
//! as a yes or no question whose default is to take it.

use dialoguer::Confirm;

use crate::domain::error::HortError;
use crate::ports::Proposer;

/// A `Proposer` that asks through `dialoguer`.
pub struct DialoguerProposer;

impl Proposer for DialoguerProposer {
    fn propose(&self, question: &str) -> Result<bool, HortError> {
        // Anything but an answer, a terminal that went away or an Esc, is taken
        // as a no: the refusal that follows names the command to run instead,
        // while a yes nobody gave would build a sandbox.
        let answer = Confirm::new().with_prompt(question).default(true).interact_opt();
        Ok(matches!(answer, Ok(Some(true))))
    }
}
