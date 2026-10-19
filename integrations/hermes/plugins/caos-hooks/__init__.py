"""Hermes-side equivalent of caos's Claude Code hooks (see rust/crates/caos-cli/MCP.md).

Three events, three jobs, matching serve.rs/mod.rs exactly:

1. pre_tool_call -- the PreToolUse job. serve.rs declares caos_session,
   caos_tool_use_id, caos_prompt_id as optional string properties on every
   tool's schema ("Supplied automatically by the caos PreToolUse hook; do
   not set it.") and *requires* caos_session to attribute a call to a
   conversation (serve.rs:968, "the PreToolUse hook that supplies it is
   not installed"). This hook merges them into the call's args directly in
   Python -- no subprocess, no git/server contact, exactly what
   on_pre_tool_use() in mod.rs does for Claude Code, just without needing to
   shell out to `caos mcp hook` for this one event.

2. pre_llm_call -- the UserPromptSubmit job. Only the Rust binary can create
   or extend a caos conversation (git transitions live in the client, not
   here), so this shells out to `caos mcp hook` with a payload shaped like
   Claude Code's own UserPromptSubmit hook event. Fires once per turn,
   before the tool loop -- the same ordering guarantee
   rust/crates/caos-cli/src/mcp/mod.rs's wait_for_conversation() tolerates a
   near-miss on (serve.rs: "the first tool call can beat it").

3. post_llm_call -- the Stop job. Same subprocess shape, firing once per
   turn after a *successful* turn.

Deliberately NOT wired for v1: StopFailure. post_llm_call does not fire on
an interrupted/errored turn, and Hermes has no exact equivalent hook in the
current catalog. This is a known gap, not a silent one -- but it is also
not a correctness gap: rust/crates/caos-cli/src/mcp/resume.rs's `settle()`
already exists specifically to close a request a dead session left open
(cancelled calls, interrupted request) the next time anything forks/resumes
that conversation. A Hermes crash mid-turn behaves exactly like a killed
Claude Code process did before StopFailure existed -- a slightly messier
log, not a stuck or corrupted one.

CAOS_BIN / the --llm-step locator: resolved by bin/caos-mcp-launch for the
MCP server itself; this plugin shells out to the SAME binary with `mcp hook`
instead of `mcp serve`, so it needs to find it the same way. See the
_caos_bin() TODO below -- this currently assumes the MCP server has already
been launched at least once in this session (so CAOS_BIN or
result/bin/caos exists); it does not independently trigger a nix build.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any, Optional

_HERE = Path(__file__).resolve().parent
# AT RUNTIME this is the INSTALLED profile's own root
# (~/.hermes/profiles/<name>/, or /state/hermes/profiles/<name>/ in the
# bundled stack) -- `hermes profile install` copies a plugin's whole
# directory under profiles/<name>/plugins/<name>/, two levels below. Only in
# an un-installed, hand-run-from-source Desktop profile does this instead
# land on the source tree's integrations/hermes/.
_DIST_ROOT = _HERE.parent.parent
_REPO_ROOT = _DIST_ROOT.parent.parent  # the caos checkout, when this IS one


def _read_profile_env() -> dict[str, str]:
    """The vars `scripts/caos-mcp-launch` gets (CAOS_BIN, CAOS_LLM_STEP_REV,
    CAOS_MCP_DEFAULT_SERVER, ...), read directly from the profile's `.env`
    file rather than from this process's OWN environment.

    THIS PROCESS NEVER HAS THEM. Hermes passes them to the MCP SERVER
    subprocess via config.yaml's `${VAR}` substitution at spawn time, but
    this plugin's hooks run IN-PROCESS inside `hermes serve` itself, which
    never gets them -- confirmed directly: `hermes serve`'s own
    `/proc/<pid>/environ` carries none of CAOS_BIN, CAOS_LLM_STEP_REV,
    CAOS_MCP_DEFAULT_SERVER, CAOS_REPO, CAOS_HERMES_DIST_DIR. Before this,
    `_caos_bin()` always returned None here (the `_REPO_ROOT/result/bin/caos`
    fallback never existing in a bundled deploy either), so `_run_hook` was a
    silent no-op for EVERY UserPromptSubmit/Stop call -- no conversation was
    ever created, which is why `caos_status` reported having none.
    `_DIST_ROOT` is the installed profile's own directory (see above), the
    same place `.env` lives, so read it from there instead.
    """
    path = _DIST_ROOT / ".env"
    values: dict[str, str] = {}
    try:
        for line in path.read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, _, value = line.partition("=")
            values[key.strip()] = value.strip()
    except OSError:
        pass
    return values


def _env(name: str, profile_env: dict[str, str]) -> Optional[str]:
    """A var from the profile's `.env` first, then this process's own
    environment -- the latter covers a hand-run, never-`install`ed Desktop
    profile, which has no `.env` to read and gets these from its shell
    instead."""
    return profile_env.get(name) or os.environ.get(name)


def _caos_bin(profile_env: dict[str, str]) -> Optional[str]:
    """Same resolution order as bin/caos-mcp-launch, read-only (no nix build
    triggered here -- if the MCP server hasn't been started yet in this
    profile, there is nothing to hook into regardless)."""
    env_bin = _env("CAOS_BIN", profile_env)
    if env_bin and not _unsubstituted(env_bin) and os.access(env_bin, os.X_OK):
        return env_bin
    candidate = _REPO_ROOT / "result" / "bin" / "caos"
    if candidate.is_file() and os.access(candidate, os.X_OK):
        return str(candidate)
    return None


def _unsubstituted(value: Optional[str]) -> bool:
    """Hermes's config.yaml ${VAR} substitution leaves an optional var that's
    absent from the installed profile's .env as the literal, unexpanded text
    "${VAR_NAME}" rather than an empty string -- confirmed directly via
    scripts/caos-mcp-launch hitting this for CAOS_LLM_STEP_REV. Catch the
    same shape here.
    (Reading `.env` directly, as `_read_profile_env` now does, mostly avoids
    this -- an UNSET var is simply absent from the dict -- but a line that
    HAS been written with the literal substitution placeholder as its value
    could still carry this shape, so keep the guard.)"""
    return value is not None and value.startswith("${") and value.endswith("}")


def _dev_sha(profile_env: dict[str, str]) -> Optional[str]:
    """Same refs/caos/dev probe `scripts/caos-mcp-launch` does, and for the
    same reason -- see that script's comment. Kept in sync by hand; if one
    changes, change the other."""
    server = _env("CAOS_MCP_DEFAULT_SERVER", profile_env)
    if not server or _unsubstituted(server):
        return None
    try:
        result = subprocess.run(
            ["git", "ls-remote", server, "refs/caos/dev"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except Exception:
        return None
    if result.returncode != 0:
        return None
    out = result.stdout.strip()
    return out.split()[0] if out else None


def _session_sha() -> Optional[str]:
    """Metta-AI/caos-session's current main tip -- the DEFAULT --llm-step
    source once no dev ref is available (see _llm_step_arg). Same bounded
    `git ls-remote` pattern as `_dev_sha`, just against a fixed repo instead
    of a configured server."""
    try:
        result = subprocess.run(
            ["git", "ls-remote", "https://github.com/Metta-AI/caos-session", "main"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
    except Exception:
        return None
    if result.returncode != 0:
        return None
    out = result.stdout.strip()
    return out.split()[0] if out else None


def _llm_step_arg(profile_env: dict[str, str]) -> str:
    """Same rewrite scripts/caos-mcp-launch applies, and for the same reason:
    `--llm-step:@=std/llm-step` is a path relative to the resolving
    process's OWN cwd, which this subprocess call's caller (the Hermes
    agent process) never puts inside a caos checkout.

    PREFERS the server's own refs/caos/dev (`_dev_sha`), same as
    caos-mcp-launch and for the identical reason: it is the one locator that
    matches what THIS stack's seeded core actually answers for, uncommitted
    changes included, with no second version to keep in sync.

    DEFAULT, otherwise: Metta-AI/caos-session's current main tip -- the
    SAME tree a real Claude Code session reads its own pin from (its
    `caos-std/` mount, per that repo's root `.caos-expr` +
    `std/flake-input-loader`), via `:@@=`'s own evaluation of
    caos-session's `.caos-expr` chain -- no local checkout needed, since
    `:@@=` is "exactly right for an image locator" like this one
    (session_base.rs's own words; it was only wrong for THAT module's
    different purpose, resolving a conversation's raw content commit, not
    an image). No CAOS_LLM_STEP_REV/CAOS_REPO fallback anymore -- those
    were the only options for a deploy with no refs/caos/dev to ask, and a
    bundled gateway deploy never has either set, so this always fell
    through to the bare local form below and failed to resolve: the exact
    bug this default replaces.
    """
    dev_sha = _dev_sha(profile_env)
    if dev_sha:
        return f"--llm-step:@@=git+caos://local?rev={dev_sha}&dir=std/llm-step"
    session_sha = _session_sha()
    if session_sha:
        return (
            "--llm-step:@@=github:Metta-AI/caos-session"
            f"?rev={session_sha}&dir=caos-std/llm-step"
        )
    return "--llm-step:@=std/llm-step"


def _run_hook(payload: dict[str, Any]) -> Optional[dict[str, Any]]:
    """POST one hook payload to `caos mcp hook`, Claude-Code-shaped. Returns the
    parsed stdout JSON (a systemMessage or hookSpecificOutput envelope) on
    success, or None on any failure -- hooks are fail-open by design here;
    losing the durable-log recording for one turn is recoverable (see
    module docstring), blocking the user's turn on it is not."""
    profile_env = _read_profile_env()
    bin_path = _caos_bin(profile_env)
    if bin_path is None:
        return None
    # CAOS_MCP_DEFAULT_SERVER, EXPLICITLY, not just inherited: this process
    # (`hermes serve`) never has it in its OWN environment (see
    # `_read_profile_env`'s docstring), and `caos mcp hook` needs it to find
    # a workspace at all when there is no real git checkout at its cwd or
    # $CLAUDE_PROJECT_DIR -- `session_base::default_server_scratch()` reads
    # exactly this var and returns `None` (not an error; just "nothing to
    # fall back to") when it is unset. Confirmed the hard way: with every
    # other part of this fix in place, the subprocess still failed with
    # "caos-cli must run inside a git working tree (none found)" until this
    # one variable was added to its environment -- a conversation was still
    # never created.
    server = _env("CAOS_MCP_DEFAULT_SERVER", profile_env)
    env = dict(os.environ)
    if server and not _unsubstituted(server):
        env["CAOS_MCP_DEFAULT_SERVER"] = server
    try:
        result = subprocess.run(
            [bin_path, "mcp", "hook", _llm_step_arg(profile_env)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
            env=env,
        )
    except Exception:
        return None
    if result.returncode not in (0, 2):
        # Non-2 non-zero is a real crash; 2 is "blocked, reason on stderr" for
        # UserPromptSubmit/PreToolUse specifically (mod.rs's hook()) -- neither
        # should take the user's turn down over caos bookkeeping.
        return None
    out = (result.stdout or "").strip()
    if not out:
        return None
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return None


def on_pre_tool_call(
    tool_name: str = "",
    args: Optional[dict[str, Any]] = None,
    session_id: str = "",
    tool_call_id: str = "",
    turn_id: str = "",
    **_kwargs: Any,
) -> Optional[dict[str, Any]]:
    # Hermes's MCP tool naming (tools/mcp_tool_schema.py's mcp_prefixed_tool_name):
    # mcp__{server_name}__{tool_name}, double underscores, same convention
    # Claude Code uses. "caos" is this distribution's server name
    # (config.yaml's mcp_servers.caos).
    if not tool_name.startswith("mcp__caos__"):
        return None
    injected: dict[str, Any] = {"caos_session": session_id}
    if tool_call_id:
        injected["caos_tool_use_id"] = tool_call_id
    if turn_id:
        injected["caos_prompt_id"] = turn_id
    return {"action": "modify", "args": injected}


def on_pre_llm_call(
    session_id: str = "",
    user_message: str = "",
    is_first_turn: bool = False,
    **_kwargs: Any,
) -> None:
    if not user_message.strip():
        return None
    _run_hook(
        {
            "hook_event_name": "UserPromptSubmit",
            "session_id": session_id,
            "prompt": user_message,
        }
    )
    return None  # observer only; no context injection


def on_post_llm_call(
    session_id: str = "",
    assistant_response: str = "",
    **_kwargs: Any,
) -> None:
    _run_hook(
        {
            "hook_event_name": "Stop",
            "session_id": session_id,
            "last_assistant_message": assistant_response,
        }
    )
    return None


def register(ctx) -> None:
    ctx.register_hook("pre_tool_call", on_pre_tool_call)
    ctx.register_hook("pre_llm_call", on_pre_llm_call)
    ctx.register_hook("post_llm_call", on_post_llm_call)
