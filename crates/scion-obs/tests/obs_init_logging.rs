//! SPEC 15.1: `init_logging` is idempotent and coexists with `LogCapture` installed later in
//! the same process (its own test binary, because the subscriber is process-global).

use scion_obs::capture::LogCapture;
use scion_obs::{LogFormat, RpcLogEvent, init_logging};

#[test]
fn init_logging_then_capture() {
    let _g1 = init_logging(LogFormat::Json);
    let _g2 = init_logging(LogFormat::Text); // no-op, no panic
    let cap = LogCapture::install();
    RpcLogEvent {
        request_id: "req-init-77e1".into(),
        rpc: "WhoAmI".into(),
        tier: "server",
        principal: Some("service:ci".into()),
        repo: "r".into(),
        status: "OK".into(),
        duration_ms: 1,
        ..Default::default()
    }
    .emit();
    let events = cap.events();
    let e = events
        .iter()
        .find(|e| e["request_id"] == "req-init-77e1")
        .expect("event captured");
    assert_eq!(e["principal"], "service:ci");
}
