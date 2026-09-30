//! The secret-store formats the client and server share (SPEC.md, "Secrets"):
//! a secret file, the tree a push sends, and the message a push signs.

/// Names one or more SecretReaderKeys, space-separated, on a request. Out of
/// band because a key is a credential and an ArgTree is readable by anyone.
pub const READERS_HEADER: &str = "X-Caos-Secret-Readers";
/// The conversation a request is for, which `reader:@=` grants are scoped to.
pub const CONVERSATION_HEADER: &str = "X-Caos-Conversation";

/// The SecretReaderKey a push replaces the tree of.
pub const PUSH_KEY_HEADER: &str = "X-Caos-Secret-Reader-Key";
/// The push's sequence number, which must exceed the last one accepted.
pub const PUSH_SEQUENCE_HEADER: &str = "X-Caos-Secret-Sequence";
/// The SecretWriterKey's signature over [`push_message`], as hex.
pub const PUSH_SIGNATURE_HEADER: &str = "X-Caos-Secret-Signature";

/// Where a pushed tree keeps each secret: `<name>/spec` and `<name>/value`.
pub const PUSHED_SPEC: &str = "spec";
pub const PUSHED_VALUE: &str = "value";

/// The bytes a push signs.
pub fn push_message(reader_key: &str, tree: &str, sequence: u64) -> Vec<u8> {
    format!("caos-secrets-push\n{reader_key}\n{tree}\n{sequence}\n").into_bytes()
}

/// Who may read a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reader {
    /// `reader:@@=<locator> [since=<sha>]`: the image at the locator's `dir=`,
    /// in its `rev` and first-parent ancestors back to `since`.
    Locator {
        locator: String,
        since: Option<String>,
    },
    /// `reader:@=<path> conversation=<id>`: whatever is at `path` in one
    /// conversation.
    Conversation { path: String, conversation: String },
}

impl Reader {
    fn render(&self) -> String {
        match self {
            Reader::Locator { locator, since } => match since {
                Some(since) => format!("reader:@@={locator} since={since}"),
                None => format!("reader:@@={locator}"),
            },
            Reader::Conversation { path, conversation } => {
                format!("reader:@={path} conversation={conversation}")
            }
        }
    }
}

/// Where a secret file takes its value from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Inline(String),
    /// Relative to the secret file.
    File(String),
}

/// One secret file, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub entropy: Option<String>,
    pub value: Option<Value>,
    pub readers: Vec<Reader>,
}

impl Spec {
    /// The `spec` blob a push stores: everything but the value, which travels
    /// as its own blob so it may hold any bytes.
    pub fn render_pushed(&self) -> String {
        let mut out = String::new();
        if let Some(entropy) = &self.entropy {
            out.push_str(&format!("entropy={entropy}\n"));
        }
        for reader in &self.readers {
            out.push_str(&reader.render());
            out.push('\n');
        }
        out
    }
}

/// Parse a secret file. `file_name` is the default name and labels errors.
pub fn parse_spec(file_name: &str, text: &str) -> Result<Spec, String> {
    let mut spec = Spec {
        name: file_name.to_string(),
        entropy: None,
        value: None,
        readers: Vec::new(),
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, val) = line
            .split_once('=')
            .ok_or_else(|| format!("secret {file_name}: line {line:?} is not key=value"))?;
        match key {
            "name" => spec.name = val.trim().to_string(),
            "entropy" => spec.entropy = Some(val.trim().to_string()),
            "value" => spec.value = Some(Value::Inline(val.to_string())),
            "value:@" => spec.value = Some(Value::File(val.trim().to_string())),
            "reader:@@" => spec.readers.push(parse_locator_reader(file_name, val)?),
            "reader:@" => spec
                .readers
                .push(parse_conversation_reader(file_name, val)?),
            other => return Err(format!("secret {file_name}: unknown key {other:?}")),
        }
    }
    if spec.name.is_empty() || spec.name.contains('/') || spec.name.starts_with('.') {
        return Err(format!("secret {file_name}: bad name {:?}", spec.name));
    }
    Ok(spec)
}

fn parse_locator_reader(file_name: &str, val: &str) -> Result<Reader, String> {
    let mut words = val.split_whitespace();
    let locator = words
        .next()
        .ok_or_else(|| format!("secret {file_name}: reader:@@= names no locator"))?
        .to_string();
    let mut since = None;
    for word in words {
        match word.split_once('=') {
            Some(("since", sha)) if is_full_sha(sha) => since = Some(sha.to_string()),
            Some(("since", sha)) => {
                return Err(format!(
                    "secret {file_name}: since={sha:?} is not a 40-hex commit"
                ))
            }
            _ => return Err(format!("secret {file_name}: unknown reader field {word:?}")),
        }
    }
    Ok(Reader::Locator { locator, since })
}

fn parse_conversation_reader(file_name: &str, val: &str) -> Result<Reader, String> {
    let mut words = val.split_whitespace();
    let path = words
        .next()
        .ok_or_else(|| format!("secret {file_name}: reader:@= names no path"))?
        .to_string();
    let mut conversation = None;
    for word in words {
        match word.split_once('=') {
            Some(("conversation", id)) if !id.is_empty() => conversation = Some(id.to_string()),
            _ => return Err(format!("secret {file_name}: unknown reader field {word:?}")),
        }
    }
    let conversation = conversation
        .ok_or_else(|| format!("secret {file_name}: reader:@={path} needs conversation=<id>"))?;
    Ok(Reader::Conversation { path, conversation })
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A SecretReaderKey: an ed25519 public key as 64 lowercase hex.
pub fn is_reader_key(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn parses_both_reader_forms() {
        let text = format!(
            "# a comment\nname=gh\nentropy=E\nvalue=tok\n\
             reader:@@=git+https://x/y?rev={SHA}&dir=std/llm-step since={SHA}\n\
             reader:@@=git+https://x/y?rev={SHA}\n\
             reader:@=imports/y/tool conversation=c1\n"
        );
        let spec = parse_spec("file", &text).unwrap();
        assert_eq!(spec.name, "gh");
        assert_eq!(spec.value, Some(Value::Inline("tok".into())));
        assert_eq!(
            spec.readers,
            vec![
                Reader::Locator {
                    locator: format!("git+https://x/y?rev={SHA}&dir=std/llm-step"),
                    since: Some(SHA.into()),
                },
                Reader::Locator {
                    locator: format!("git+https://x/y?rev={SHA}"),
                    since: None,
                },
                Reader::Conversation {
                    path: "imports/y/tool".into(),
                    conversation: "c1".into(),
                },
            ]
        );
        // The pushed spec round-trips, without the value.
        let pushed = parse_spec("gh", &spec.render_pushed()).unwrap();
        assert_eq!(pushed.readers, spec.readers);
        assert_eq!(pushed.entropy, spec.entropy);
        assert_eq!(pushed.value, None);
    }

    #[test]
    fn refuses_what_it_cannot_honour() {
        assert!(parse_spec("f", "reader:@=a/b\n").is_err());
        assert!(parse_spec("f", "reader:@@=git+https://x/y since=main\n").is_err());
        assert!(parse_spec("f", "reader:@@=git+https://x/y until=x\n").is_err());
        assert!(parse_spec("f", "reader=std/x\n").is_err());
        assert!(parse_spec("f", "value:env=X\n").is_err());
        assert!(parse_spec("f", "name=../x\n").is_err());
    }
}
