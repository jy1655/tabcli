use crate::native::terminal::facts::{
    CloseEvidenceChoice, PasteEnvelope, Registration, SurfaceFacts,
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
use crate::native::session::launch::{BindingBudget, BindingFacts};

pub(in crate::native) const REGISTRATION: Registration = Registration {
    facts: SurfaceFacts {
        // The server is the surface: a user can keep it with windows of their own.
        outlives_owner: true,
        // The creation-proven server incarnation identifies a failed start's surface.
        failed_start_identity: true,
        // The prompt file carries the bracketed-paste markers; `paste-buffer -r` writes
        // them unchanged (measured on tmux 3.6, 2026-11-17).
        paste_envelope: PasteEnvelope::Bracketed,
        guards_dialog_input: false,
    },
    host_command: "native-tmux-host",
    // The pane runs its own host (`native-tmux-host`), not the macOS launch host.
    launch_host: None,
    close_evidence: CloseEvidenceChoice::BridgeCreatedHost,
    // `capture-pane -p` reads the pane's visible screen.
    screen_read_refusal: None,
    guarded_input_refusal: Some(
        "tmux cannot compare the screen and send a key in one call; answer the dialog in its pane",
    ),
    #[cfg(windows)]
    binding: None,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    binding: Some(BindingFacts {
        host: "tmux",
        cancelled: "tmux launch was cancelled or timed out before surface binding",
        budget: BindingBudget::RemainingReceiptCapped,
    }),
};
