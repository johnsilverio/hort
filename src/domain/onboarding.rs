//! Onboarding decision as a pure generator (ADR-0012): `generate_config(caps,
//! answers) -> (Config, Vec<Warning>)`. A capability the host lacks is emitted
//! as commented-out JSONC with how to enable it, never a silent promise. The
//! interactive prompts and the file write are effects (in `adapters`/`commands`).
//!
//! See backlog D-11.

// TODO(D-11): the pure config generator over `Capabilities` + `OnboardingAnswers`.
