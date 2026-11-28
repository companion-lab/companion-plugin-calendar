//! Temporary connectivity probe: reproduces the exact blocking client the
//! OAuth/token code builds and prints the full error cause chain.

use std::time::Duration;

fn chain(e: &dyn std::error::Error) -> String {
    let mut msg = format!("{e}");
    let mut src = e.source();
    while let Some(s) = src {
        msg.push_str(&format!("\n  caused by: {s}"));
        src = s.source();
    }
    msg
}

#[test]
fn probe_google_token_endpoint() {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let result = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[("grant_type", "client_credentials")])
        .send();
    match result {
        Ok(r) => println!("PROBE OK: HTTP {}", r.status()),
        Err(e) => {
            println!("PROBE FAILED:\n{}", chain(&e));
            panic!("probe failed — see cause chain above");
        }
    }
}

#[test]
fn probe_google_calendar_api() {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let result = client
        .get("https://www.googleapis.com/calendar/v3/users/me/calendarList")
        .send();
    match result {
        Ok(r) => println!("PROBE OK: HTTP {}", r.status()),
        Err(e) => {
            println!("PROBE FAILED:\n{}", chain(&e));
            panic!("probe failed — see cause chain above");
        }
    }
}
