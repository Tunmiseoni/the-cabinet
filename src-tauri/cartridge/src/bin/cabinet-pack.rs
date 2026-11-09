use cabinet_cartridge::archive::{self, EXTENSION};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage:
  cabinet-pack pack <folder> [--out <file.cart>]
  cabinet-pack inspect <file.cart>";

enum Command {
    Pack {
        source: PathBuf,
        out: Option<PathBuf>,
    },
    Inspect {
        cart: PathBuf,
    },
}

fn parse(args: &[String]) -> Result<Command, String> {
    match args {
        [command, source, rest @ ..] if command == "pack" => {
            let out = match rest {
                [] => None,
                [flag, out] if flag == "--out" => Some(PathBuf::from(out)),
                _ => return Err(USAGE.to_string()),
            };
            Ok(Command::Pack {
                source: PathBuf::from(source),
                out,
            })
        }
        [command, cart] if command == "inspect" => Ok(Command::Inspect {
            cart: PathBuf::from(cart),
        }),
        _ => Err(USAGE.to_string()),
    }
}

fn publisher_line(publisher: Option<&cabinet_cartridge::Publisher>) -> String {
    match publisher {
        Some(publisher) => format!("publisher {}", publisher.fingerprint()),
        None => "unsigned".to_string(),
    }
}

fn run(command: Command) -> cabinet_cartridge::Result<()> {
    match command {
        Command::Pack { source, out } => {
            let manifest_text = std::fs::read_to_string(source.join("cartridge.toml"))?;
            let manifest = cabinet_cartridge::Manifest::parse(&manifest_text, None)?;
            let out = out.unwrap_or_else(|| {
                PathBuf::from(format!("{}-{}.{EXTENSION}", manifest.id, manifest.version))
            });
            let packed = archive::pack(&source, &out, None)?;
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
