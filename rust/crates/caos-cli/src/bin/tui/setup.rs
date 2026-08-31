//! First-run API-key setup for `caos tui`: when the `anthropic-api-key`
//! secret is missing, ask for the key — pasted, or a path to a file holding
//! one — before the alternate screen takes the terminal, write the canonical
//! `.caos-secrets` entry with fresh entropy included, make sure git ignores
//! the store, prove it loads through the same loader every turn uses, and
//! continue straight into the UI — no relaunch.
//!
//! A store that exists but fails to LOAD is not handled here on purpose: that
//! is an existing configuration broken, and a setup prompt would hide the
//! actual error (see [`caos_cli::model_secret_missing`]).

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use caos::{fresh_entropy, GitTransport, SECRETS_DIR};
use caos_cli::{
    ensure_conversation_secret, model_secret_manual_setup, model_secret_missing, MODEL_API_SECRET,
    MODEL_API_SECRET_READERS, MODEL_API_SECRET_VALUE_FILE,
};
use ratatui_crossterm::crossterm::terminal::size as terminal_size;

const PROMPT: &str = "> ";
/// Anthropic keys are ~100 characters; anything this short is a truncated
/// paste or a stray token, worth refusing while the person is still looking.
const MIN_KEY_CHARS: usize = 12;

/// Check the model credential before the TUI takes over the terminal — and
/// when none is configured, ask for one and install it instead of exiting
/// with instructions. The caller has already verified stdin/stdout are a
/// terminal and the server is reachable.
pub(crate) fn ensure_model_secret(transport: &GitTransport) -> Result<(), String> {
    if !model_secret_missing(transport)? {
        return Ok(());
    }
    let cols = terminal_size().map(|(cols, _)| cols).unwrap_or(80);
    let key = read_key(
        &mut io::stdin().lock(),
        &mut io::stdout(),
        cols,
        std::env::var_os("HOME").map(PathBuf::from),
    )?;
    // The store loader reads `.caos-secrets` relative to the working
    // directory, so that is where the entry is written.
    let root = std::env::current_dir()
        .map_err(|error| format!("reading the current directory: {error}"))?;
    for line in install_model_secret(&root, &key)? {
        println!("{line}");
    }
    // Prove the new entry through the loader every turn uses — value read,
    // readers resolved, credential present — while failures are still
    // readable at the shell prompt.
    ensure_conversation_secret(transport)?;
    println!("{MODEL_API_SECRET} configured; starting the tui");
    Ok(())
}

/// Prompt until one usable key arrives. An empty line or end of input aborts
/// with the by-hand instructions, so giving up here leaves the person no worse
/// off than the old failure did.
fn read_key(
    input: &mut impl BufRead,
    output: &mut impl Write,
    cols: u16,
    home: Option<PathBuf>,
) -> Result<String, String> {
    emit(
        output,
        &format!(
            "No Anthropic API key is configured, so conversations cannot run.\n\
             caos keeps the key in `{SECRETS_DIR}/` — a git-ignored, per-checkout store — as the\n\
             `{MODEL_API_SECRET}` secret granted only to the conversation workers (README, Secrets).\n\n\
             Paste an API key (sk-ant-…), or enter the path to a file that holds one.\n\
             Press Enter on an empty line to abort.\n"
        ),
    )?;
    loop {
        emit(output, PROMPT)?;
        let mut line = String::new();
        let read = input
            .read_line(&mut line)
            .map_err(|error| format!("reading the key entry: {error}"))?;
        let entry = line.trim();
        if read == 0 || entry.is_empty() {
            if read == 0 {
                emit(output, "\n")?;
            }
            return Err(format!(
                "no key entered; the tui needs an Anthropic API key to run \
                 conversations. To set one up by hand, {}",
                model_secret_manual_setup()
            ));
        }
        // The terminal echoed the entry while it was typed. If it is a key,
        // that echo is a secret on screen: overwrite it before anything else.
        if entry.starts_with("sk-") {
            erase_echoed_key(output, entry.chars().count(), cols)?;
        }
        match resolve_key(entry, home.as_deref()) {
            Ok(key) => return Ok(key),
            Err(reason) => emit(output, &format!("{reason}\n"))?,
        }
    }
}

