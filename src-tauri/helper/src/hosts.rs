use std::io::Write;
use std::net::Ipv4Addr;
use std::ops::Range;
use std::path::Path;

pub const OPEN: &str = "# >>> the-cabinet ";
pub const CLOSE: &str = "# <<< the-cabinet ";
const MAX_SIZE: u64 = 1024 * 1024;
const TEMP_NAME: &str = "hosts.the-cabinet.tmp";

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn blocks(lines: &[&str]) -> Vec<(String, Range<usize>)> {
    let mut found = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if let Some(id) = lines[index].trim().strip_prefix(OPEN) {
            let close = format!("{CLOSE}{id}");
            let end = lines[index + 1..]
                .iter()
                .take_while(|line| !line.trim().starts_with(OPEN))
                .position(|line| line.trim() == close);
            if let Some(offset) = end {
                let end = index + 1 + offset;
                found.push((id.to_string(), index..end + 1));
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    found
}

pub fn ids(text: &str) -> Vec<String> {
    blocks(&lines(text)).into_iter().map(|(id, _)| id).collect()
}

pub fn remove(text: &str, id: &str) -> String {
    let lines = lines(text);
    let ranges: Vec<Range<usize>> = blocks(&lines)
        .into_iter()
        .filter(|(found, _)| found == id)
        .map(|(_, range)| range)
        .collect();
    lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !ranges.iter().any(|range| range.contains(index)))
        .map(|(_, line)| *line)
        .collect()
}

fn line_end(text: &str) -> &'static str {
    if text.contains("\r\n") || (text.is_empty() && cfg!(windows)) {
        "\r\n"
    } else {
        "\n"
    }
}

pub fn add(text: &str, id: &str, names: &[String], target: Ipv4Addr) -> String {
    let end = line_end(text);
    let mut out = remove(text, id);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(end);
    }
    out.push_str(&format!("{OPEN}{id}{end}"));
    for name in names {
        out.push_str(&format!("{target} {name}{end}"));
    }
    out.push_str(&format!("{CLOSE}{id}{end}"));
    out
}

pub fn outside(text: &str) -> Vec<(String, String)> {
    let lines = lines(text);
    let ranges: Vec<Range<usize>> = blocks(&lines).into_iter().map(|(_, range)| range).collect();
    let mut mapped = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if ranges.iter().any(|range| range.contains(&index)) {
            continue;
        }
        let mut fields = line.split('#').next().unwrap_or("").split_whitespace();
        let Some(address) = fields.next() else {
            continue;
        };
        for name in fields {
            mapped.push((name.to_ascii_lowercase(), address.to_string()));
        }
    }
    mapped
}

pub fn read(path: &Path) -> Result<String, String> {
    let fail = |err: &dyn std::fmt::Display| format!("cannot read {}: {err}", path.display());
    let size = std::fs::metadata(path).map_err(|err| fail(&err))?.len();
    if size > MAX_SIZE {
        return Err(fail(&"the file is unexpectedly large"));
    }
    std::fs::read_to_string(path).map_err(|err| fail(&err))
}

pub fn write(path: &Path, text: &str) -> Result<(), String> {
    let fail = |err: std::io::Error| format!("cannot write {}: {err}", path.display());
    let permissions = std::fs::metadata(path).map_err(fail)?.permissions();
    let temp = path.with_file_name(TEMP_NAME);
    let _ = std::fs::remove_file(&temp);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.set_permissions(permissions)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    written.map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        fail(err)
    })
}

