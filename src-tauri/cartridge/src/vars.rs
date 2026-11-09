#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Var<'a> {
    Cartridge,
    Data,
    Platform,
    Setting(&'a str),
    Asset(&'a str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    Text(&'a str),
    Var(Var<'a>),
}

fn var(name: &str) -> Result<Var<'_>, String> {
    match name {
        "cartridge" => Ok(Var::Cartridge),
        "data" => Ok(Var::Data),
        "platform" => Ok(Var::Platform),
        _ => match name.split_once(':') {
            Some(("setting", key)) if !key.is_empty() => Ok(Var::Setting(key)),
            Some(("asset", id)) if !id.is_empty() => Ok(Var::Asset(id)),
            _ => Err(format!("unknown variable ${{{name}}}")),
        },
    }
}

pub fn parse(template: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut pieces = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            pieces.push(Piece::Text(&rest[..start]));
        }
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated variable in \"{template}\""))?;
        pieces.push(Piece::Var(var(&after[..end])?));
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest));
    }
    Ok(pieces)
}

pub fn expand<F>(template: &str, mut resolve: F) -> Result<String, String>
where
    F: FnMut(Var<'_>) -> Result<String, String>,
{
    let mut out = String::new();
    for piece in parse(template)? {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Var(var) => out.push_str(&resolve(var)?),
        }
    }
    Ok(out)
}

pub fn vars(template: &str) -> Result<Vec<Var<'_>>, String> {
    Ok(parse(template)?
        .into_iter()
        .filter_map(|piece| match piece {
            Piece::Var(var) => Some(var),
            Piece::Text(_) => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_and_variables() {
        assert_eq!(
            parse("${setting:rom_dir}/sfiii3nr1.zip").unwrap(),
            vec![
                Piece::Var(Var::Setting("rom_dir")),
                Piece::Text("/sfiii3nr1.zip")
            ]
        );
        assert_eq!(
            parse("a$b${data}c").unwrap(),
            vec![Piece::Text("a$b"), Piece::Var(Var::Data), Piece::Text("c")]
        );
        assert_eq!(parse("").unwrap(), vec![]);
    }

    #[test]
    fn rejects_unknown_and_unterminated_variables() {
        assert!(parse("${HOME}/x").is_err());
        assert!(parse("${setting:}").is_err());
        assert!(parse("${data").is_err());
        assert!(parse("${env:PATH}").is_err());
    }

    #[test]
    fn expands_through_the_resolver() {
        let expanded = expand(
            "${data}/assets/${asset:core}-${platform}",
            |var| match var {
                Var::Data => Ok("/d".to_string()),
                Var::Asset(id) => Ok(id.to_uppercase()),
                Var::Platform => Ok("linux-x86_64".to_string()),
                other => Err(format!("{other:?}")),
            },
        )
        .unwrap();
        assert_eq!(expanded, "/d/assets/CORE-linux-x86_64");
        assert!(expand("${cartridge}", |_| Err("nope".to_string())).is_err());
    }
}
