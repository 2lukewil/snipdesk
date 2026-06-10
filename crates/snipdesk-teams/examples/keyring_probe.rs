//! Cross-process keychain probe. Run with `store` in one process and
//! `load` in another; persistence across processes proves the real OS
//! keystore is active (the mock store would lose the value).
//! `cleanup` removes the probe entry afterwards.

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let url = "probe://keyring-test";
    match mode.as_str() {
        "store" => {
            snipdesk_teams::credentials::store(url, "probe-token-123").expect("store");
            println!("stored");
        }
        "load" => match snipdesk_teams::credentials::load(url).expect("load") {
            Some(t) => println!("loaded: {t}"),
            None => println!("MISSING"),
        },
        "cleanup" => {
            snipdesk_teams::credentials::delete(url).expect("delete");
            println!("cleaned");
        }
        other => eprintln!("usage: keyring_probe store|load|cleanup (got {other:?})"),
    }
}
