//! `mr person` — the people whose machines accounts egress through.
//!
//! A person is a tag plus a proxy. Tagging an account with that tag is the whole configuration:
//! the account's traffic then leaves through that person's proxy, and a combo step can select the
//! account by the same tag. Nothing here stores a credential — the proxy's own secret lives in the
//! vault under the proxy's `secret_ref`, exactly as it does for a proxy configured by hand.

use clap::Subcommand;

use mr_core::config::PersonConfig;

use crate::output::{print_json, say, CliError, Table};
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum PersonCmd {
    /// Declare a person: a tag, and the proxy their accounts should egress through.
    Add(AddArgs),
    /// List the declared people.
    List,
    /// Remove a person. Accounts keep their tag; they simply stop being routed through it.
    Remove {
        /// The person's tag.
        tag: String,
    },
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// The tag that names this person, for example `alice`.
    pub tag: String,
    /// The proxy id their accounts should use, for example a Tailscale peer's SOCKS5 endpoint.
    #[arg(long)]
    pub proxy: String,
    /// A human-readable name, shown instead of the tag where there is room.
    #[arg(long)]
    pub label: Option<String>,
}

/// Run a person subcommand.
pub async fn run(cmd: PersonCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
        PersonCmd::Add(a) => add(ctx, a),
        PersonCmd::List => list(ctx),
        PersonCmd::Remove { tag } => remove(ctx, &tag),
    }
}

fn add(ctx: &Ctx, args: AddArgs) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let tag = args.tag.trim().to_owned();
    if tag.is_empty() {
        return Err(CliError::validation("a person's tag cannot be empty"));
    }
    // The proxy has to exist: a person pointing at a proxy id that is not configured would send
    // their accounts out directly, which is the opposite of what the operator asked for. The
    // request path tolerates it (a missing proxy is a direct connection), but a *new* declaration
    // that cannot work is worth refusing while the operator is still looking at it.
    if cfg.proxy(&args.proxy).is_none() {
        let known: Vec<String> = cfg.proxies.iter().map(|p| p.id.clone()).collect();
        let mut e = CliError::not_found(format!("no proxy with id `{}`", args.proxy));
        e = if known.is_empty() {
            e.with_detail("no proxies are configured; add one with `mr config` or the panel first")
        } else {
            e.with_detail(format!("known proxies: {}", known.join(", ")))
        };
        return Err(e);
    }
    if let Some(existing) = cfg.person_for_tag(&tag) {
        if existing.proxy != args.proxy {
            return Err(CliError::validation(format!(
                "`{tag}` is already a person, using proxy `{}`; nothing was written",
                existing.proxy
            ))
            .with_detail("remove it first with `mr person remove <tag>` to change the proxy"));
        }
        // Nothing to write, so nothing is saved: a no-op must not rewrite the config and bump the
        // routing digest, which would make every relay reload for no reason.
        if ctx.json {
            return print_json(&serde_json::json!({
                "tag": tag,
                "proxy": args.proxy,
                "changed": false,
            }));
        }
        say(format!(
            "`{tag}` already routes through `{}`; nothing changed",
            args.proxy
        ));
        return Ok(());
    }
    let mut person = PersonConfig::new(&tag, &args.proxy);
    person.label = args.label.clone();
    cfg.people.push(person);
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![
            format!("`{tag}` now egresses through proxy `{}`", args.proxy),
            format!("tag an account with `mr account tag <provider> <account> {tag}` to use it"),
        ],
        serde_json::json!({ "tag": tag, "proxy": args.proxy, "changed": true }),
    )
}

fn list(ctx: &Ctx) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    if ctx.json {
        return print_json(&cfg.people);
    }
    if cfg.people.is_empty() {
        say("no people are declared; `mr person add <tag> --proxy <id>` sends that person's accounts through their machine");
        return Ok(());
    }
    let mut t = Table::new(["TAG", "LABEL", "PROXY", "ACCOUNTS"]);
    for person in &cfg.people {
        // Counting the accounts that resolve to this person is the useful number: it is what the
        // operator is about to route, and it is the only way to notice a tag that matches nothing.
        let mut accounts = 0usize;
        for provider in &cfg.providers {
            for account in &provider.accounts {
                if cfg
                    .person_for_account(account)
                    .is_some_and(|p| p.tag.eq_ignore_ascii_case(&person.tag))
                {
                    accounts += 1;
                }
            }
        }
        t.row(vec![
            person.tag.clone(),
            person.label.clone().unwrap_or_default(),
            person.proxy.clone(),
            accounts.to_string(),
        ]);
    }
    t.print();
    Ok(())
}

