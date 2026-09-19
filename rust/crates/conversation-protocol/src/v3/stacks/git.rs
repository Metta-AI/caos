use super::{ConflictMessage, ConflictStage, MergeResult};
use crate::v3::Oid;

/// Parse the NUL-delimited form; filenames can contain tabs and newlines.
/// The status, not the presence of stage rows, determines whether it conflicted.
pub(crate) fn parse_merge(bytes: &[u8], conflicted: bool) -> Result<MergeResult, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "merge paths must be UTF-8")?;
    let mut fields = text.split_terminator('\0');
    let tree = Oid::parse(
        fields.next().ok_or("merge returned no tree")?,
        "merged tree",
    )?;
    let mut stages = Vec::new();
    let mut messages = Vec::new();
    if fields.clone().next().is_some() {
        loop {
            let line = fields.next().ok_or("missing conflict separator")?;
            if line.is_empty() {
                break;
            }
            let (info, path) = line.split_once('\t').ok_or("invalid conflict stage")?;
            let mut info = info.split(' ');
            let mode = info.next().ok_or("missing conflict mode")?.to_owned();
            crate::v3::Mode::parse(&mode)?;
            let oid = Oid::parse(
                info.next().ok_or("missing conflict object")?,
                "conflict object",
            )?;
            let stage = info
                .next()
                .ok_or("missing conflict stage")?
                .parse::<u8>()
                .map_err(|_| "invalid conflict stage")?;
            if info.next().is_some() || !(1..=3).contains(&stage) {
                return Err("invalid conflict stage".into());
            }
            stages.push(ConflictStage {
                path: path.into(),
                mode,
                oid,
                stage,
            });
        }
    }
    while let Some(count) = fields.next() {
        let count = count
            .parse::<usize>()
            .map_err(|_| "invalid merge message path count")?;
        let paths = (0..count)
            .map(|_| {
                fields
                    .next()
                    .map(str::to_owned)
                    .ok_or("missing merge message path")
            })
            .collect::<Result<Vec<_>, _>>()?;
        let kind = fields.next().ok_or("missing conflict type")?.to_owned();
        let message = fields.next().ok_or("missing conflict message")?.to_owned();
        messages.push(ConflictMessage {
            paths,
            kind,
            message,
        });
    }
    Ok(MergeResult {
        tree,
        conflicted,
        stages,
        messages,
    })
}