/// One entry, resolved to the key it names: `sk-`-prefixed is the key itself,
/// anything else is read as the path to a file holding one (with `~` expanded
/// against `home`). A file's key is trimmed — the store's value is used
/// verbatim, straight into the `x-api-key` header, so a stray trailing
/// newline must never survive to it.
fn resolve_key(entry: &str, home: Option<&Path>) -> Result<String, String> {
    if entry.starts_with("sk-") {
        validate_key(entry)?;
        return Ok(entry.to_string());
    }
    let path = expand_home(entry, home);
    let text = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "{}: {error} — an API key starts with `sk-`; anything else is read \
             as the path to a key file",
            path.display()
        )
    })?;
    let key = text.trim();
    validate_key(key).map_err(|reason| format!("{}: {reason}", path.display()))?;
    Ok(key.to_string())
}

fn validate_key(key: &str) -> Result<(), String> {
    let chars = key.chars().count();
    if chars < MIN_KEY_CHARS {
        return Err(format!(
            "the key is {chars} character(s) — too short for an API key"
        ));
    }
    if let Some(bad) = key.chars().find(|c| !c.is_ascii_graphic()) {
        return Err(format!(
            "the key contains {bad:?}; expected one unbroken ASCII token"
        ));
    }
    Ok(())
}

fn expand_home(entry: &str, home: Option<&Path>) -> PathBuf {
    if let Some(home) = home {
        if entry == "~" {
            return home.to_path_buf();
        }
        if let Some(rest) = entry.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    PathBuf::from(entry)
}

/// Write the model credential under `root`: the empty store directory first
/// (git's directory-only ignore patterns like `.caos-secrets/` only match a
/// directory that exists), then the ignore rule — so no secret bytes ever land
/// in a directory git could track — then the value, then the spec file,
/// complete in one write with entropy included, so there is no window where a
/// loadable entry lacks its cache isolation. Returns the lines to show for
/// what was done.
fn install_model_secret(root: &Path, key: &str) -> Result<Vec<String>, String> {
    let dir = root.join(SECRETS_DIR);
    let spec_path = dir.join(MODEL_API_SECRET);
    if spec_path.exists() {
        // Reachable when the file parses but defines some other secret (e.g.
        // its `name=` differs). Never overwrite a person's configuration.
        return Err(format!(
            "{SECRETS_DIR}/{MODEL_API_SECRET} already exists but does not define a \
             `{MODEL_API_SECRET}` secret the store can use; fix or remove that file, then rerun"
        ));
    }
    create_secret_dir(&dir)?;
    let mut done = Vec::new();
    if let Some(line) = ensure_store_ignored(root)? {
        done.push(line);
    }
    // Exactly the key, no trailing newline: the value is used verbatim, and a
    // newline would ride into the `x-api-key` header.
    write_private(&dir.join(MODEL_API_SECRET_VALUE_FILE), key.as_bytes())
        .map_err(|error| format!("writing {SECRETS_DIR}/{MODEL_API_SECRET_VALUE_FILE}: {error}"))?;
    let spec = format!(
        "name={MODEL_API_SECRET}\nvalue:@={MODEL_API_SECRET_VALUE_FILE}\n\
         entropy={}\nreader={}\nreader={}\n",
        fresh_entropy()?,
        MODEL_API_SECRET_READERS[0],
        MODEL_API_SECRET_READERS[1],
    );
    write_private(&spec_path, spec.as_bytes())
        .map_err(|error| format!("writing {SECRETS_DIR}/{MODEL_API_SECRET}: {error}"))?;
    done.push(format!(
        "stored the key in {SECRETS_DIR}/{MODEL_API_SECRET} with fresh cache-isolation \
         entropy (what `caos secrets` adds)"
    ));
    Ok(done)
}

/// `.caos-secrets` must never become tracked: the tui's own `/update-tree`
/// runs `git add -A`, which would fold the store into a turn commit pushed to
/// the server. When git does not already ignore it, add the rule to the
/// repo-local `.git/info/exclude` — never the tracked `.gitignore`, since a
/// setup step must not dirty the working tree.
fn ensure_store_ignored(root: &Path) -> Result<Option<String>, String> {
    let status = Command::new("git")
        .args(["check-ignore", "-q", "--", SECRETS_DIR])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("running git check-ignore: {error}"))?;
    match status.code() {
        Some(0) => return Ok(None),
        Some(1) => {}
        _ => {
            return Err(format!(
                "git check-ignore {SECRETS_DIR} failed ({status}); is this a git working tree?"
            ))
        }
    }
    let exclude = Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("running git rev-parse --git-path: {error}"))?;
    if !exclude.status.success() {
        return Err(format!(
            "locating info/exclude: {}",
            String::from_utf8_lossy(&exclude.stderr).trim()
        ));
    }
    let named = String::from_utf8_lossy(&exclude.stdout).trim().to_string();
    if named.is_empty() {
        return Err("git named no info/exclude path".to_string());
    }
    // Relative to `root` in an ordinary checkout; already absolute in a
    // linked worktree — `join` passes an absolute path through.
    let path = root.join(&named);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("creating {}: {error}", parent.display()))?;
    }
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    std::fs::write(&path, format!("{existing}{sep}{SECRETS_DIR}/\n"))
        .map_err(|error| format!("writing {}: {error}", path.display()))?;
    Ok(Some(format!(
        "added `{SECRETS_DIR}/` to {named} so git never tracks the store"
    )))
}

