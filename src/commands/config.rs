//! `config`: onboarding. Detect the environment, prompt inline (`dialoguer`),
//! write commented JSONC via `generate_config` (D-11). Auto-runs on first use
//! (no config + TTY); no-op under a non-TTY stdin. Post-skeleton (ADR-0012).
//!
//! See backlog C-06.

// TODO(C-06): the ConfigCommand coordinator.
