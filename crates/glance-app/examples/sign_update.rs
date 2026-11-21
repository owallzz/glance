//! Sign files with a throwaway minisign key, for testing the updater against
//! a fake release (never use this for real releases; release.yml signs those).
//!
//!   cargo run -p glance-app --example sign_update -- <out-dir> <file>...
//!
//! Writes `<out-dir>/test.pub` (pass its contents as GLANCE_UPDATE_PUBKEY
//! together with GLANCE_UPDATE_URL) and `<file>.minisig` next to each file,
//! with the same trusted comment release.yml writes.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((out, files)) = args.split_first() else {
        eprintln!("usage: sign_update <out-dir> <file>...");
        std::process::exit(2);
    };
    let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
    std::fs::create_dir_all(out).expect("out dir");
    let pub_path = std::path::Path::new(out).join("test.pub");
    std::fs::write(&pub_path, kp.pk.to_box().expect("pk").to_string()).expect("write key");
    println!("{}", pub_path.display());
    for file in files {
        let path = std::path::Path::new(file);
        let name = path.file_name().expect("file name").to_string_lossy();
        let comment = format!("timestamp:0\tfile:{name}\thashed");
        let data = std::fs::File::open(path).expect("open file");
        let sig = minisign::sign(Some(&kp.pk), &kp.sk, data, Some(&comment), None).expect("sign");
        let sig_path = format!("{file}.minisig");
        std::fs::write(&sig_path, sig.to_string()).expect("write signature");
        println!("{sig_path}");
    }
}