pub fn flush_cache() {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let ipconfig = crate::win::windows_dir()
            .map(|dir| dir.join(r"System32\ipconfig.exe"))
            .unwrap_or_else(|_| r"C:\Windows\System32\ipconfig.exe".into());
        match std::process::Command::new(&ipconfig)
            .arg("/flushdns")
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => log::warn!("{} exited with {}", ipconfig.display(), output.status),
            Err(err) => log::warn!("cannot run {}: {err}", ipconfig.display()),
        }
    }
    #[cfg(target_os = "macos")]
    for (program, args) in [
        ("/usr/bin/dscacheutil", &["-flushcache"][..]),
        ("/usr/bin/killall", &["-HUP", "mDNSResponder"][..]),
    ] {
        match std::process::Command::new(program).args(args).status() {
            Ok(status) if status.success() => {}
            Ok(status) => log::warn!("{program} exited with {status}"),
            Err(err) => log::warn!("cannot run {program}: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "127.0.0.1\tlocalhost\n::1             localhost\n\n# >>> other-tool\n127.0.0.1 api.example-game.net\n# <<< other-tool\n";

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn adds_and_removes_a_fenced_block_leaving_the_rest_untouched() {
        let target = Ipv4Addr::new(100, 64, 0, 1);
        let added = add(BASE, "7", &names(&["cabinet-test.invalid"]), target);
        assert_eq!(
            added,
            format!(
                "{BASE}# >>> the-cabinet 7\n100.64.0.1 cabinet-test.invalid\n# <<< the-cabinet 7\n"
            )
        );
        assert_eq!(ids(&added), vec!["7"]);
        assert_eq!(remove(&added, "7"), BASE);
        assert_eq!(remove(BASE, "7"), BASE);
    }

    #[test]
    fn adding_twice_keeps_one_block_and_blocks_are_independent() {
        let target = Ipv4Addr::LOCALHOST;
        let once = add(BASE, "1", &names(&["cabinet-test.invalid"]), target);
        assert_eq!(
            add(&once, "1", &names(&["cabinet-test.invalid"]), target),
            once
        );
        let two = add(&once, "2", &names(&["cabinet-test.invalid"]), target);
        assert_eq!(ids(&two), vec!["1", "2"]);
        assert_eq!(ids(&remove(&two, "1")), vec!["2"]);
        assert_eq!(remove(&remove(&two, "1"), "2"), BASE);
    }

    #[test]
    fn a_missing_final_newline_is_completed_first() {
        let added = add(
            "127.0.0.1 localhost",
            "1",
            &names(&["a.invalid"]),
            Ipv4Addr::LOCALHOST,
        );
        assert!(added.starts_with("127.0.0.1 localhost\n# >>> the-cabinet 1\n"));
        assert_eq!(
            add("", "1", &names(&["a.invalid"]), Ipv4Addr::LOCALHOST)
                .lines()
                .count(),
            3
        );
    }

    #[test]
    fn an_unclosed_fence_is_never_removed() {
        let broken = format!("{BASE}# >>> the-cabinet 3\n127.0.0.1 kept.invalid\n");
        assert!(ids(&broken).is_empty());
        assert_eq!(remove(&broken, "3"), broken);
        let nested =
            format!("{broken}# >>> the-cabinet 4\n127.0.0.1 x.invalid\n# <<< the-cabinet 3\n");
        assert!(ids(&nested).is_empty());
        assert_eq!(remove(&nested, "3"), nested);
    }

    #[test]
    fn keeps_windows_line_endings_outside_the_block() {
        let text = "127.0.0.1 localhost\r\n# mine\r\n";
        let added = add(text, "1", &names(&["a.invalid"]), Ipv4Addr::LOCALHOST);
        assert_eq!(remove(&added, "1"), text);
    }

    #[test]
    fn writes_its_block_with_the_line_endings_of_the_file() {
        let text = "127.0.0.1 localhost\r\n# mine";
        let added = add(text, "1", &names(&["a.invalid"]), Ipv4Addr::LOCALHOST);
        assert_eq!(
            added,
            "127.0.0.1 localhost\r\n# mine\r\n# >>> the-cabinet 1\r\n127.0.0.1 a.invalid\r\n# <<< the-cabinet 1\r\n"
        );
        assert_eq!(ids(&added), vec!["1"]);
        assert_eq!(outside(&added).len(), 1);
        assert_eq!(remove(&added, "1"), "127.0.0.1 localhost\r\n# mine\r\n");
        let empty = add("", "2", &names(&["a.invalid"]), Ipv4Addr::LOCALHOST);
        assert_eq!(empty.contains("\r\n"), cfg!(windows));
    }

    #[test]
    fn lists_mappings_outside_its_own_blocks() {
        let text = add(
            BASE,
            "1",
            &names(&["cabinet-test.invalid"]),
            Ipv4Addr::LOCALHOST,
        );
        let outside = outside(&format!(
            "{text}203.0.113.7 Two.Example three.example # note\n"
        ));
        assert!(outside.contains(&("api.example-game.net".to_string(), "127.0.0.1".to_string())));
        assert!(outside.contains(&("two.example".to_string(), "203.0.113.7".to_string())));
        assert!(outside.contains(&("three.example".to_string(), "203.0.113.7".to_string())));
        assert!(!outside
            .iter()
            .any(|(name, _)| name == "cabinet-test.invalid"));
        assert!(!outside.iter().any(|(name, _)| name == "note"));
    }
}
