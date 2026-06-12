//! `StdinConfirmer`: the real `Confirmer`, asking a plain yes/no question on
//! stdin. It prints the rendered message, reads one line, and accepts `y` or
//! `yes` (case-insensitive) as yes, anything else as no. This is a bare prompt,
//! not a dialoguer one; dialoguer is reserved for onboarding.

use std::io::{self, Write};

use crate::domain::error::HortError;
use crate::ports::Confirmer;

/// A `Confirmer` that reads a yes/no answer from stdin.
pub struct StdinConfirmer;

impl Confirmer for StdinConfirmer {
    fn confirm(&self, message: &str) -> Result<bool, HortError> {
        print!("{message} [y/N] ");
        let _ = io::stdout().flush();

        let mut answer = String::new();
        // A confirmation we could not read is not a yes: decline rather than
        // proceed with a destructive action on an unreadable stdin.
        if io::stdin().read_line(&mut answer).is_err() {
            return Ok(false);
        }

        let answer = answer.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
}
