//! Push one pinned code commit directly from the server object store.
use crate::{Config, HttpError};
use conversation_protocol::v3::{oid::G3, PublicationStatus};
use serde::{Deserialize, Deserializer};
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    destination: String,
    commit: String,
    branch: String,
    // No default: omission is different from an explicit create-only lease.
    #[serde(deserialize_with = "expected")]
    expected: Option<String>,
}

fn expected<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Option::deserialize(d)
}

pub(crate) fn endpoint(
    config: &Config,
    request: &mut tiny_http::Request,
) -> Result<Vec<u8>, HttpError> {
    let (mut input, token): (Input, _) = crate::remote_git::input(request)?;
    if !git_locator::import::commit(&input.commit)
        || input
            .expected
            .as_ref()
            .is_some_and(|h| !git_locator::import::commit(h))
    {
        return Err(HttpError::new(400, "push requires full commit hashes"));
    }
    input.commit.make_ascii_lowercase();
    if let Some(old) = &mut input.expected {
        old.make_ascii_lowercase();
    }
    let token = token.as_deref();
    git_locator::import::git(&input.destination, token).map_err(|e| HttpError::new(400, e))?;
    let run = |args: &[&str]| -> Result<std::process::Output, String> {
        git_locator::import::git(&input.destination, token)?
            .args(["--git-dir", &config.git_dir])
            .args(args)
            .output()
            .map_err(|_| "could not start Git publication".into())
    };
    let capture = |args: &[&str]| -> Result<String, String> {
        let output = run(args)?;
        if !output.status.success() {
            return Err("Git publication validation failed".into());
        }
        String::from_utf8(output.stdout).map_err(|_| "invalid Git response".into())
    };
    let refname = format!("refs/heads/{}", input.branch);
    if input.branch.starts_with('-')
        || !run(&["check-ref-format", &refname])
            .map_err(|e| HttpError::new(502, e))?
            .status
            .success()
    {
        return Err(HttpError::new(400, "invalid publication branch"));
    }
    let invalid = |e| HttpError::new(422, e);
    if capture(&["cat-file", "-t", &input.commit])
        .map_err(invalid)?
        .trim()
        != "commit"
    {
        return Err(HttpError::new(422, "publication requires a code commit"));
    }
    // Ingestion and startup certify full closure. Only check whether this is
    // conversation ancestry; an absent G3 cannot be an ancestor of stored H.
    let genesis = gix::ObjectId::from_hex(G3.as_bytes()).expect("valid G3");
    if config
        .repo
        .to_thread_local()
        .try_find_object(genesis)
        .map_err(|_| HttpError::new(422, "could not check source ancestry"))?
        .is_some()
    {
        match run(&["merge-base", "--is-ancestor", G3, &input.commit])
            .map_err(invalid)?
            .status
            .code()
        {
            Some(0) => {
                return Err(HttpError::new(
                    422,
                    "conversation commits cannot be published as source",
                ))
            }
            Some(1) => {}
            _ => return Err(HttpError::new(422, "could not check source ancestry")),
        }
    }
    git_locator::publish::reject_caos(&input.commit, capture).map_err(invalid)?;
    git_locator::publish::reject_markers(&input.commit, run).map_err(invalid)?;

    let observe = || git_locator::publish::read_branch(&input.destination, &input.branch, token);
    let receipt = |status: PublicationStatus, observed: Option<String>, kind: &str| {
        Ok(serde_json::to_vec(&json!({
            "commit": input.commit, "branch": input.branch, "status": status,
            "observed": observed, "kind": kind
        }))
        .unwrap())
    };
    let observed = observe().map_err(|e| HttpError::new(502, e))?;
    if observed.as_deref() == Some(&input.commit) {
        return receipt(PublicationStatus::Complete, observed, "ref-converged");
    }
    if observed != input.expected {
        return receipt(PublicationStatus::Conflict, observed, "lease-rejected");
    }
    if let Some(old) = &input.expected {
        if !run(&["merge-base", "--is-ancestor", old, &input.commit])
            .map_err(invalid)?
            .status
            .success()
        {
            return Err(HttpError::new(
                422,
                "publication would not fast-forward; import and merge the remote head first",
            ));
        }
    }
    let lease = format!(
        "--force-with-lease={refname}:{}",
        input.expected.as_deref().unwrap_or("")
    );
    let refspec = format!("{}:{refname}", input.commit);
    let output = run(&[
        "-c",
        "push.followTags=false",
        "push",
        "--no-verify",
        "--recurse-submodules=no",
        &lease,
        "--",
        &input.destination,
        &refspec,
    ]);
    if output.is_ok_and(|o| o.status.success()) {
        return receipt(
            PublicationStatus::Complete,
            Some(input.commit.clone()),
            "push-success",
        );
    }
    // A failed transport can still have updated the branch. Never refresh the
    // caller's lease or interpret an unchanged branch as proof of failure.
    match observe() {
        Ok(observed) if observed.as_deref() == Some(&input.commit) => {
            receipt(PublicationStatus::Complete, observed, "ref-converged")
        }
        Ok(observed) if observed != input.expected => {
            receipt(PublicationStatus::Conflict, observed, "ref-drift")
        }
        observed => receipt(
            PublicationStatus::Uncertain,
            observed.ok().flatten(),
            "ambiguous",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expected_is_required_but_may_be_null() {
        let mut value =
            json!({"destination":"https://host/repo","commit":"a".repeat(40),"branch":"topic"});
        assert!(serde_json::from_value::<Input>(value.clone()).is_err());
        value["expected"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Input>(value.clone())
            .unwrap()
            .expected
            .is_none());
        value["expected"] = json!("b".repeat(40));
        assert!(serde_json::from_value::<Input>(value)
            .unwrap()
            .expected
            .is_some());
    }
}
