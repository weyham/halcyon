use halcyon_core::update::verify::verify_minisign;
use minisign::{KeyPair, SecretKey, SignatureBox};
use std::fs;
use std::io::Cursor;
use std::path::Path;

fn main() {
    if let Err(error) = run() {
        eprintln!("halcyon-sign: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("test-keygen") => test_keygen(&args[1..]),
        Some("sign") => sign(&args[1..]),
        Some("verify") => verify(&args[1..]),
        _ => Err("usage: halcyon-sign test-keygen --dir DIR | sign --secret-key PATH --input PATH --output PATH | verify --public-key PATH --input PATH --signature PATH".into()),
    }
}

fn test_keygen(args: &[String]) -> Result<(), String> {
    let dir = arg_value(args, "--dir").ok_or("test-keygen 缺少 --dir")?;
    let directory = Path::new(&dir);
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let keys = KeyPair::generate_unencrypted_keypair().map_err(|error| error.to_string())?;
    fs::write(directory.join("halcyon-release.key"), keys.sk.to_bytes())
        .map_err(|error| error.to_string())?;
    fs::write(directory.join("halcyon-release.pub"), keys.pk.to_base64())
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn sign(args: &[String]) -> Result<(), String> {
    let secret_path = arg_value(args, "--secret-key").ok_or("sign 缺少 --secret-key")?;
    let input_path = arg_value(args, "--input").ok_or("sign 缺少 --input")?;
    let output_path = arg_value(args, "--output").ok_or("sign 缺少 --output")?;
    let trusted_comment = arg_value(args, "--trusted-comment");
    let secret_bytes = fs::read(&secret_path).map_err(|error| error.to_string())?;
    let secret = if secret_bytes.starts_with(b"untrusted comment:") {
        let text = String::from_utf8(secret_bytes).map_err(|error| error.to_string())?;
        SecretKey::from_box(text.into(), None).map_err(|error| error.to_string())?
    } else {
        SecretKey::from_bytes(&secret_bytes).map_err(|error| error.to_string())?
    };
    let data = fs::read(&input_path).map_err(|error| error.to_string())?;
    let signature: SignatureBox = minisign::sign(
        None,
        &secret,
        Cursor::new(&data),
        trusted_comment.as_deref(),
        None,
    )
    .map_err(|error| error.to_string())?;
    fs::write(output_path, String::from(signature)).map_err(|error| error.to_string())?;
    Ok(())
}

fn verify(args: &[String]) -> Result<(), String> {
    let public_path = arg_value(args, "--public-key").ok_or("verify 缺少 --public-key")?;
    let input_path = arg_value(args, "--input").ok_or("verify 缺少 --input")?;
    let signature_path = arg_value(args, "--signature").ok_or("verify 缺少 --signature")?;
    let public_key = fs::read_to_string(public_path).map_err(|error| error.to_string())?;
    let data = fs::read(input_path).map_err(|error| error.to_string())?;
    let signature = fs::read_to_string(signature_path).map_err(|error| error.to_string())?;
    verify_minisign(&public_key, &data, &signature).map_err(|error| error.to_string())?;
    println!("signature verified");
    Ok(())
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].clone())
}
