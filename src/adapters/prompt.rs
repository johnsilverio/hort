//! `DialoguerPrompter`: the real `Prompter`, asking the onboarding questions
//! through `dialoguer`'s inline prompts. Used by `config` and by nothing else;
//! the yes/no confirmations of `down` and `prune` are a bare read off stdin.

use dialoguer::{Confirm, Input, MultiSelect};

use crate::domain::error::HortError;
use crate::ports::Prompter;

/// A `Prompter` that asks with `dialoguer`'s inline prompts.
pub struct DialoguerPrompter;

impl Prompter for DialoguerPrompter {
    fn confirm(&self, question: &str) -> Result<bool, HortError> {
        Confirm::new().with_prompt(question).interact().map_err(no_terminal)
    }

    fn ask(&self, question: &str) -> Result<String, HortError> {
        Input::<String>::new().with_prompt(question).interact_text().map_err(no_terminal)
    }

    fn choose(&self, question: &str, options: &[String]) -> Result<Vec<String>, HortError> {
        // A MultiSelect handed no items errors instead of answering that
        // nothing was picked, so a host carrying none of the candidates would
        // lose the whole dialogue over one question with nothing behind it.
        if options.is_empty() {
            return Ok(Vec::new());
        }

        let picked = MultiSelect::new()
            .with_prompt(question)
            .items(options)
            .interact()
            .map_err(no_terminal)?;
        Ok(picked.into_iter().map(|option| options[option].clone()).collect())
    }
}

/// The only failure `dialoguer` reports is an I/O error on the terminal, and a
/// terminal hort cannot ask through is the condition this error already names,
/// whether stdin was never one or stopped being one part way through.
fn no_terminal(_: dialoguer::Error) -> HortError {
    HortError::ConfigNeedsTerminal
}
