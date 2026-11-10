//! Who may call the agent: `LINEAR_TRUSTED_USERS` and `ZULIP_TRUSTED_EMAILS` refuse everyone
//! else, and leave everyone heard when unset.

use crate::{
    harness::{Ctx, Harness, Setup, ZULIP_TOKEN, eventually},
    scenarios::{Scenario, body, created, issue, mentioned, prompted},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

macro_rules! scenarios {
    ($($name:ident),* $(,)?) => {
        &[$((stringify!($name), (|ctx| Box::pin($name(ctx))) as Scenario)),*]
    };
}

pub const ALL: &[(&str, Scenario)] = scenarios![
    linear_untrusted_callers_start_nothing,
    zulip_untrusted_sender_is_refused,
    without_trusted_lists_anyone_is_heard,
];

const REFUSED: &str = "This agent only works for its owner.";
const IGNORED: &str = "This agent only works for its owner, so it ignored that message.";

/// A Linear user as webhooks carry one.
fn user(id: &str, email: &str) -> Value {
    json!({ "id": id, "email": email, "name": id })
}

/// `event`, a `created` one, with `creator` as the session's creator.
fn by(mut event: Value, creator: &Value) -> Value {
    event["agentSession"]["creatorId"] = creator["id"].clone();
    event["agentSession"]["creator"] = creator.clone();
    event
}

/// `event`, a `prompted` one, written by `author`.
fn from(mut event: Value, author: &Value) -> Value {
    event["agentActivity"]["userId"] = author["id"].clone();
    event["agentActivity"]["user"] = author.clone();
    event
}

/// A Zulip channel mention sent by `email`.
fn zulip_mention(id: u64, sender_id: u64, name: &str, email: &str, text: &str) -> Value {
    json!({
        "token": ZULIP_TOKEN, "trigger": "mention", "data": format!("@**Bot** {text}"),
        "message": {
            "id": id, "type": "stream", "stream_id": 7, "subject": "deploy",
            "display_recipient": "ops", "sender_id": sender_id, "sender_full_name": name,
            "sender_email": email, "content": format!("@**Bot** {text}"),
        },
    })
}

/// Waits for the `n`th (from 1) message the bot posted to Zulip and returns its text.
async fn zulip_reply(h: &Harness, n: usize) -> Result<String> {
    eventually(&format!("Zulip reply #{n}"), || {
        h.mock
            .read(|r| r.zulip_messages.get(n - 1).cloned())
            .and_then(|m| m.get("content").cloned())
    })
    .await
}

/// Delegation, mention and a reply by someone the list does not name, and a session Linear
/// names no creator for, each get a short answer and no agent; the owner, named by id or by
/// email in any letter case, still gets one.
async fn linear_untrusted_callers_start_nothing(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        env: vec![(
            "LINEAR_TRUSTED_USERS",
            "user-alice, Owner@Example.com".into(),
        )],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let eve = user("user-eve", "eve@example.com");
    for n in 41..=44 {
        h.mock.add_issue(&issue(n, "Do it."));
    }

    let delegated = by(created("t1", &issue(41, ""), "context of SH-41"), &eve);
    let mention = mentioned(
        "t2",
        &issue(42, ""),
        "@mothership do it",
        "context of SH-42",
    );
    let automation = created("t3", &issue(43, ""), "context of SH-43");
    for event in [delegated, by(mention, &eve), automation] {
        ensure!(h.linear(event).await? == 200);
    }
    for sid in ["t1", "t2", "t3"] {
        let response = h.activity(sid, "response", 1).await?;
        ensure!(body(&response) == REFUSED, "{sid}: {response}");
    }
    h.logged("[t1] refused a session from Linear user user-eve")
        .await?;
    h.logged("[t2] refused a session from Linear user user-eve")
        .await?;
    h.logged("[t3] refused a session from Linear user none")
        .await?;

    let owner = user("user-other-id", "owner@example.com");
    let ours = issue(44, "");
    let event = by(created("t4", &ours, "context of SH-44"), &owner);
    ensure!(h.linear(event).await? == 200);
    let response = h.activity("t4", "response", 1).await?;
    ensure!(body(&response) == "Done: context of SH-44", "{response}");

    ensure!(
        h.linear(from(prompted("t4", &ours, "eve was here"), &eve))
            .await?
            == 200
    );
    let response = h.activity("t4", "response", 2).await?;
    ensure!(body(&response) == IGNORED, "{response}");
    h.logged("[t4] ignored a prompt from Linear user user-eve")
        .await?;

    let alice = user("user-alice", "alice@example.com");
    ensure!(
        h.linear(from(prompted("t4", &ours, "alice here"), &alice))
            .await?
            == 200
    );
    let response = h.activity("t4", "response", 3).await?;
    ensure!(body(&response) == "Done: alice here", "{response}");

    for refused in ["SH-41", "SH-42", "SH-43", "eve was here"] {
        let prompts = h.prompts_with(refused);
        ensure!(
            prompts.is_empty(),
            "{refused} reached an agent: {prompts:?}"
        );
    }
    let sessions = h.sessions()?;
    ensure!(
        ["t1", "t2", "t3"].iter().all(|sid| sessions[sid].is_null()),
        "{sessions}"
    );
    Ok(())
}

/// A sender the list does not name gets a short answer and no agent, nor the :eyes:; a listed
/// sender, in any letter case, is answered as usual.
async fn zulip_untrusted_sender_is_refused(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        zulip: true,
        env: vec![("ZULIP_TRUSTED_EMAILS", "Ann@Zulip.test".into())],
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let eve = zulip_mention(601, 66, "Eve", "eve@zulip.test", "from eve");
    ensure!(h.zulip(&eve).await? == 200);
    ensure!(zulip_reply(&h, 1).await? == REFUSED);
    h.logged("[zulip:7:deploy] refused a message from Zulip user 66")
        .await?;

    let ann = zulip_mention(602, 12, "Ann", "ann@zulip.test", "from ann");
    ensure!(h.zulip(&ann).await? == 200);
    let reply = zulip_reply(&h, 2).await?;
    ensure!(
        reply == "Done: From Ann (ann@zulip.test):\nfrom ann",
        "{reply}"
    );
    ensure!(h.prompts_with("from eve").is_empty());
    let reactions = h.mock.read(|r| r.reactions.clone());
    ensure!(
        reactions.iter().all(|(_, id, _)| *id == 602),
        "{reactions:?}"
    );
    Ok(())
}

/// Unset, both lists let anyone in: a delegation and a reply by different Linear users, and
/// any Zulip sender, all reach the agent.
async fn without_trusted_lists_anyone_is_heard(ctx: Ctx) -> Result<()> {
    let setup = Setup {
        zulip: true,
        ..Setup::default()
    };
    let h = Harness::start(&ctx, setup).await?;
    let ours = issue(45, "Do it.");
    h.mock.add_issue(&ours);
    let ann = user("user-ann", "ann@example.com");
    let event = by(created("t5", &ours, "context of SH-45"), &ann);
    ensure!(h.linear(event).await? == 200);
    let response = h.activity("t5", "response", 1).await?;
    ensure!(body(&response) == "Done: context of SH-45", "{response}");
    let bob = user("user-bob", "bob@example.com");
    ensure!(
        h.linear(from(prompted("t5", &ours, "bob here"), &bob))
            .await?
            == 200
    );
    let response = h.activity("t5", "response", 2).await?;
    ensure!(body(&response) == "Done: bob here", "{response}");

    let eve = zulip_mention(603, 66, "Eve", "eve@zulip.test", "from eve");
    ensure!(h.zulip(&eve).await? == 200);
    let reply = zulip_reply(&h, 1).await?;
    ensure!(
        reply == "Done: From Eve (eve@zulip.test):\nfrom eve",
        "{reply}"
    );
    Ok(())
}
