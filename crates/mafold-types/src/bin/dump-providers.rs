//! Print the provider pack the publish pipeline signs and uploads.
//!
//! The registry is still AUTHORED here, in Rust, with types and tests — what
//! changed is only how it reaches clients. This binary is the seam: one command
//! turns the authored const into the exact bytes `publish-providers.yml` signs,
//! so nobody hand-maintains a JSON copy that could drift from the rows the
//! tests check.
//!
//!   cargo run -p mafold-types --bin dump-providers            # the pack
//!   cargo run -p mafold-types --bin dump-providers -- digest 42   # what to sign
//!
//! `digest` prints the SHA-256 as lowercase hex for a given version, because
//! shell pipelines cannot compute it and signing anything else would produce a
//! pack that verifies nowhere.

use mafold_types::connections::{provider_infos, providers_checksum, providers_digest};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let providers = provider_infos();

    match args.first().map(String::as_str) {
        Some("digest") => {
            let version: u32 = args
                .get(1)
                .and_then(|v| v.parse().ok())
                .expect("usage: dump-providers digest <version>");
            let d = providers_digest(version, &providers);
            println!("{}", d.iter().map(|b| format!("{b:02x}")).collect::<String>());
        }
        Some("checksum") => println!("{}", providers_checksum(&providers)),
        Some("count") => println!("{}", providers.len()),
        None => println!(
            "{}",
            serde_json::to_string(&providers).expect("the registry serialises")
        ),
        Some(other) => {
            eprintln!("unknown subcommand `{other}` — try digest/checksum/count, or none");
            std::process::exit(2);
        }
    }
}
