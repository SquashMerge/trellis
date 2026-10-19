use super::*;
#[test]
fn maintenance_failures_are_sanitized_and_rate_limited_without_sleep() {
    let start = Instant::now();
    let mut limiter = MaintenanceErrorLimiter::default();
    assert!(limiter.permit(start));
    assert!(!limiter.permit(start + Duration::from_millis(20)));
    assert!(!limiter.permit(start + Duration::from_secs(59)));
    assert!(limiter.permit(start + Duration::from_secs(60)));
    let sensitive = "workspace/private/path-and-source";
    let opaque = anyhow::anyhow!("{sensitive}");
    let (category, code) = maintenance_error_class(&opaque);
    assert_eq!((category, code), ("other", 0));
    let notice = maintenance_error_notice(category, code);
    assert_eq!(notice, "maintenance tick failed: category=other code=0\n");
    assert!(!notice.contains(sensitive));

    let db = rusqlite::Connection::open_in_memory().unwrap();
    let sqlite = db
        .execute("INSERT INTO private_missing_table(id) VALUES (1)", [])
        .unwrap_err();
    let wrapped = anyhow::Error::new(sqlite).context(sensitive);
    let (category, code) = maintenance_error_class(&wrapped);
    assert_eq!(category, "sqlite");
    assert_ne!(code, 0);
    let notice = maintenance_error_notice(category, code);
    assert!(!notice.contains(sensitive));
    assert!(!notice.contains("private_missing_table"));
}

#[test]
fn deferred_preemption_busy_and_success_keep_max_age_without_sleep() {
    let now = Instant::now();
    let mut stats = MaintenanceTelemetry {
        deferred_since: Some(now - Duration::from_secs(2)),
        ..Default::default()
    };
    stats.busy_attempts = 1; // Store's exact counter is sampled by the scheduler.
    let (age, max) = stats.deferred(now);
    assert!(age >= 2_000 && max >= 2_000);
    stats.observe_due_age(Some(899));
    assert_eq!(stats.max_deferred_age_s, 0);
    stats.observe_due_age(Some(905));
    assert_eq!(stats.max_deferred_age_s, 5);
    stats.observe_due_age(None);
    assert_eq!(stats.max_deferred_age_s, 5);
    assert_eq!((stats.preemptions, stats.busy_attempts), (1, 1));
    let (age, max) = stats.progressed(now);
    assert!(age >= 2_000 && max >= 2_000);
    assert_eq!((stats.successful_units, stats.preemptions), (1, 1));
    assert!(stats.deferred_since.is_none());
}
