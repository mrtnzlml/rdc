//! `RDC_TRACE_HTTP` — the opt-in per-attempt HTTP trace in `api::retry`.
//!
//! This MUST stay the only test in this binary. The trace sink is a
//! process-wide `OnceLock` initialised from the environment on the first
//! request, so a second test could neither observe the disabled state nor
//! redirect the sink — and `set_var` is only sound while no other thread is
//! reading the environment.

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn trace_writes_one_csv_line_per_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let trace_path = dir.path().join("http.csv");

    // SAFETY: the only test in this binary, and nothing has spawned a thread
    // that reads the environment yet (the mock server starts below).
    unsafe { std::env::set_var("RDC_TRACE_HTTP", &trace_path) };

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    // 429 with `Retry-After: 0` is retriable and sleeps for zero seconds, so
    // this burns every attempt without making the test slow.
    Mock::given(method("GET"))
        .and(path("/throttled"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
        .mount(&server)
        .await;

    let http = reqwest::Client::new();

    let ok_url = format!("{}/ok", server.uri());
    let r = rdc::api::retry::send_with_retry(|| http.get(&ok_url), "GET /ok", None, None)
        .await
        .expect("the 200 request must succeed");
    assert_eq!(r.status(), 200);

    let throttled_url = format!("{}/throttled", server.uri());
    let r = rdc::api::retry::send_with_retry(
        || http.get(&throttled_url),
        "GET /throttled",
        None,
        None,
    )
    .await
    .expect("a retriable status is returned, not an error");
    assert_eq!(r.status(), 429);

    let body = std::fs::read_to_string(&trace_path)
        .expect("setting RDC_TRACE_HTTP must create the trace file");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(
        lines.len(),
        6,
        "one line per ATTEMPT: 1 for /ok + 5 for /throttled, got {lines:?}"
    );

    // `desc` is the last field and is not quoted, so split into exactly 5.
    let fields: Vec<Vec<&str>> = lines.iter().map(|l| l.splitn(5, ',').collect()).collect();
    for f in &fields {
        assert_eq!(f.len(), 5, "epoch_ms,limiter_wait_ms,duration_ms,status,desc");
        assert!(
            f[0].parse::<f64>().unwrap() > 1_700_000_000_000.0,
            "epoch_ms must be a real wall-clock millisecond stamp, got {}",
            f[0]
        );
        assert_eq!(f[1], "0.0", "no limiter was passed, so no gate wait");
        assert!(f[2].parse::<f64>().unwrap() >= 0.0, "duration_ms must parse");
    }
    assert_eq!((fields[0][3], fields[0][4]), ("200", "GET /ok"));
    for f in &fields[1..] {
        assert_eq!((f[3], f[4]), ("429", "GET /throttled"));
    }
}
