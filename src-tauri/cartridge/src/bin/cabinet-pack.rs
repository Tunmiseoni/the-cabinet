use cabinet_cartridge::archive::{self, EXTENSION};
use cabinet_cartridge::signature::{KeySigner, Signer, PASSWORD_ENV};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage:
  cabinet-pack pack <folder> [--out <file.cart>] [--key <secret.key>]
  cabinet-pack inspect <file.cart>
  cabinet-pack keygen <secret.key> <publisher.pub>

An encrypted secret key takes its password from CABINET_PACK_KEY_PASSWORD;
keygen encrypts the new key with it when it is set.";

enum Command {
    Pack {
        source: PathBuf,
        out: Option<PathBuf>,
        key: Option<PathBuf>,
    },
    Inspect {
        cart: PathBuf,
    },
    Keygen {
        secret: PathBuf,
        public: PathBuf,
    },
}

fn parse(args: &[String]) -> Result<Command, String> {
    match args {
        [command, source, rest @ ..] if command == "pack" => {
            let (mut out, mut key) = (None, None);
            let mut flags = rest.iter();
            while let Some(flag) = flags.next() {
                let value = flags.next().ok_or_else(|| USAGE.to_string())?;
                match flag.as_str() {
                    "--out" if out.is_none() => out = Some(PathBuf::from(value)),
                    "--key" if key.is_none() => key = Some(PathBuf::from(value)),
                    _ => return Err(USAGE.to_string()),
                }
            }
            Ok(Command::Pack {
                source: PathBuf::from(source),
                out,
                key,
            })
        }
        [command, cart] if command == "inspect" => Ok(Command::Inspect {
            cart: PathBuf::from(cart),
        }),
        [command, secret, public] if command == "keygen" => Ok(Command::Keygen {
            secret: PathBuf::from(secret),
            public: PathBuf::from(public),
        }),
        _ => Err(USAGE.to_string()),
    }
}

fn password() -> Option<String> {
    std::env::var(PASSWORD_ENV)
        .ok()
        .filter(|password| !password.is_empty())
}

fn write_new(path: &Path, text: &str, private: bool) -> cabinet_cartridge::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options.open(path).map_err(|err| {
        cabinet_cartridge::Error::Archive(format!("cannot create {}: {err}", path.display()))
    })?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

fn publisher_line(publisher: Option<&cabinet_cartridge::Publisher>) -> String {
    match publisher {
        Some(publisher) => format!("publisher {}", publisher.fingerprint()),
        None => "unsigned".to_string(),
    }
}

fn run(command: Command) -> cabinet_cartridge::Result<()> {
    match command {
        Command::Pack { source, out, key } => {
            let manifest_text = std::fs::read_to_string(source.join("cartridge.toml"))?;
            let manifest = cabinet_cartridge::Manifest::parse(&manifest_text, None)?;
            let out = out.unwrap_or_else(|| {
                PathBuf::from(format!("{}-{}.{EXTENSION}", manifest.id, manifest.version))
            });
            let signer = match key {
                Some(key) => Some(KeySigner::from_key_file(
                    &std::fs::read_to_string(&key)?,
                    password(),
                )?),
                None => None,
            };
            let packed = archive::pack(
                &source,
                &out,
                signer.as_ref().map(|signer| signer as &dyn Signer),
            )?;
            println!("packed {}", out.display());
            println!("id {}", packed.manifest.id);
            println!("version {}", packed.manifest.version);
            println!("digest {}", packed.digest);
            println!("{}", publisher_line(packed.publisher.as_ref()));
        }
        Command::Inspect { cart } => {
            let inspected = archive::inspect(&cart, None)?;
            println!("id {}", inspected.manifest.id);
            println!("version {}", inspected.manifest.version);
            println!("digest {}", inspected.digest);
            println!("{}", publisher_line(inspected.publisher.as_ref()));
            for entry in &inspected.entries {
                println!("{}  {}  {}", entry.sha256, entry.size, entry.path);
            }
        }
        Command::Keygen { secret, public } => {
            if public.exists() {
                return Err(cabinet_cartridge::Error::Archive(format!(
                    "{} already exists",
                    public.display()
                )));
            }
            let (secret_text, public_text) = KeySigner::generate(password())?;
            write_new(&secret, &secret_text, true)?;
            write_new(&public, &public_text, false)?;
            let publisher = cabinet_cartridge::Publisher::parse(&public_text)?;
            println!("secret key {}", secret.display());
            println!("public key {}", public.display());
            println!("publisher {}", publisher.fingerprint());
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse(&args) {
        Ok(command) => command,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cabinet-pack: {err}");
            ExitCode::FAILURE
        }
    }
}
