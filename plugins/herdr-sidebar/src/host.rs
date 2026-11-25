//! Who runs the sidebar, and how it runs itself again (the preview viewer,
//! the custom editor, file transfer servers): the `herdr-sidebar` program,
//! or a host such as herdr-native that runs it as one of its own commands
//! (`herdr-native sidebar …`).
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    /// A plugin pane per tab, opened and kept by herdr's plugin hooks.
    Plugin,
    /// One sidebar for the whole session, docked by the host beside every
    /// tab; it follows the workspace being viewed.
    Dock,
}

struct Invocation {
    program: PathBuf,
    prefix: Vec<String>,
}

static INVOCATION: OnceLock<Invocation> = OnceLock::new();
static HOST: OnceLock<Host> = OnceLock::new();

/// Run by a host: `program prefix… <sidebar args>` runs the sidebar again.
pub fn set_invocation(program: PathBuf, prefix: Vec<String>) {
    let _ = INVOCATION.set(Invocation { program, prefix });
}

pub fn set_host(host: Host) {
    let _ = HOST.set(host);
}

pub fn host() -> Host {
    HOST.get().copied().unwrap_or(Host::Plugin)
}

pub fn is_dock() -> bool {
    host() == Host::Dock
}

/// This program again with `args`, for spawning directly.
pub fn command(args: &[&str]) -> std::io::Result<Command> {
    let mut command = match INVOCATION.get() {
        Some(invocation) => {
            let mut command = Command::new(&invocation.program);
            command.args(&invocation.prefix);
            command
        }
        None => Command::new(std::env::current_exe()?),
    };
    command.args(args);
    Ok(command)
}

/// This program again with `args`, as a line typed into a shell.
pub fn shell_line(args: &[&str]) -> String {
    let mut words = match INVOCATION.get() {
        Some(invocation) => {
            let mut words = vec![quote(&invocation.program.to_string_lossy())];
            words.extend(invocation.prefix.iter().map(|word| quote(word)));
            words
        }
        None => vec![crate::state::EXECUTABLE_NAME.to_owned()],
    };
    words.extend(args.iter().map(|word| quote(word)));
    words.join(" ")
}

fn quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        return word.to_owned();
    }
    if cfg!(windows) {
        format!("\"{}\"", word.replace('"', "\"\""))
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_are_quoted_only_when_needed() {
        assert_eq!(quote("--preview"), "--preview");
        assert_eq!(
            quote("/home/a b/herdr"),
            if cfg!(windows) {
                "\"/home/a b/herdr\""
            } else {
                "'/home/a b/herdr'"
            }
        );
        if !cfg!(windows) {
            assert_eq!(quote("it's"), "'it'\\''s'");
        }
    }
}
