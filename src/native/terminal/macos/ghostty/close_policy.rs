use crate::native::terminal::close_policy::{ClosePolicy, PreClosePolicy};

pub(in crate::native::terminal) const CLOSE_POLICY: ClosePolicy = ClosePolicy {
    outlives_owner: true,
    failed_start_identity: true,
    requires_app_incarnation: false,
    pre_close: PreClosePolicy::None,
};
