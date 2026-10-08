fn main() {
    let mut args = std::env::args().skip(1);
    let mut token = None;
    while let Some(arg) = args.next() {
        if arg == "--readiness-token" {
            token = args.next();
        }
    }
    let token = token.expect("readiness token");
    let cwd = std::env::current_dir().expect("current dir");
    let version = std::fs::read_to_string(cwd.join("VERSION.txt")).expect("version");
    let path = cwd.join("updates/readiness").join(format!("{token}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).expect("readiness dir");
    std::fs::write(path, format!("{{\"protocol\":1,\"version\":\"{}\",\"ready\":true}}\n", version.trim()))
        .expect("readiness marker");
    std::thread::sleep(std::time::Duration::from_millis(2500));
}
