//! Host operations on named workspaces. Settings and code move through the same lease.
use super::*;
use conversation_protocol::v3::WorkspaceConfig;

pub fn configure(
    t: &GitTransport,
    id: &str,
    name: &str,
    config: WorkspaceConfig,
) -> Result<String, String> {
    let refname = refs::head_ref(id)?;
    let mut preimage = None;
    append_transition(t, id, &refname, "configuring a workspace", |store, head| {
        let current = Conversation::open(store, head)?.workspace_config(name)?;
        if current == config {
            return Ok(Step::Done(head.to_string()));
        }
        if preimage.as_ref().is_some_and(|before| before != &current) {
            return Err(format!(
                "workspace {name:?} settings changed; reload before editing them"
            ));
        }
        preimage = Some(current);
        Ok(Step::Mint(Transition::WorkspaceConfigure {
            name: name.to_string(),
            config: config.clone(),
        }))
    })
}

pub fn create_from_workspace(
    t: &GitTransport,
    id: &str,
    name: &str,
    source: &str,
    stacked: bool,
) -> Result<String, String> {
    let store = open_store(t)?;
    let (refname, head) =
        fetch_validated_head(t, &store, id)?.ok_or_else(|| format!("no conversation {id:?}"))?;
    // Resolve once. A retry must not silently start from a newer source snapshot.
    let transitions = conversation_protocol::v3::workspaces::create_from_workspace(
        &Conversation::open(&store, &head)?,
        name,
        source,
        stacked,
    )?;
    append_transition(t, id, &refname, "creating a workspace", |store, head| {
        let view = Conversation::open(store, head)?;
        if view.workspace(name)?.is_some() {
            return Err(format!("workspace {name:?} already exists"));
        }
        Ok(Step::MintMany(transitions.clone()))
    })
}