fn create_secret_dir(dir: &Path) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|error| format!("creating {}: {error}", dir.display()))?;
    // Owner-only for a directory of secrets — but only when this run created
    // it, so an existing store keeps whatever its owner chose.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("restricting {}: {error}", dir.display()))?;
    }
    Ok(())
}

fn write_private(path: &Path, content: &[u8]) -> io::Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// After Enter the typed key sits on screen. The cursor is at column 0 one row
/// below the entry, so move up over every row the prompt + entry occupied,
/// clear to the end of the screen, and leave a placeholder in their place.
fn erase_echoed_key(output: &mut impl Write, typed: usize, cols: u16) -> Result<(), String> {
    let rows = erased_rows(typed, cols);
    emit(
        output,
        &format!("\u{1b}[{rows}A\r\u{1b}[0J{PROMPT}[api key hidden]\n"),
    )
}

/// Rows the prompt plus `typed` characters occupied on a `cols`-wide terminal.
/// Terminals wrap the cursor lazily, so an entry filling its last row exactly
/// still occupies only that row.
fn erased_rows(typed: usize, cols: u16) -> usize {
    (PROMPT.len() + typed).div_ceil(cols.max(1) as usize).max(1)
}

fn emit(output: &mut impl Write, text: &str) -> Result<(), String> {
    output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
        .map_err(|error| format!("writing the setup prompt: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "caos-tui-setup-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A throwaway git repo, so no test depends on cwd being one.
    fn throwaway_repo(name: &str) -> PathBuf {
        let dir = scratch_dir(name);
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        dir
    }

    fn check_ignored(repo: &Path) -> bool {
        Command::new("git")
            .args(["check-ignore", "-q", "--", SECRETS_DIR])
            .current_dir(repo)
            .status()
            .unwrap()
            .success()
    }

    fn read_key_from(entries: &str) -> (Result<String, String>, String) {
        let mut input = io::Cursor::new(entries.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = read_key(&mut input, &mut output, 80, None);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn resolves_pasted_keys_and_refuses_short_or_missing_ones() {
        assert_eq!(
            resolve_key("sk-ant-api03-abcdef", None).unwrap(),
            "sk-ant-api03-abcdef"
        );
        assert!(resolve_key("sk-abc", None)
            .unwrap_err()
            .contains("too short"));
        let missing = resolve_key("/no/such/caos-key-file", None).unwrap_err();
        assert!(missing.contains("/no/such/caos-key-file"));
        assert!(missing.contains("sk-"));
    }

    #[test]
    fn reads_keys_out_of_files_trimmed() {
        let dir = scratch_dir("key-files");
        // The usual `echo key > file` shape: the trailing newline is trimmed,
        // never stored (the value is used verbatim by every consumer).
        let trailing = dir.join("trailing");
        std::fs::write(&trailing, "sk-ant-file-key-123\n").unwrap();
        assert_eq!(
            resolve_key(trailing.to_str().unwrap(), None).unwrap(),
            "sk-ant-file-key-123"
        );

        let broken = dir.join("broken");
        std::fs::write(&broken, "sk-ant-file-key-123\nsecond line\n").unwrap();
        let error = resolve_key(broken.to_str().unwrap(), None).unwrap_err();
        assert!(error.contains("unbroken"), "{error}");

        let binary = dir.join("binary");
        std::fs::write(&binary, [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
        assert!(resolve_key(binary.to_str().unwrap(), None)
            .unwrap_err()
            .contains("UTF-8"));
    }

    #[test]
    fn tilde_expands_against_home() {
        let home = scratch_dir("home");
        std::fs::write(home.join("caos-key"), "sk-ant-home-key-123").unwrap();
        assert_eq!(
            resolve_key("~/caos-key", Some(&home)).unwrap(),
            "sk-ant-home-key-123"
        );
        // Without a home, `~` stays literal (and so fails as a path).
        assert!(resolve_key("~/caos-key", None).is_err());
    }

    #[test]
    fn installs_the_key_and_ignores_the_store() {
        let repo = throwaway_repo("install");
        assert!(!check_ignored(&repo));
        let done = install_model_secret(&repo, "sk-ant-pasted-key-123").unwrap();

        let value = repo.join(SECRETS_DIR).join(MODEL_API_SECRET_VALUE_FILE);
        assert_eq!(
            std::fs::read_to_string(&value).unwrap(),
            "sk-ant-pasted-key-123",
            "the value is verbatim: no trailing newline"
        );
        let spec = std::fs::read_to_string(repo.join(SECRETS_DIR).join(MODEL_API_SECRET)).unwrap();
        let lines: Vec<&str> = spec.lines().collect();
        assert_eq!(lines[0], format!("name={MODEL_API_SECRET}"));
        assert_eq!(lines[1], format!("value:@={MODEL_API_SECRET_VALUE_FILE}"));
        let entropy = lines[2].strip_prefix("entropy=").unwrap();
        assert_eq!(entropy.len(), 32);
        assert!(entropy.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(lines[3], format!("reader={}", MODEL_API_SECRET_READERS[0]));
        assert_eq!(lines[4], format!("reader={}", MODEL_API_SECRET_READERS[1]));

        assert!(check_ignored(&repo), "the store is git-ignored afterwards");
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(exclude.ends_with(&format!("{SECRETS_DIR}/\n")));
        assert!(done.iter().any(|line| line.contains("info/exclude")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&repo.join(SECRETS_DIR)), 0o700);
            assert_eq!(mode(&value), 0o600);
        }

        // A second run never overwrites the person's configuration.
        let error = install_model_secret(&repo, "sk-ant-other-key-456").unwrap_err();
        assert!(error.contains("already exists"), "{error}");
    }

    #[test]
    fn leaves_existing_ignore_rules_alone() {
        let repo = throwaway_repo("install-ignored");
        std::fs::write(repo.join(".gitignore"), format!("{SECRETS_DIR}/\n")).unwrap();

        let done = install_model_secret(&repo, "sk-ant-covered-key-123").unwrap();

        // `.gitignore` already covered the store: nothing to add.
        assert!(
            !repo.join(".git/info/exclude").exists() || {
                !std::fs::read_to_string(repo.join(".git/info/exclude"))
                    .unwrap()
                    .contains(SECRETS_DIR)
            }
        );
        assert!(!done.iter().any(|line| line.contains("info/exclude")));
    }

    #[test]
    fn prompts_until_a_usable_entry_and_hides_pasted_keys() {
        let (result, shown) = read_key_from("/no/such/file\nsk-ant-retried-key\n");
        assert_eq!(result.unwrap(), "sk-ant-retried-key");
        assert!(shown.contains("/no/such/file"), "{shown}");
        assert!(shown.contains("[api key hidden]"));
        assert!(shown.contains("\u{1b}[1A"), "erases the echoed entry");
        assert!(!shown.contains("sk-ant-retried-key"), "never re-prints it");
    }

    #[test]
    fn empty_entry_or_eof_aborts_with_the_by_hand_instructions() {
        let (result, _) = read_key_from("\n");
        let error = result.unwrap_err();
        assert!(error.contains("no key entered"));
        assert!(
            error.contains(&format!(".caos-secrets/{MODEL_API_SECRET}")),
            "{error}"
        );
        let (result, _) = read_key_from("");
        assert!(result.unwrap_err().contains("no key entered"));
    }

    #[test]
    fn erase_covers_every_row_the_entry_wrapped_onto() {
        assert_eq!(erased_rows(3, 80), 1);
        assert_eq!(erased_rows(78, 80), 1, "prompt + entry exactly one row");
        assert_eq!(erased_rows(79, 80), 2, "one character onto the next row");
        assert_eq!(erased_rows(100, 40), 3);
        assert_eq!(erased_rows(5, 0), 7, "a width of zero degrades safely");
    }
}
