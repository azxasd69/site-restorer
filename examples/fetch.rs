use std::time::{Duration, Instant};

fn main() {
    let url = std::env::args().nth(1).unwrap();
    let cfg = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(90)))
        .build();
    let agent = ureq::Agent::new_with_config(cfg);
    let t = Instant::now();
    match agent.get(&url).header("User-Agent", "SiteRestorer/2.0").call() {
        Ok(mut r) => {
            let status = r.status().as_u16();
            println!("статус {status} за {:?}", t.elapsed());
            match r.body_mut().read_to_vec() {
                Ok(b) => println!("тело {} байт, всего {:?}", b.len(), t.elapsed()),
                Err(e) => println!("ошибка чтения: {e}"),
            }
        }
        Err(e) => println!("ошибка: {e} за {:?}", t.elapsed()),
    }
}
