//! The shipped example config must load, validate and normalise.
#[tokio::test]
async fn the_example_config_is_a_config_this_build_accepts() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config.json");
    std::fs::copy("config/config.example.json", &file).unwrap();

    let store = chtting_relay::config::ConfigStore::load(&file)
        .await
        .expect("the example config must load");
    let cfg = store.current();

    let phone = cfg.keys.iter().find(|k| k.id == "key_phone").unwrap();
    assert!(phone.kind.is_private());
    let friend = cfg.keys.iter().find(|k| k.id == "key_friend").unwrap();
    assert!(!friend.kind.is_private());
    assert_eq!(friend.billing.tax_percent, Some(11.0));
    assert!(friend.billing.auto_invoice);
    assert_eq!(cfg.security.private_user_id, "fingerprint");
    assert!(cfg.security.dashboard_origin_guard);
    assert_eq!(cfg.billing.number_prefix, "INV");
    assert_eq!(store.keys().len(), cfg.keys.len());
}