fn remove(ctx: &Ctx, tag: &str) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let tag = tag.trim();
    let Some(index) = cfg
        .people
        .iter()
        .position(|p| p.tag.eq_ignore_ascii_case(tag))
    else {
        let known: Vec<String> = cfg.people.iter().map(|p| p.tag.clone()).collect();
        let mut e = CliError::not_found(format!("no person tagged `{tag}`"));
        e = if known.is_empty() {
            e.with_detail("no people are declared")
        } else {
            e.with_detail(format!("known people: {}", known.join(", ")))
        };
        return Err(e);
    };
    let removed = cfg.people.remove(index);
    // Accounts keep the tag: it is a note about who the account belongs to, and it stays true after
    // the person stops being routed. Only the egress changes, and it changes to a direct
    // connection, which is what the request path does for a tag that names nobody.
    let mut affected = Vec::new();
    for provider in &cfg.providers {
        for account in &provider.accounts {
            if account.has_tag(&removed.tag) {
                affected.push(format!("{}/{}", provider.id, account.id));
            }
        }
    }
    let out = ctx.save(&cfg)?;
    let mut human = vec![format!(
        "`{}` is no longer a person; their accounts connect directly",
        removed.tag
    )];
    if !affected.is_empty() {
        human.push(format!(
            "{} account(s) keep the tag: {}",
            affected.len(),
            affected.join(", ")
        ));
    }
    ctx.finish(
        &out,
        human,
        serde_json::json!({ "tag": removed.tag, "proxy": removed.proxy, "accounts": affected }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_person_needs_a_tag_and_a_proxy() {
        let person = PersonConfig::new("alice", "alice-laptop");
        assert_eq!(person.tag, "alice");
        assert_eq!(person.proxy, "alice-laptop");
        assert_eq!(person.display(), "alice");
        let mut labelled = person.clone();
        labelled.label = Some("Alice Example".into());
        assert_eq!(labelled.display(), "Alice Example");
    }

    use crate::output::testutil::{block_on, with_config_dir};
    use crate::output::Exit;

    fn proxy(id: &str) -> mr_core::config::ProxyConfig {
        mr_core::config::ProxyConfig {
            id: id.into(),
            label: None,
            url: "socks5h://100.64.0.7:1080".into(),
            username: None,
            secret_ref: None,
            enabled: true,
            priority: 0,
            extra: Default::default(),
        }
    }

    /// A config with one proxy and one account, so a person has something to route.
    fn seed() {
        let mut cfg = mr_core::config::Config::default();
        cfg.proxies.push(proxy("laptop"));
        let mut p = mr_core::model::Provider::new(
            "alpha",
            mr_core::dialect::Dialect::OpenAi,
            "http://127.0.0.1:9/v1",
        );
        let mut a = mr_core::model::Account::new("mine");
        a.id = mr_core::ids::AccountId::from_raw("a1");
        p.accounts = vec![a];
        cfg.providers.push(p);
        cfg.save_default().unwrap();
    }

    fn add(tag: &str, proxy: &str) -> PersonCmd {
        PersonCmd::Add(AddArgs {
            tag: tag.into(),
            proxy: proxy.into(),
            label: None,
        })
    }

    #[test]
    fn a_person_is_added_once_and_removed_cleanly() {
        with_config_dir("person-add", |_| {
            seed();
            let ctx = Ctx { json: false };
            block_on(run(add("alice", "laptop"), &ctx)).unwrap();
            let cfg = mr_core::config::Config::load_default().unwrap();
            assert_eq!(cfg.people.len(), 1);
            assert_eq!(cfg.people[0].tag, "alice");
            assert_eq!(cfg.people[0].proxy, "laptop");

            // Adding the same person again changes nothing, so the routing digest does not move.
            let before = cfg.routing_digest();
            block_on(run(add("alice", "laptop"), &ctx)).unwrap();
            let cfg = mr_core::config::Config::load_default().unwrap();
            assert_eq!(cfg.people.len(), 1);
            assert_eq!(before, cfg.routing_digest());

            block_on(run(
                PersonCmd::Remove {
                    tag: "ALICE".into(),
                },
                &ctx,
            ))
            .unwrap();
            let cfg = mr_core::config::Config::load_default().unwrap();
            assert!(cfg.people.is_empty());
        });
    }

    #[test]
    fn a_person_cannot_name_a_proxy_that_does_not_exist() {
        with_config_dir("person-unknown-proxy", |_| {
            seed();
            let ctx = Ctx { json: false };
            let err = block_on(run(add("alice", "nowhere"), &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::NotFound, "{err}");
            let cfg = mr_core::config::Config::load_default().unwrap();
            assert!(cfg.people.is_empty());
        });
    }

    /// Changing a person's proxy is a remove-then-add, so the command refuses rather than silently
    /// moving every account tagged for them.
    #[test]
    fn a_person_cannot_be_moved_to_another_proxy_in_place() {
        with_config_dir("person-move", |_| {
            seed();
            let mut cfg = mr_core::config::Config::load_default().unwrap();
            cfg.proxies.push(proxy("office"));
            cfg.save_default().unwrap();
            let ctx = Ctx { json: false };
            block_on(run(add("alice", "laptop"), &ctx)).unwrap();
            let err = block_on(run(add("alice", "office"), &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::Validation, "{err}");
            let cfg = mr_core::config::Config::load_default().unwrap();
            assert_eq!(cfg.people[0].proxy, "laptop");
        });
    }

    #[test]
    fn removing_an_unknown_person_is_a_not_found() {
        with_config_dir("person-remove-missing", |_| {
            seed();
            let ctx = Ctx { json: false };
            let err = block_on(run(PersonCmd::Remove { tag: "bob".into() }, &ctx)).unwrap_err();
            assert_eq!(err.exit, Exit::NotFound, "{err}");
        });
    }

    /// Removing a person leaves the account tags alone: they are still true statements about who the
    /// account belongs to, and re-adding the person restores the egress.
    #[test]
    fn removing_a_person_keeps_the_account_tags() {
        with_config_dir("person-remove-keeps-tags", |_| {
            seed();
            let mut cfg = mr_core::config::Config::load_default().unwrap();
            cfg.providers[0].accounts[0].tags.push("alice".into());
            cfg.save_default().unwrap();
            let ctx = Ctx { json: false };
            block_on(run(add("alice", "laptop"), &ctx)).unwrap();
            block_on(run(
                PersonCmd::Remove {
                    tag: "alice".into(),
                },
                &ctx,
            ))
            .unwrap();
            let cfg = mr_core::config::Config::load_default().unwrap();
            assert!(cfg.people.is_empty());
            assert_eq!(cfg.providers[0].accounts[0].tags, ["alice"]);
        });
    }
}
